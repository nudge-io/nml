use std::collections::{HashMap, HashSet};

use nml_core::ast::*;
use nml_core::model::{EnumDef, FieldDef, FieldType, ModelDef, OneOfDef};
use nml_core::schema::{ExtractedSchema, report_graph_cycles};
use nml_core::schema_index::{BodyShape, FieldTarget, SchemaIndex};
use nml_core::span::Span;
use nml_core::types::{PrimitiveType, Value};

use nml_core::diagnostic::{Diagnostic, codes};

const MAX_VALIDATION_DEPTH: u32 = 64;

/// Diagnostic for a scalar shorthand item on a union-typed list — out of scope
/// (RFC 0005 §10), flagged here in both the top-level and nested list paths.
const UNION_SHORTHAND_MSG: &str =
    "shorthand is not supported on union-typed lists; specify the variant explicitly";

/// Validates instance declarations against model definitions.
///
/// In default mode, unknown properties are reported as warnings and blocks
/// with no matching model are silently skipped.  Call [`Self::strict`] to
/// promote unknown-property diagnostics to errors and to detect blocks /
/// arrays whose keyword has no model definition.
///
/// By default, the validator is **domain-neutral**: no modifiers, membership
/// keywords, or built-in references are assumed.  Embedders opt in to
/// domain-specific checks via builder methods.
#[derive(Debug)]
/// Schema names a single checked file declares, for the validator's in-file
/// composition checks (RFC 0011): `composables` (models/traits) resolve as
/// `is` targets; `wrong_kind` (enums/oneofs) classify as non-composable.
/// Instance *typing* still comes exclusively from the loaded schema set.
struct FileLocalSchema<'a> {
    composables: HashSet<&'a str>,
    wrong_kind: HashMap<&'a str, &'static str>,
}

#[derive(Debug)]
pub struct SchemaValidator {
    index: SchemaIndex,
    valid_modifiers: Vec<String>,
    strict_unknown_fields: bool,
    membership: MembershipSemantics,
    /// When set, in-file `is`-composition checks are skipped: the caller
    /// routes definitions through the loader pipeline
    /// (`find_composition_errors`), which sees the whole file and reports
    /// once. The editor keeps them on — its validators run without a
    /// loader pass over the open document.
    composition_checked_at_load: bool,
    /// Closed vocabulary (RFC 0012): this validator's schema set is the
    /// *entire* authority (a package binding). In-file schema definitions
    /// have no effect and draw NML2026 — a tenant file can neither shadow
    /// an operator schema nor mint keywords past strict's unknown-keyword
    /// wall. Set only by [`crate::package::SchemaPackage::validator`]
    /// (authority follows provenance; there is no user-facing knob).
    closed_vocabulary: bool,
}

/// Opt-in configuration for embedders that model membership / access-control
/// relationships (e.g. RBAC roles, ACL groups).  When all fields are at
/// defaults (empty / `None`), the validator performs purely structural checks.
#[derive(Debug, Clone, Default)]
pub struct MembershipSemantics {
    /// Block keywords whose bodies contain membership references and should
    /// participate in cycle detection (e.g. `["role", "plan"]`).
    pub member_keywords: Vec<String>,
    /// Reference values that are reserved built-ins and should NOT appear
    /// inside member lists (e.g. `["@public", "@authenticated"]`).
    pub builtin_refs: Vec<String>,
    /// Prefix for references that target individual principals.  Warned about
    /// when it appears inside access-control modifier rules (e.g. `"@user/"`).
    pub user_ref_prefix: Option<String>,
}

impl From<ExtractedSchema> for SchemaValidator {
    /// Build a validator from a loaded schema (use after running the
    /// inheritance/cycle passes, e.g. via [`crate::loader::load_schema`]).
    fn from(schema: ExtractedSchema) -> Self {
        Self::new(schema.models, schema.enums, schema.oneofs)
    }
}

/// How an element's diagnostics name their location: the declaring field
/// (or block keyword) and the container word the INLINE spelling uses.
/// Threading it is what makes spelling parity cover message PROSE — a
/// `set<string>` element must read "in set 'tags'" in both spellings, and
/// no diagnostic may ever put a TYPE where a field name belongs.
#[derive(Clone, Copy)]
struct ElemLabel<'a> {
    field: &'a str,
    container: &'a str,
}

impl<'a> ElemLabel<'a> {
    /// A list/array element of `field`.
    fn array(field: &'a str) -> Self {
        Self {
            field,
            container: "in array",
        }
    }

    /// The same field, relabelled for a `set<T>` container.
    fn in_set(self) -> Self {
        Self {
            container: "in set",
            ..self
        }
    }

    /// Label an element of `field` whose declared container is `ty` — the
    /// ONE place the container word is derived from a type, so no call site
    /// can mislabel a `set<T>` as an array.
    fn for_type(field: &'a str, ty: &FieldType) -> Self {
        let base = Self::array(field);
        if matches!(ty, FieldType::Set(_)) {
            base.in_set()
        } else {
            base
        }
    }
}

impl SchemaValidator {
    pub fn new(models: Vec<ModelDef>, enums: Vec<EnumDef>, oneofs: Vec<OneOfDef>) -> Self {
        Self {
            index: SchemaIndex::build(models, enums, oneofs),
            valid_modifiers: Vec::new(),
            strict_unknown_fields: false,
            membership: MembershipSemantics::default(),
            composition_checked_at_load: false,
            closed_vocabulary: false,
        }
    }

    /// The schema index backing this validator, for callers that need the shared
    /// lookup / dispatch primitive (e.g. the defaulting pass).
    pub fn index(&self) -> &SchemaIndex {
        &self.index
    }

    /// Promote unknown-property diagnostics to errors and reject blocks /
    /// arrays whose keyword has no matching model definition.
    pub fn strict(mut self) -> Self {
        self.strict_unknown_fields = true;
        self
    }

    /// Declare that definition composition (`is` targets, RFC 0011) is
    /// validated by a loader pass over the same content, so the in-file twin
    /// stays silent instead of double-reporting. Instance validation and
    /// field-default checks are unaffected.
    pub fn composition_checked_at_load(mut self) -> Self {
        self.composition_checked_at_load = true;
        self
    }

    /// Mark this validator's schema set as a closed vocabulary (RFC 0012) —
    /// see the field docs. Called by the package layer only.
    pub fn closed_vocabulary(mut self) -> Self {
        self.closed_vocabulary = true;
        self
    }

    /// The NML2026 refusal for a schema definition authored in a file
    /// governed by a closed binding: warning in lenient mode (the definition
    /// is inert, the file still passes), error under strict (CI posture).
    fn ineffective_definition(&self, kind: &str, span: Span) -> Diagnostic {
        let message = format!(
            "in-file {kind} definitions have no effect under this schema package \
             binding — the package's schemas are the vocabulary"
        );
        let diag = if self.strict_unknown_fields {
            Diagnostic::error(message)
        } else {
            Diagnostic::warning(message)
        };
        diag.with_code(codes::INEFFECTIVE_DEFINITIONS)
            .with_span(span)
    }

    /// Set valid modifier names. When non-empty, unknown modifiers produce
    /// warnings. When empty (the default), all modifier names are accepted.
    pub fn with_modifiers(mut self, modifiers: Vec<String>) -> Self {
        self.valid_modifiers = modifiers;
        self
    }

    /// Configure membership / access-control semantics.  When set, the
    /// validator checks for cycles among `member_keywords`, warns about
    /// `builtin_refs` in member lists, and warns about `user_ref_prefix`
    /// references inside modifier rules.
    pub fn with_membership_semantics(mut self, membership: MembershipSemantics) -> Self {
        self.membership = membership;
        self
    }

    /// Candidate names for an unknown-keyword suggestion: every declared
    /// *instantiable* model and `oneof` — the two targets a block or array
    /// keyword can resolve to. Traits are excluded (RFC 0011): suggesting
    /// one would teach the NML2024 error.
    fn keyword_candidates(&self) -> impl Iterator<Item = &str> + '_ {
        self.index
            .models()
            .iter()
            .filter(|m| !m.is_trait())
            .map(|m| m.name.as_str())
            .chain(self.index.oneofs().iter().map(|o| o.name.as_str()))
    }

    /// The trait-instantiation gate (RFC 0011): if `keyword` names a trait,
    /// push NML2024 and report `true`. An error even in lenient mode — the
    /// schema declared the name, so a block using it is a mistake, never
    /// another tool's vocabulary (the same reasoning as NML2003).
    fn check_trait_instantiation(
        &self,
        keyword: &str,
        span: Span,
        diags: &mut Vec<Diagnostic>,
    ) -> bool {
        let Some(model) = self.find_model(keyword) else {
            return false;
        };
        if !model.is_trait() {
            return false;
        }
        diags.push(
            Diagnostic::error(format!(
                "trait '{keyword}' cannot be instantiated — traits only compose into \
                 models with `is {keyword}`"
            ))
            .with_code(codes::TRAIT_INSTANTIATED)
            .with_span(span),
        );
        true
    }

    /// An "unknown property" diagnostic (warning by default, error under
    /// [`Self::strict`]) with a near-miss suggestion against the model's
    /// declared fields when one is close enough. The suggestion span is the
    /// property-name token, so the quick-fix is machine-applicable.
    fn unknown_property_diagnostic(&self, name: &str, model: &ModelDef, span: Span) -> Diagnostic {
        self.unknown_name_diagnostic("property", name, model, span)
    }

    /// The unknown-name treatment shared by properties and shared
    /// properties: warning lenient / error strict, NML2001, did-you-mean
    /// over the model's fields, machine-applicable at the name token.
    fn unknown_name_diagnostic(
        &self,
        noun: &str,
        name: &str,
        model: &ModelDef,
        span: Span,
    ) -> Diagnostic {
        let message = format!(
            "unknown {noun} '{name}' (not defined in model '{}')",
            model.name
        );
        let diag = if self.strict_unknown_fields {
            Diagnostic::error(message)
        } else {
            Diagnostic::warning(message)
        }
        .with_code(codes::UNKNOWN_PROPERTY)
        .with_span(span);
        match nml_core::suggest::suggest(name, model.fields.iter().map(|f| f.name.as_str())) {
            Some(s) => diag.with_suggestion(s, span),
            None => diag,
        }
    }

    /// Collect a nested list body's `.prop` entries and validate them
    /// against the element type (resolved body-independently — shared
    /// properties precede any single item's shape).
    fn validate_body_shared_properties(
        &self,
        body: &Body,
        inner: &FieldType,
        depth: u32,
        diags: &mut Vec<Diagnostic>,
    ) {
        let shared: Vec<&SharedProperty> = body
            .entries
            .iter()
            .filter_map(|e| match &e.kind {
                BodyEntryKind::SharedProperty(sp) => Some(sp),
                _ => None,
            })
            .collect();
        if shared.is_empty() {
            return;
        }
        // A union element type gets subset semantics over its model variants
        // (the `oneof` rule generalized): resolving against an empty body
        // would arbitrarily pick the first variant and flag names a later
        // variant legitimately defines.
        if let FieldType::Union(variants) = inner {
            let models: Vec<&ModelDef> = variants
                .iter()
                .filter_map(|v| match v {
                    FieldType::ModelRef(name) => self.find_model(name).filter(|m| !m.is_trait()),
                    _ => None,
                })
                .collect();
            if !models.is_empty() {
                self.validate_shared_properties_union(&shared, &models, depth, diags);
                return;
            }
        }
        let empty = Body::fresh(Vec::new());
        let elem = self.index.resolve_type_in_body(inner, &empty);
        self.validate_shared_properties(&shared, &elem, depth, diags);
    }

    /// Union-element subset semantics for shared properties: a name is known
    /// if ANY model variant defines it, and its value must satisfy at least
    /// one defining variant's field type — checked through the standard
    /// union value check, so the mismatch message and code are the ones
    /// every union value gets. Block-valued shared properties follow the same
    /// subset rule: the block's content must be a valid instance for at least
    /// one declaring variant's field model (the validator never merges, so this
    /// is the only pre-merge coverage the content gets — RFC 0015 made
    /// same-class unions the newly reachable case).
    fn validate_shared_properties_union(
        &self,
        shared: &[&SharedProperty],
        models: &[&ModelDef],
        depth: u32,
        diags: &mut Vec<Diagnostic>,
    ) {
        for sp in shared {
            let name = sp.name.name.as_str();
            let defining: Vec<&FieldDef> = models
                .iter()
                .filter_map(|m| m.fields.iter().find(|f| f.name == name))
                .collect();
            if defining.is_empty() {
                let listed = models
                    .iter()
                    .map(|m| format!("'{}'", m.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                let message = format!(
                    "unknown shared property '.{name}' — no union variant ({listed}) defines it"
                );
                let mut diag = if self.strict_unknown_fields {
                    Diagnostic::error(message)
                } else {
                    Diagnostic::warning(message)
                }
                .with_code(codes::UNKNOWN_PROPERTY)
                .with_span(sp.name.span);
                let mut candidates: Vec<&str> = models
                    .iter()
                    .flat_map(|m| m.fields.iter().map(|f| f.name.as_str()))
                    .collect();
                candidates.sort_unstable();
                candidates.dedup();
                if let Some(s) = nml_core::suggest::suggest(name, candidates) {
                    diag = diag.with_suggestion(s, sp.name.span);
                }
                diags.push(diag);
                continue;
            }
            match &sp.kind {
                SharedPropertyKind::Scalar(sv) => {
                    if let [only] = defining.as_slice() {
                        self.validate_value_against_type(
                            &sv.value,
                            &only.field_type,
                            name,
                            "for shared property",
                            sv.span,
                            diags,
                        );
                    } else {
                        let union = FieldType::Union(
                            defining.iter().map(|f| f.field_type.clone()).collect(),
                        );
                        self.validate_value_against_type(
                            &sv.value,
                            &union,
                            name,
                            "for shared property",
                            sv.span,
                            diags,
                        );
                    }
                }
                SharedPropertyKind::Block(body) => {
                    // Subset semantics, block form: the content must be a valid
                    // instance for AT LEAST ONE variant declaring the field as
                    // something block-capable (mirroring the scalar rule).
                    // Declarers resolve through the canonical body-aware
                    // resolver, so a union-typed declarer selects its variant by
                    // the block's shape. A clean pass on any declarer accepts;
                    // findings from the first block-capable declarer otherwise;
                    // and if NO declarer can hold a block at all, that is a
                    // type mismatch — never a silent accept.
                    let mut first: Option<Vec<Diagnostic>> = None;
                    let mut ok = false;
                    let mut block_capable = false;
                    for field in &defining {
                        // Block-capable = anything a block can legally fill: what
                        // `validate_target_instance` validates (model, oneof,
                        // list/set per-item, arms) PLUS declared-opaque `object`
                        // (free-form by definition — content accepted without a
                        // schema walk, exactly like the non-union path). NOT
                        // Model-only, which false-errored on `[]sub`, oneof, and
                        // object declarers and on a union's list variant
                        // legitimately selected by an item-shaped block.
                        let target = self.index.resolve_type_in_body(&field.field_type, body);
                        if matches!(target, FieldTarget::Object) {
                            ok = true;
                            block_capable = true;
                            break;
                        }
                        let mut local = Vec::new();
                        if self.validate_target_instance(
                            &target,
                            body,
                            depth + 1,
                            Some(sp.name.span),
                            ElemLabel::array(&sp.name.name),
                            &mut local,
                        ) {
                            block_capable = true;
                            if local.is_empty() {
                                ok = true;
                                break;
                            }
                            if first.is_none() {
                                first = Some(local);
                            }
                        }
                    }
                    if !block_capable {
                        diags.push(
                            Diagnostic::error(format!(
                                "type mismatch for shared property '.{name}': no union \
                                 variant declares '{name}' as a block-capable type"
                            ))
                            .with_code(codes::TYPE_MISMATCH)
                            .with_span(sp.name.span),
                        );
                    } else if !ok {
                        if let Some(local) = first {
                            diags.extend(local);
                        }
                    }
                }
            }
        }
    }

    /// Validate shared properties against their list's element type,
    /// anchored at the `.prop` token itself (pre-merge — the merged copies
    /// inside items are covered by per-item validation). Unknown names get
    /// the unknown-property treatment (NML2001: warning lenient, error
    /// strict, did-you-mean); known scalar values type-check against the
    /// field. For a `oneof` element, a name is flagged only when the
    /// discriminator and every variant lack it — it may legitimately serve
    /// a subset of variants.
    fn validate_shared_properties(
        &self,
        shared: &[&SharedProperty],
        elem: &FieldTarget<'_>,
        depth: u32,
        diags: &mut Vec<Diagnostic>,
    ) {
        for sp in shared {
            let name = sp.name.name.as_str();
            match elem {
                FieldTarget::Model(model) => {
                    let Some(field) = model.fields.iter().find(|f| f.name == name) else {
                        diags.push(self.unknown_name_diagnostic(
                            "shared property",
                            &format!(".{name}"),
                            model,
                            sp.name.span,
                        ));
                        continue;
                    };
                    match &sp.kind {
                        SharedPropertyKind::Scalar(sv) => {
                            self.validate_value_against_type(
                                &sv.value,
                                &field.field_type,
                                name,
                                "for shared property",
                                sv.span,
                                diags,
                            );
                        }
                        SharedPropertyKind::Block(body) => {
                            if let FieldTarget::Model(inner) = self.index.resolve_field(field) {
                                self.validate_instance_against_model(
                                    body,
                                    inner,
                                    depth + 1,
                                    Some(sp.name.span),
                                    diags,
                                );
                            }
                        }
                    }
                }
                FieldTarget::OneOf(oneof) => {
                    let known = name == oneof.discriminator
                        || oneof.variants.iter().any(|(_, variant)| {
                            self.find_model(variant)
                                .is_some_and(|m| m.fields.iter().any(|f| f.name == name))
                        });
                    if !known {
                        let message = format!(
                            "unknown shared property '.{name}' — neither the discriminator \
                             nor any variant of oneof '{}' defines it",
                            oneof.name
                        );
                        let diag = if self.strict_unknown_fields {
                            Diagnostic::error(message)
                        } else {
                            Diagnostic::warning(message)
                        }
                        .with_code(codes::UNKNOWN_PROPERTY)
                        .with_span(sp.name.span);
                        diags.push(diag);
                    }
                }
                // Unions/lists-of resolve per item; free-form targets have
                // no fields to check against.
                _ => {}
            }
        }
    }

    pub fn find_model(&self, name: &str) -> Option<&ModelDef> {
        self.index.model(name)
    }

    pub fn find_enum(&self, name: &str) -> Option<&EnumDef> {
        self.index.enum_def(name)
    }

    pub fn find_oneof(&self, name: &str) -> Option<&OneOfDef> {
        self.index.oneof(name)
    }

    /// Validate the file's **definition-side** facts: the same body pass
    /// `check` runs over `model`/`trait`/`enum` declarations (field
    /// defaults vs declared types, RFC 0007 §4.3 type-shape rules,
    /// misplaced arms/field definitions, modifier declarations) — one code
    /// path, so a new definition-side check can never split the verbs
    /// again. Instance typing stays exclusively in [`Self::validate`].
    pub fn validate_definitions(&self, file: &File) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for decl in &file.declarations {
            if let DeclarationKind::Block(block) = &decl.kind {
                let keyword = block.keyword.name.as_str();
                if matches!(keyword, "model" | "trait" | "enum") {
                    self.validate_body(&block.body, true, keyword, &mut diagnostics);
                    if matches!(keyword, "model" | "trait") {
                        self.validate_field_defaults(block, &mut diagnostics);
                    }
                }
            }
        }
        diagnostics
    }

    /// Validate a parsed NML file against the loaded models.
    pub fn validate(&self, file: &File) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Schema names the *file itself* declares, split by composability.
        // In-file schema definitions are validated when a schema set is
        // loaded, not here — so in-file checks (the `is`-target twin below)
        // must resolve a self-contained file's own trait/model instead of
        // flagging it against a foreign schema set, and must classify a
        // file-local enum/oneof target as the wrong *kind*, not unknown.
        let mut local_composables: HashSet<&str> = HashSet::new();
        let mut local_wrong_kind: HashMap<&str, &'static str> = HashMap::new();
        for decl in &file.declarations {
            match &decl.kind {
                DeclarationKind::Block(b) => match b.keyword.name.as_str() {
                    "model" | "trait" => {
                        local_composables.insert(b.name.name.as_str());
                    }
                    "enum" => {
                        local_wrong_kind.insert(b.name.name.as_str(), "an enum");
                    }
                    _ => {}
                },
                DeclarationKind::OneOf(o) => {
                    local_wrong_kind.insert(o.name.name.as_str(), "a oneof");
                }
                _ => {}
            }
        }
        let file_locals = FileLocalSchema {
            composables: local_composables,
            wrong_kind: local_wrong_kind,
        };

        for decl in &file.declarations {
            match &decl.kind {
                DeclarationKind::Block(block) => {
                    self.validate_block(block, &file_locals, &mut diagnostics);
                }
                DeclarationKind::Array(arr) => {
                    self.validate_array(arr, &mut diagnostics);
                }
                // `oneof` declarations are schema definitions, validated when
                // the schema is loaded; they carry no instance data here —
                // but under a closed binding they are inert and say so.
                DeclarationKind::OneOf(o) => {
                    if self.closed_vocabulary {
                        diagnostics.push(self.ineffective_definition("oneof", o.name.span));
                    }
                }
                DeclarationKind::Const(_) | DeclarationKind::Template(_) => {}
            }
        }

        self.validate_member_cycles(file, &mut diagnostics);

        diagnostics
    }

    fn validate_block(
        &self,
        block: &BlockDecl,
        file_locals: &FileLocalSchema<'_>,
        diags: &mut Vec<Diagnostic>,
    ) {
        let keyword = &block.keyword.name;
        let is_schema_def = matches!(keyword.as_str(), "model" | "enum" | "trait");

        if is_schema_def && self.closed_vocabulary {
            diags.push(self.ineffective_definition(keyword, block.keyword.span));
        }

        if matches!(keyword.as_str(), "model" | "trait") {
            // The in-file twin of `nml_core::schema::find_composition_errors`
            // (RFC 0011): schema declarations authored in a *checked* file get
            // the same `is`-target diagnostics, with the same code and a
            // machine-applicable did-you-mean at the target token. Targets the
            // file itself declares resolve — a self-contained file is never
            // flagged against a foreign schema set. Skipped entirely when the
            // caller runs the loader pipeline over this content
            // (`composition_checked_at_load`) — one finding, one owner.
            let parents = if self.composition_checked_at_load {
                [].iter()
            } else {
                block.extends.iter()
            };
            for parent in parents {
                if self.find_model(&parent.name).is_some()
                    || file_locals.composables.contains(parent.name.as_str())
                {
                    continue;
                }
                let wrong_kind = if self.find_enum(&parent.name).is_some() {
                    Some("an enum")
                } else if self.find_oneof(&parent.name).is_some() {
                    Some("a oneof")
                } else {
                    file_locals.wrong_kind.get(parent.name.as_str()).copied()
                };
                diags.push(match wrong_kind {
                    Some(kind_name) => Diagnostic::error(format!(
                        "`is` target '{}' is {} — only models and traits compose",
                        parent.name, kind_name,
                    ))
                    .with_code(codes::INVALID_MIXIN_KIND)
                    .with_span(parent.span),
                    None => {
                        let mut diag = Diagnostic::error(format!(
                            "unknown `is` target '{}' — no model or trait with that name",
                            parent.name,
                        ))
                        .with_code(codes::UNKNOWN_MIXIN)
                        .with_span(parent.span);
                        if let Some(s) = nml_core::suggest::suggest(
                            &parent.name,
                            self.index
                                .models()
                                .iter()
                                .map(|m| m.name.as_str())
                                .chain(file_locals.composables.iter().copied()),
                        ) {
                            diag = diag.with_suggestion(s, parent.span);
                        }
                        diag
                    }
                });
            }
            self.validate_field_defaults(block, diags);
        }

        self.validate_body(&block.body, is_schema_def, keyword, diags);
        self.validate_members_builtin_refs(&block.body, keyword, diags);

        if !is_schema_def {
            // Traits are never block types (RFC 0011); the gate also stops the
            // body from being validated against the trait's fields below.
            if self.check_trait_instantiation(keyword, block.keyword.span, diags) {
                return;
            }
            // A block declaration (`role editor:`) fills its model's `name` field from
            // the block name — lenient: an explicit `name` in the body wins (RFC 0005
            // §5). `oneof`/other targets keep the prior path.
            let resolved = match self.index.resolve_ref(keyword) {
                Some(FieldTarget::Model(m)) => {
                    let result = nml_core::identity::materialize_named(&block.name, &block.body, m);
                    self.validate_materialized(result, m, 0, Some(block.name.span), diags);
                    true
                }
                Some(other) => self.validate_target_instance(
                    &other,
                    &block.body,
                    0,
                    Some(block.name.span),
                    ElemLabel::array(keyword),
                    diags,
                ),
                // A label-only keyword (no model/oneof): nothing to validate
                // against; the strict-keyword check reports unknowns.
                None => false,
            };
            if !resolved && self.strict_unknown_fields {
                let mut diag =
                    Diagnostic::error(format!("block keyword '{keyword}' has no model definition"))
                        .with_code(codes::UNKNOWN_BLOCK_KEYWORD)
                        .with_span(block.keyword.span);
                if let Some(s) = nml_core::suggest::suggest(keyword, self.keyword_candidates()) {
                    diag = diag.with_suggestion(s, block.keyword.span);
                }
                diags.push(diag);
            }
        }
    }

    fn validate_array(&self, arr: &ArrayDecl, diags: &mut Vec<Diagnostic>) {
        // Array-level modifiers cover the items, so the *element* model's
        // modifier fields are the vocabulary.
        let elem_governing = self
            .find_model(&arr.item_keyword.name)
            .filter(|m| !m.is_trait());
        for modifier in &arr.body.modifiers {
            self.validate_modifier_name(modifier, elem_governing, diags);
            self.validate_modifier_content(modifier, diags);
        }

        let keyword = &arr.item_keyword.name;
        let is_schema_def = matches!(keyword.as_str(), "model" | "enum" | "trait");
        // Traits are never element types either (RFC 0011): report once at
        // the keyword and skip item validation — the declaration is already
        // in error, and its items have no model to validate against.
        if !is_schema_def && self.check_trait_instantiation(keyword, arr.item_keyword.span, diags) {
            return;
        }
        // An array item keyword may name a model or a `oneof`, mirroring the
        // block-keyword dispatch in `validate_block` — resolved once and reused
        // both for the strict check and to validate each item below.
        let elem = self.index.resolve_ref(keyword);
        let shared: Vec<&SharedProperty> = arr.body.shared_properties.iter().collect();
        if let Some(elem) = &elem {
            self.validate_shared_properties(&shared, elem, 0, diags);
        }
        let resolves =
            !is_schema_def && matches!(elem, Some(FieldTarget::Model(_) | FieldTarget::OneOf(_)));

        // Only *named* items carry a body that needs a model/oneof to validate against;
        // a scalar item is a bare value (e.g. `[]plugin globalPlugins:` of plugin-name
        // strings), valid under a keyword that is just a label, not a model. So only
        // named items trigger the strict "no definition" check.
        let has_named_items = arr
            .body
            .items
            .iter()
            .any(|i| matches!(&i.kind, ListItemKind::Named { .. }));

        if !is_schema_def && !resolves && has_named_items && self.strict_unknown_fields {
            let mut diag = Diagnostic::error(format!(
                "array item keyword '{keyword}' has no model or oneof definition"
            ))
            .with_code(codes::UNKNOWN_ARRAY_KEYWORD)
            .with_span(arr.item_keyword.span);
            if let Some(s) = nml_core::suggest::suggest(keyword, self.keyword_candidates()) {
                diag = diag.with_suggestion(s, arr.item_keyword.span);
            }
            diags.push(diag);
        }

        for item in &arr.body.items {
            // A named item's body — or a scalar shorthand's optional `: body` — gets
            // the same body-level checks (field-def placement, builtin member refs);
            // references/links carry none. Inline items (named or scalar) are then
            // validated against the element target after identity materialization
            // (RFC 0005 §10); a bare scalar `- "/api"` fills the model's `+` field.
            let item_body = match &item.kind {
                ListItemKind::Named { body, .. } => Some(body),
                ListItemKind::Shorthand { body, .. } => body.as_ref(),
                ListItemKind::Reference(_) | ListItemKind::Role(_) => None,
            };
            if let Some(body) = item_body {
                self.validate_body(body, is_schema_def, keyword, diags);
                self.validate_members_builtin_refs(body, keyword, diags);
                // A top-level array element is a single keyword, never a union,
                // so an `as <Variant>` annotation on an item is always stray.
                if !is_schema_def {
                    self.flag_stray_annotation(body, diags);
                }
            }
            // Item-shape decisions live in the dispatcher's matrix; the gate
            // here is only "is there a target at all" (unknown keywords were
            // reported above) and the schema-def exemption.
            if !is_schema_def {
                if let Some(elem) = &elem {
                    self.validate_inline_item(item, elem, ElemLabel::array(keyword), 0, diags);
                }
            }
        }
    }

    /// Validate one inline list item against its already-resolved element target,
    /// **materializing the item's identity** into the body first (RFC 0005 §10): a
    /// named item's `name`, or a bare scalar's shorthand field, becomes a present
    /// property so the required-field scan sees it. A scalar on a union list is out of
    /// scope and flagged explicitly. This is the single inline-item path shared by
    /// top-level arrays, the `[]T` field arm, and the `ListOf` dispatch — references
    /// and links carry no inline instance and are skipped.
    /// The item-shape × element-target decision matrix — TOTAL, with no
    /// catch-all at either level: every (target, item shape) combination is
    /// an explicit decision, so adding a variant to either enum forces a
    /// compile-time choice here instead of a silent skip. (Three silent-arm
    /// bugs preceded this design: the lowering fallthrough that emptied
    /// secret items, the `Leaf` erasure that skipped dash-item typing, and
    /// the body-drop this matrix's NML2055 arm now reports.)
    ///
    /// `label` names the element's location exactly as the INLINE spelling
    /// names it (declaring field + container word), so both spellings
    /// produce identical message PROSE, not merely identical codes.
    fn validate_inline_item(
        &self,
        item: &ListItem,
        elem: &FieldTarget,
        label: ElemLabel<'_>,
        depth: u32,
        diags: &mut Vec<Diagnostic>,
    ) {
        use ListItemKind as K;
        let header = match &item.kind {
            K::Named { name, .. } => Some(name.span),
            K::Shorthand { value, .. } => Some(value.span),
            K::Reference(id) => Some(id.span),
            K::Role(_) => Some(item.span),
        };
        // Tier-1 shared scalar check: THE same total checker the inline-array
        // spelling uses, so the two spellings agree by construction —
        // diagnostics, did-you-mean suggestions, fallback-leg recursion, all
        // of it (the spelling-parity property test pins this).
        let check_value = |value: &Value,
                           span: Span,
                           ty: &FieldType,
                           diags: &mut Vec<Diagnostic>| {
            self.validate_value_against_type(value, ty, label.field, label.container, span, diags);
        };
        // Tier-2: content with nowhere to go is an error, never a silent
        // drop — NML2055, the body-side mirror of NML2049's dropped key.
        let dropped_body =
            |body: &Body, ty: &FieldType, span: Span, diags: &mut Vec<Diagnostic>| {
                if !body.entries.is_empty() {
                    diags.push(
                        Diagnostic::error(format!(
                            "this item's body has nowhere to go: the element type \
                             `{ty}` has no fields to fill"
                        ))
                        .with_code(codes::DROPPED_ITEM_BODY)
                        .with_span(span),
                    );
                }
            };
        match elem {
            // Inline items (named or scalar) validate against the model after
            // identity materialization (a named item's `name` / a scalar's
            // `+` field); reference/role items are links — resolved by the
            // consumer, never validated as inline instances.
            FieldTarget::Model(m) => match &item.kind {
                K::Named { .. } | K::Shorthand { .. } => {
                    let result = nml_core::identity::materialize_item(item, m);
                    self.validate_materialized(result, m, depth, header, diags);
                }
                K::Reference(_) | K::Role(_) => {}
            },
            FieldTarget::OneOf(_) => match &item.kind {
                K::Named { body, .. } => {
                    self.validate_target_instance(elem, body, depth, header, label, diags);
                }
                // A scalar can only fill a model's `+` field; on a union its
                // variant isn't yet known, so it is out of scope — flagged.
                K::Shorthand { value, .. } => diags.push(
                    Diagnostic::error(UNION_SHORTHAND_MSG.to_string())
                        .with_code(codes::UNION_SHORTHAND)
                        .with_span(value.span),
                ),
                K::Reference(_) | K::Role(_) => {}
            },
            // Leaf/Union: value items type-check; bodies have nowhere to go.
            FieldTarget::Leaf(ty) | FieldTarget::Union(ty) => match &item.kind {
                K::Shorthand { value, body } => {
                    check_value(&value.value, value.span, ty, diags);
                    if let Some(b) = body {
                        dropped_body(b, ty, value.span, diags);
                    }
                }
                K::Named { name, body } => dropped_body(body, ty, name.span, diags),
                K::Role(r) => {
                    check_value(&Value::Role(r.clone()), item.span, ty, diags);
                }
                // References resolve later (a name may become the element's
                // value downstream) — accepted anywhere, like `$ENV` refs.
                K::Reference(_) => {}
            },
            // Nested collections. A NAMED item's body here is a GROUP whose
            // entries are the nested list (the workflow grouped-thread
            // pattern, `- Group:` under `[]step`) — recurse, don't reject.
            // A scalar item checks against the declared list/set type (an
            // array value recurses element-wise inside the checker; a bare
            // scalar gets the type mismatch).
            FieldTarget::ListOf(ty, _) | FieldTarget::SetOf(ty, _) => match &item.kind {
                K::Shorthand { value, body } => {
                    check_value(&value.value, value.span, ty, diags);
                    if let Some(b) = body {
                        self.validate_target_instance(elem, b, depth, header, label, diags);
                    }
                }
                K::Named { body, .. } => {
                    self.validate_target_instance(elem, body, depth, header, label, diags);
                }
                K::Role(r) => check_value(&Value::Role(r.clone()), item.span, ty, diags),
                K::Reference(_) => {}
            },
            // Free-form: accepts every shape by definition.
            FieldTarget::Object => match &item.kind {
                K::Named { .. } | K::Shorthand { .. } | K::Reference(_) | K::Role(_) => {}
            },
            // Arm-set bodies hold arms, not list items; a list item here is
            // body-shape territory (validate_instance_against_arms reports
            // non-arm entries) — deliberate skip, not a silent one.
            FieldTarget::Arms { .. } => match &item.kind {
                K::Named { .. } | K::Shorthand { .. } | K::Reference(_) | K::Role(_) => {}
            },
        }
    }

    /// Surface a materialization's findings (coded at their source —
    /// `NML2049` dropped key, `NML2050` arm-shorthand mismatch) and validate
    /// the enriched body against `model` — unless the item is unplaceable, in
    /// which case the required-field scan is skipped so it doesn't pile noise
    /// on the materialization finding. The single "materialize → validate"
    /// path shared by list items (`materialize_item`) and block declarations
    /// (`materialize_named`).
    fn validate_materialized(
        &self,
        result: nml_core::identity::Materialized,
        model: &ModelDef,
        depth: u32,
        header: Option<Span>,
        diags: &mut Vec<Diagnostic>,
    ) {
        diags.extend(result.diagnostics);
        if result.validatable {
            self.validate_instance_against_model(&result.body, model, depth, header, diags);
        }
    }

    /// Type-check each declared default against its field's type, reusing the
    /// exact check applied to instance values so default-checking and
    /// value-checking can never diverge. Only this model's own declared fields
    /// are checked (inherited fields are checked on their defining model), so a
    /// default is never reported twice.
    fn validate_field_defaults(&self, block: &BlockDecl, diags: &mut Vec<Diagnostic>) {
        let Some(model) = self.find_model(&block.name.name) else {
            return;
        };
        for entry in &block.body.entries {
            let BodyEntryKind::FieldDefinition(fd) = &entry.kind else {
                continue;
            };
            let Some(default) = &fd.default_value else {
                continue;
            };
            let Some(field) = model.fields.iter().find(|f| f.name == fd.name.name) else {
                continue;
            };
            self.validate_value_against_type(
                &default.value,
                &field.field_type,
                &field.name,
                "as the default for",
                default.span,
                diags,
            );
        }
    }

    fn validate_body(
        &self,
        body: &Body,
        is_schema_def: bool,
        keyword: &str,
        diags: &mut Vec<Diagnostic>,
    ) {
        // The governing model (concrete only) supplies the modifier
        // vocabulary when it declares modifier fields.
        let governing = self.find_model(keyword).filter(|m| !m.is_trait());
        for entry in &body.entries {
            match &entry.kind {
                BodyEntryKind::Modifier(m) => {
                    self.validate_modifier_name(m, governing, diags);
                    self.validate_modifier_content(m, diags);
                    // RFC 0007 §4.3: a modifier's instance value is an inline
                    // value or a list block — an arm body can never appear
                    // under a modifier, so an arm set ANYWHERE in a modifier's
                    // declared type has no instance form.
                    if let ModifierValue::TypeAnnotation { field_type, .. } = &m.value {
                        field_type_shape_errors(
                            field_type,
                            Some("a modifier's declared type"),
                            entry.span,
                            diags,
                        );
                    }
                }
                BodyEntryKind::FieldDefinition(_) if !is_schema_def => {
                    diags.push(
                        Diagnostic::error(format!(
                            "field definitions are only allowed in model declarations, not '{keyword}'"
                        ))
                        .with_code(codes::MISPLACED_FIELD_DEFINITION)
                        .with_span(entry.span),
                    );
                }
                // RFC 0007 §4.3 arm-set shape rules: the grammar is
                // deliberately permissive about type composition, so the
                // schema layer rejects the shapes that have no instance form.
                BodyEntryKind::FieldDefinition(f) => {
                    field_type_shape_errors(&f.field_type, None, entry.span, diags);
                }
                // RFC 0007 §4.2: a schema declaration declares an arm set via
                // the field *type* '(K -> V)'; arm entries belong in instances.
                BodyEntryKind::Arm(_) if is_schema_def => {
                    diags.push(
                        Diagnostic::error(format!(
                            "routing arms are not allowed in '{keyword}' declarations; declare \
                             the field as '(K -> V)' and write the arms in the instance block"
                        ))
                        .with_code(codes::ARMS_IN_DEFINITION)
                        .with_span(entry.span),
                    );
                }
                BodyEntryKind::NestedBlock(nb) => {
                    self.validate_body(&nb.body, is_schema_def, keyword, diags);
                }
                _ => {}
            }
        }
    }

    fn validate_modifier_name(
        &self,
        m: &Modifier,
        governing: Option<&ModelDef>,
        diags: &mut Vec<Diagnostic>,
    ) {
        // A model that declares modifier *fields* (`|allow []role?`) is the
        // vocabulary for its blocks — per-block precision the global list
        // can't give, and it types the values too. The manifest/project
        // list stays the fallback for blocks whose model declares none.
        if let Some(model) = governing {
            let declared: Vec<&str> = model
                .fields
                .iter()
                .filter(|f| matches!(f.field_type, FieldType::Modifier(_)))
                .map(|f| f.name.as_str())
                .collect();
            if !declared.is_empty() {
                if !declared.contains(&m.name.name.as_str()) {
                    let listed = declared
                        .iter()
                        .map(|d| format!("|{d}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let message = format!(
                        "unknown modifier '|{}' — model '{}' declares: {listed}",
                        m.name.name, model.name
                    );
                    let mut diag = if self.strict_unknown_fields {
                        Diagnostic::error(message)
                    } else {
                        Diagnostic::warning(message)
                    }
                    .with_code(codes::UNKNOWN_MODIFIER)
                    .with_span(m.name.span);
                    if let Some(sugg) =
                        nml_core::suggest::suggest(&m.name.name, declared.iter().copied())
                    {
                        diag = diag.with_suggestion(sugg, m.name.span);
                    }
                    diags.push(diag);
                }
                return;
            }
        }
        if self.valid_modifiers.is_empty() {
            return;
        }
        if !self.valid_modifiers.iter().any(|v| v == &m.name.name) {
            let mut diag = Diagnostic::warning(format!(
                "unknown modifier '|{}'; expected one of: {}",
                m.name.name,
                self.valid_modifiers
                    .iter()
                    .map(|s| format!("|{s}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .with_code(codes::UNKNOWN_MODIFIER)
            .with_span(m.name.span);
            if let Some(s) = nml_core::suggest::suggest(
                &m.name.name,
                self.valid_modifiers.iter().map(String::as_str),
            ) {
                diag = diag.with_suggestion(s, m.name.span);
            }
            diags.push(diag);
        }
    }

    /// Validate an instance body against a named type reference — a model or a
    /// `oneof` — via the shared name→target dispatch. Enum and unknown refs carry
    /// no instance structure to validate. This is the single place the validator
    /// turns a `someModel` reference into a nested validation, sharing
    /// [`SchemaIndex::resolve_ref`] with the defaulting pass so the dispatch has
    /// one definition.
    /// Returns whether the reference resolved to a model or `oneof` (callers at
    /// keyword level use this to emit a strict "no definition" diagnostic).
    fn validate_ref_instance(
        &self,
        ref_name: &str,
        body: &Body,
        depth: u32,
        header_span: Option<Span>,
        diags: &mut Vec<Diagnostic>,
    ) -> bool {
        let Some(target) = self.index.resolve_ref(ref_name) else {
            // Unknown/enum name: a leaf reference with no instance shape to
            // validate; reference-existence is its own check elsewhere.
            return false;
        };
        self.validate_target_instance(
            &target,
            body,
            depth,
            header_span,
            ElemLabel::array(ref_name),
            diags,
        )
    }

    /// Validate `body` against an already-resolved [`FieldTarget`]. The single
    /// dispatch on a resolved target, shared by keyword/ref dispatch
    /// ([`Self::validate_ref_instance`]) and union variant selection (via
    /// [`SchemaIndex::resolve_type_in_body`]). A `ListOf` target validates each
    /// inline item (named or scalar) against the element target via
    /// [`Self::validate_inline_item`]. Returns whether the target carried
    /// instance structure (model / oneof / list of those).
    /// RFC 0015 nominal-union enforcement — the one gate shared by the field and
    /// list-element union sites, so both behave identically:
    ///
    /// * an `as <Variant>` annotation that names no variant is an error with a
    ///   machine-applicable did-you-mean (drawn from the union's nameable
    ///   variants — the same candidate set the LSP completes);
    /// * a same-class instance with **no** annotation whose body shape cannot
    ///   choose between ≥2 model variants is the D2 hard error — fail-closed, so
    ///   an ambiguous instance is unrepresentable rather than silently guessed.
    ///
    /// Disjoint unions are untouched: a single nameable variant, or a body that
    /// structurally selects a list/arm-set variant, is never ambiguous.
    /// Returns whether instance validation should proceed. `false` means a
    /// union-level error (unknown variant / shape mismatch / D2 ambiguity) was
    /// already reported — validating the body against a *guessed* variant would
    /// only pile spurious unknown-property/missing-field noise on top of the
    /// real finding (the same no-noise rule `materialize_item` applies via
    /// `validatable`).
    fn check_union_annotation(
        &self,
        variants: &[FieldType],
        body: &Body,
        union_span: Span,
        fix_anchor: Option<(&str, Span)>,
        diags: &mut Vec<Diagnostic>,
    ) -> bool {
        if let Some(ann) = &body.type_annotation {
            if self
                .index
                .select_variant_by_type_name(variants, &ann.name)
                .is_none()
            {
                let nameable = self.index.nameable_variant_names(variants);
                let mut diag =
                    Diagnostic::error(format!("`{}` is not a variant of this union", ann.name))
                        .with_code(codes::UNKNOWN_UNION_VARIANT)
                        .with_span(ann.span);
                if let Some(s) = nml_core::suggest::suggest(&ann.name, nameable.iter().copied()) {
                    diag = diag.with_suggestion(s.to_string(), ann.span);
                }
                diags.push(diag);
                return false;
            }
            return true;
        }
        // No annotation → the body's shape must select a variant.
        let shape = BodyShape::of(body);
        // A list/arm shape only *disambiguates* if the union actually has that
        // variant. If it does not, the shape matches NO variant: it resolves to
        // `Leaf` and would be validated as nothing (silently dropped). Flag it as
        // a shape mismatch — the same fail-loud stance D2 takes for the keyed
        // case, closing the model-only-union hole.
        let has_arms_variant = variants.iter().any(|v| matches!(v, FieldType::Arms { .. }));
        let has_list_variant = variants.iter().any(|v| matches!(v, FieldType::List(_)));
        if (shape.has_arms && !has_arms_variant) || (shape.has_list_items && !has_list_variant) {
            let shape_name = if shape.has_arms {
                "routing arms"
            } else {
                "a list"
            };
            let nameable = self.index.nameable_variant_names(variants);
            let expected = if nameable.is_empty() {
                String::new()
            } else {
                format!("; expected a block form of one of: {}", nameable.join(", "))
            };
            diags.push(
                Diagnostic::error(format!(
                    "{shape_name} is not a valid instance of this union{expected}"
                ))
                .with_code(codes::UNION_TYPE_MISMATCH)
                .with_span(union_span),
            );
            return false;
        }
        // D2, via the shared AMBIGUITY ORACLE — the same rule the LSP's
        // union-of-fields completion consumes, so editor and validator can
        // never disagree about what is ambiguous.
        if let Some(candidates) = self.index.ambiguous_union_variants(variants, body) {
            let names: Vec<&str> = candidates.iter().map(|c| c.name()).collect();
            let mut diag = Diagnostic::error(if fix_anchor.is_some() {
                format!(
                    "ambiguous union instance: shape cannot choose between {}; \
                     add an explicit type with `as <variant>`",
                    names.join(" | ")
                )
            } else {
                // Reference / scalar-shorthand / role items cannot carry `as`
                // in place — the fix is the block form.
                format!(
                    "ambiguous union instance: shape cannot choose between {}; \
                     write the item in block form `- <name> as <variant>:`",
                    names.join(" | ")
                )
            })
            .with_code(codes::AMBIGUOUS_UNION_INSTANCE)
            .with_span(union_span);
            // One mutually exclusive Fix per candidate, ONLY where the
            // annotation is grammatical and meaning-preserving (a field header
            // or a Named item — an anchored name token to extend). Capped: an
            // adversarial 1000-variant union must not mint 1000 actions.
            const MAX_FIX_ALTERNATIVES: usize = 8;
            if let Some((anchor_name, anchor_span)) = fix_anchor {
                for c in candidates.iter().take(MAX_FIX_ALTERNATIVES) {
                    diag = diag.with_fix(format!("{anchor_name} as {}", c.name()), anchor_span);
                }
            }
            diags.push(diag);
            return false;
        }
        true
    }

    /// Resolve a list element's type against its body, first running the RFC
    /// 0038 union gate when the element type is a union — so every `[](A | B)`
    /// element enforces `as`/D2 identically to a field-level union, from one
    /// place.
    fn resolve_elem_checked<'a>(
        &'a self,
        inner: &'a FieldType,
        item: &ListItem,
        probe: &Body,
        diags: &mut Vec<Diagnostic>,
    ) -> Option<FieldTarget<'a>> {
        if let FieldType::Union(variants) = inner {
            // A pure VALUE item (bare scalar / role) carries no body, so
            // body-based variant selection is meaningless — it checks
            // against the whole union, exactly like an inline array element
            // (the spelling-parity invariant). References are NOT values
            // here: they can name a union variant instance, so they keep
            // the RFC 0015 machinery (D2 ambiguity with anchored fixes).
            if matches!(
                &item.kind,
                ListItemKind::Shorthand { body: None, .. } | ListItemKind::Role(_)
            ) {
                return Some(FieldTarget::Union(inner));
            }
            // A Named item anchors the diagnostic AND the fix at its NAME token
            // (`- one` of `- one as modelB:`); reference/shorthand/role items
            // cannot carry `as` in place, so they get no fix anchor and the
            // message steers to the block form.
            let anchor = match &item.kind {
                ListItemKind::Named { name, .. } => Some((name.name.as_str(), name.span)),
                _ => None,
            };
            let span = anchor.map(|(_, s)| s).unwrap_or(item.span);
            if !self.check_union_annotation(variants, probe, span, anchor, diags) {
                // A union-level error was reported; skip instance validation
                // quietly instead of validating a guessed variant.
                return None;
            }
        } else {
            self.flag_stray_annotation(probe, diags);
        }
        Some(self.index.resolve_type_in_body(inner, probe))
    }

    /// RFC 0015: flag an `as <Variant>` annotation on a non-union field or
    /// element — there is no variant to select, so it has no effect. The
    /// complement of [`Self::check_union_annotation`]: together they make every
    /// annotated body either meaningful (union) or flagged (non-union), never
    /// silently ignored — the same single-source discipline everywhere.
    fn flag_stray_annotation(&self, body: &Body, diags: &mut Vec<Diagnostic>) {
        if let Some(ann) = &body.type_annotation {
            diags.push(
                Diagnostic::error(format!(
                    "`as {}` is only valid on a union-typed field; there is no variant to select here",
                    ann.name
                ))
                .with_code(codes::STRAY_TYPE_ANNOTATION)
                .with_span(ann.span),
            );
        }
    }

    fn validate_target_instance(
        &self,
        target: &FieldTarget,
        body: &Body,
        depth: u32,
        header_span: Option<Span>,
        label: ElemLabel<'_>,
        diags: &mut Vec<Diagnostic>,
    ) -> bool {
        match target {
            FieldTarget::Model(m) => {
                self.validate_instance_against_model(body, m, depth, header_span, diags);
                true
            }
            FieldTarget::OneOf(o) => {
                self.validate_instance_against_oneof(body, o, depth, header_span, diags);
                true
            }
            FieldTarget::ListOf(_, inner) => {
                for entry in &body.entries {
                    if let BodyEntryKind::ListItem(item) = &entry.kind {
                        self.validate_inline_item(item, inner.as_ref(), label, depth, diags);
                    }
                }
                true
            }
            FieldTarget::SetOf(_, inner) => {
                // Shape: exactly a list. Then RFC 0032 uniqueness — duplicate
                // elements are load errors, reported at the SECOND occurrence.
                // Identity is value-level for scalar items (semantic_eq, span-
                // blind) and name-level for named/reference items.
                let mut items: Vec<&nml_core::ast::ListItem> = Vec::new();
                for entry in &body.entries {
                    if let BodyEntryKind::ListItem(item) = &entry.kind {
                        // The inline spelling says "in set" here; the dash
                        // spelling must say it too (prose parity).
                        self.validate_inline_item(
                            item,
                            inner.as_ref(),
                            label.in_set(),
                            depth,
                            diags,
                        );
                        items.push(item);
                    }
                }
                push_duplicate_set_items(&items, diags);
                true
            }
            FieldTarget::Arms { key, target } => {
                self.validate_instance_against_arms(body, key, target, diags);
                true
            }
            FieldTarget::Object | FieldTarget::Union(_) | FieldTarget::Leaf(_) => false,
        }
    }

    /// Validate an arm-set instance (`(K -> V)`, RFC 0007 §4.2–§4.3): every
    /// entry must be an arm; keys must conform to `K` (`else` is always
    /// legal); `else` is single and last (first-match ordering makes a
    /// non-last `else` dead code); exact-duplicate keys error. **Reference
    /// targets are deliberately not existence-checked** (§4.1): consumer
    /// resolution is cross-scope (e.g. an app-level arm targeting a
    /// deployment-level declaration), so an in-file check would false-positive
    /// on legitimate cross-file references — the target type drives editor
    /// intelligence and the consumer's own load-time resolution instead.
    fn validate_instance_against_arms(
        &self,
        body: &Body,
        key: &FieldType,
        target: &FieldType,
        diags: &mut Vec<Diagnostic>,
    ) {
        let mut else_seen = false;
        let mut keys_seen: Vec<&str> = Vec::new();
        for entry in &body.entries {
            let BodyEntryKind::Arm(arm) = &entry.kind else {
                diags.push(
                    Diagnostic::error(format!(
                        "expected a routing arm ('@selector -> Target' or 'else -> Target'); \
                         this field is typed '({key} -> …)' and holds only arms"
                    ))
                    .with_code(codes::ARMS_BODY_ENTRY)
                    .with_span(entry.span),
                );
                continue;
            };
            self.validate_arm_target(&arm.target, target, diags);
            match &arm.selector {
                ArmSelector::Else => {
                    if else_seen {
                        diags.push(
                            Diagnostic::error(
                                "duplicate 'else' arm; an arm set has at most one catch-all"
                                    .to_string(),
                            )
                            .with_code(codes::DUPLICATE_ARM)
                            .with_span(arm.selector_span),
                        );
                    }
                    else_seen = true;
                }
                ArmSelector::Role(selector) => {
                    if else_seen {
                        diags.push(
                            Diagnostic::error(format!(
                                "arm '{selector}' is unreachable: arms match first-to-last, \
                                 so 'else' must be the final arm"
                            ))
                            .with_code(codes::UNREACHABLE_ARM)
                            .with_span(arm.selector_span),
                        );
                    }
                    if !matches!(key, FieldType::Primitive(PrimitiveType::Role)) {
                        diags.push(
                            Diagnostic::error(format!(
                                "arm key '{selector}' does not conform to the declared key \
                                 type '{key}'"
                            ))
                            .with_code(codes::ARM_KEY_MISMATCH)
                            .with_span(arm.selector_span),
                        );
                    }
                    if keys_seen.contains(&selector.as_str()) {
                        diags.push(
                            Diagnostic::error(format!("duplicate arm key '{selector}'"))
                                .with_code(codes::DUPLICATE_ARM)
                                .with_span(arm.selector_span),
                        );
                    }
                    keys_seen.push(selector);
                }
            }
        }
    }

    /// Validate one arm target against the arm set's `V` (RFC 0007 §6):
    /// - a **reference** (`-> Name`) is never existence-checked (§4.1,
    ///   consumer-resolved cross-scope) — its form is legal for any `V`;
    /// - a **literal** (`-> "path/url"`) requires a *scalar-capable* `V` (you
    ///   cannot write a string where a model instance is expected), and its
    ///   string value is checked against a concrete primitive/enum `V`.
    fn validate_arm_target(
        &self,
        arm_target: &ArmTarget,
        v: &FieldType,
        diags: &mut Vec<Diagnostic>,
    ) {
        let ArmTarget::Literal { value, span } = arm_target else {
            return; // a reference is shape-legal for any V
        };
        if self.field_type_admits_a_literal(v) {
            // Concrete scalar/enum V → type-check the literal string value.
            self.validate_value_against_type(
                &Value::String(value.clone()),
                v,
                "arm target",
                "for",
                *span,
                diags,
            );
        } else {
            diags.push(
                Diagnostic::error(format!(
                    "a string-literal arm target requires a scalar target type, but this arm \
                     set targets '{v}'; use a declared name ('-> {v}Name') instead"
                ))
                .with_code(codes::ARM_TARGET_MISMATCH)
                .with_span(*span),
            );
        }
    }

    /// Whether a string-literal arm target is admissible for `V` — true for a
    /// primitive, an enum reference, an unknown name (consumer-resolved leaf),
    /// or a union with any such variant; false for a model/`oneof`/list/arms
    /// `V` (a literal can't stand in for a declared instance).
    fn field_type_admits_a_literal(&self, v: &FieldType) -> bool {
        match v {
            FieldType::Primitive(_) => true,
            FieldType::Modifier(inner) => self.field_type_admits_a_literal(inner),
            FieldType::Union(variants) => {
                variants.iter().any(|t| self.field_type_admits_a_literal(t))
            }
            FieldType::ModelRef(name) => {
                self.find_model(name).is_none() && self.find_oneof(name).is_none()
            }
            FieldType::List(_) | FieldType::Set(_) | FieldType::Arms { .. } => false,
        }
    }

    /// `header_span` points at the instance's block-header / item name and is
    /// preferred for diagnostics that concern the instance as a whole (e.g.
    /// missing required fields).
    fn validate_instance_against_model(
        &self,
        body: &Body,
        model: &ModelDef,
        depth: u32,
        header_span: Option<Span>,
        diags: &mut Vec<Diagnostic>,
    ) {
        if depth >= MAX_VALIDATION_DEPTH {
            diags.push(truncation_advisory(body, header_span));
            return;
        }

        let mut seen_fields: Vec<&str> = Vec::new();

        for entry in &body.entries {
            match &entry.kind {
                BodyEntryKind::Property(prop) => {
                    let name = &prop.name.name;
                    seen_fields.push(name);

                    if let Some(field_def) = model.fields.iter().find(|f| f.name == *name) {
                        self.validate_value_against_type(
                            &prop.value.value,
                            &field_def.field_type,
                            &field_def.name,
                            "for",
                            prop.value.span,
                            diags,
                        );
                    } else {
                        diags.push(self.unknown_property_diagnostic(name, model, prop.name.span));
                    }
                }
                BodyEntryKind::NestedBlock(nb) => {
                    seen_fields.push(&nb.name.name);

                    if let Some(field_def) = model.fields.iter().find(|f| f.name == nb.name.name) {
                        // RFC 0015: a union field — plain or MODIFIER-WRAPPED
                        // (`|slot (a | b)`) — takes one gated path: annotation/D2
                        // enforcement, then validation against the resolved
                        // variant (`resolve_type_in_body` unwraps the modifier
                        // the same way). Dispatching via `union_variants` keeps a
                        // modifier-wrapped union from silently skipping the gate
                        // the way the raw `match` did. Everything else flags a
                        // stray annotation and dispatches as before.
                        if let Some(variants) = field_def.field_type.union_variants() {
                            if self.check_union_annotation(
                                variants,
                                &nb.body,
                                nb.name.span,
                                Some((nb.name.name.as_str(), nb.name.span)),
                                diags,
                            ) {
                                let target = self
                                    .index
                                    .resolve_type_in_body(&field_def.field_type, &nb.body);
                                self.validate_target_instance(
                                    &target,
                                    &nb.body,
                                    depth + 1,
                                    Some(nb.name.span),
                                    ElemLabel::for_type(&field_def.name, &field_def.field_type),
                                    diags,
                                );
                            }
                            continue;
                        }
                        self.flag_stray_annotation(&nb.body, diags);
                        match &field_def.field_type {
                            FieldType::ModelRef(ref_name) => {
                                self.validate_ref_instance(
                                    ref_name,
                                    &nb.body,
                                    depth + 1,
                                    Some(nb.name.span),
                                    diags,
                                );
                            }
                            FieldType::List(inner) => {
                                // Each item resolves its inner type against its own
                                // body (so a `(a | b)` union variant is picked per
                                // item; a `ModelRef` inner resolves body-independently),
                                // then the shared inline-item path materializes the
                                // item's identity into the body before validating —
                                // so a required `name` supplied by the item key
                                // (`- classify:`) is seen, not reported missing.
                                let empty = Body::fresh(Vec::new());
                                self.validate_body_shared_properties(&nb.body, inner, depth, diags);
                                for entry in &nb.body.entries {
                                    let BodyEntryKind::ListItem(item) = &entry.kind else {
                                        continue;
                                    };
                                    let probe = match &item.kind {
                                        ListItemKind::Named { body, .. } => body,
                                        ListItemKind::Shorthand { body: Some(b), .. } => b,
                                        _ => &empty,
                                    };
                                    if let Some(elem) =
                                        self.resolve_elem_checked(inner, item, probe, diags)
                                    {
                                        self.validate_inline_item(
                                            item,
                                            &elem,
                                            ElemLabel::for_type(
                                                &field_def.name,
                                                &field_def.field_type,
                                            ),
                                            depth + 1,
                                            diags,
                                        );
                                    }
                                }
                            }
                            FieldType::Set(inner) => {
                                // Items validate exactly like a list's (same
                                // per-item variant resolution + identity
                                // materialization as the `List` arm above)…
                                let empty = Body::fresh(Vec::new());
                                self.validate_body_shared_properties(&nb.body, inner, depth, diags);
                                let mut items: Vec<&ListItem> = Vec::new();
                                for entry in &nb.body.entries {
                                    let BodyEntryKind::ListItem(item) = &entry.kind else {
                                        continue;
                                    };
                                    let probe = match &item.kind {
                                        ListItemKind::Named { body, .. } => body,
                                        ListItemKind::Shorthand { body: Some(b), .. } => b,
                                        _ => &empty,
                                    };
                                    if let Some(elem) =
                                        self.resolve_elem_checked(inner, item, probe, diags)
                                    {
                                        self.validate_inline_item(
                                            item,
                                            &elem,
                                            ElemLabel::for_type(
                                                &field_def.name,
                                                &field_def.field_type,
                                            ),
                                            depth + 1,
                                            diags,
                                        );
                                    }
                                    items.push(item);
                                }
                                // …then RFC 0032 uniqueness: duplicates are
                                // load errors at the second occurrence, with
                                // span-blind value identity for scalar items
                                // and name identity for named items.
                                push_duplicate_set_items(&items, diags);
                            }
                            FieldType::Arms { key, target } => {
                                self.validate_instance_against_arms(&nb.body, key, target, diags);
                            }
                            // Union fields — plain or modifier-wrapped — were
                            // dispatched by `union_variants` above, before this
                            // match.
                            FieldType::Primitive(PrimitiveType::Object) => {}
                            _ => {}
                        }
                    } else {
                        diags.push(self.unknown_property_diagnostic(
                            &nb.name.name,
                            model,
                            nb.name.span,
                        ));
                    }
                }
                BodyEntryKind::Modifier(m) => {
                    seen_fields.push(&m.name.name);

                    if let Some(field_def) = model.fields.iter().find(|f| f.name == m.name.name) {
                        self.validate_modifier_value(m, field_def, diags);
                    }
                }
                // RFC 0007 §4.2 placement rule: arms are legal only inside a
                // field typed as an arm set '(K -> V)'. A bare arm in a
                // model-typed block would otherwise parse and silently do
                // nothing — a latent trap.
                BodyEntryKind::Arm(_) => {
                    diags.push(
                        Diagnostic::error(format!(
                            "routing arms are not allowed here: '{}' holds fields, not arms; \
                             arms belong under a field typed '(K -> V)'",
                            model.name
                        ))
                        .with_code(codes::ARMS_NOT_EXPECTED)
                        .with_span(entry.span),
                    );
                }
                // A bare list item in a model body fills the model's
                // body-positional list field (RFC 0005 `+` on a list/set —
                // e.g. `plugins []tenantGrantPlugin+`): validate it deeply
                // against the declared element type, exactly as a named
                // `field:` list would be. A model with NO such field keeps the
                // historical lenient skip (additive rule — no new rejections).
                BodyEntryKind::ListItem(item) => {
                    if let Some(field_def) = model.fields.iter().find(|f| {
                        f.shorthand
                            && matches!(f.field_type, FieldType::List(_) | FieldType::Set(_))
                    }) {
                        seen_fields.push(&field_def.name);
                        let inner = match &field_def.field_type {
                            FieldType::List(i) | FieldType::Set(i) => i,
                            _ => unreachable!("guarded by the find predicate"),
                        };
                        let empty = Body::fresh(Vec::new());
                        let probe = match &item.kind {
                            ListItemKind::Named { body, .. } => body,
                            ListItemKind::Shorthand { body: Some(b), .. } => b,
                            _ => &empty,
                        };
                        if let Some(elem) = self.resolve_elem_checked(inner, item, probe, diags) {
                            self.validate_inline_item(
                                item,
                                &elem,
                                ElemLabel::for_type(&field_def.name, &field_def.field_type),
                                depth + 1,
                                diags,
                            );
                        }
                    }
                }
                _ => {}
            }
        }

        for field in &model.fields {
            if !field.optional
                && field.default_value.is_none()
                && !seen_fields.contains(&field.name.as_str())
            {
                diags.push(
                    Diagnostic::error(format!(
                        "missing required field '{}' (defined in model '{}')",
                        field.name, model.name
                    ))
                    .with_code(codes::MISSING_REQUIRED_FIELD)
                    .with_span(
                        header_span
                            .or_else(|| body.entries.first().map(|e| e.span))
                            .unwrap_or(field.span),
                    ),
                );
            }
        }
    }

    /// Validate an instance block against a `oneof`: resolve the discriminator
    /// value to a variant model, then validate the remaining fields against
    /// that variant (per-variant required/unknown-field enforcement).
    ///
    /// The discriminator field belongs to the union, not the variant model, so
    /// it is excluded before the variant check (mirroring how serde's
    /// internally-tagged enums consume the tag field).
    fn validate_instance_against_oneof(
        &self,
        body: &Body,
        oneof: &OneOfDef,
        depth: u32,
        header_span: Option<Span>,
        diags: &mut Vec<Diagnostic>,
    ) {
        if depth >= MAX_VALIDATION_DEPTH {
            diags.push(truncation_advisory(body, header_span));
            return;
        }

        let valid_values = || {
            oneof
                .variants
                .iter()
                .map(|(v, _)| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let fallback_span = || {
            header_span
                .or_else(|| body.entries.first().map(|e| e.span))
                .unwrap_or(oneof.span)
        };

        // Locate the discriminator property within the block.
        let discriminator = body.entries.iter().find_map(|entry| match &entry.kind {
            BodyEntryKind::Property(prop) if prop.name.name == oneof.discriminator => Some(prop),
            _ => None,
        });

        let Some(discriminator) = discriminator else {
            // An omitted discriminator is valid when the union declares a default —
            // the defaulting pass injects it. Validate the body against the default
            // variant so validation agrees with defaulting. (The default is
            // guaranteed to name an arm by `find_oneof_errors`.)
            if let Some(default) = &oneof.default_discriminator {
                if let Some((_, model_name)) = oneof.variants.iter().find(|(v, _)| v == default) {
                    if let Some(variant_model) = self.find_model(model_name) {
                        self.validate_instance_against_model(
                            body,
                            variant_model,
                            depth,
                            header_span,
                            diags,
                        );
                    }
                }
                return;
            }
            diags.push(
                Diagnostic::error(format!(
                    "missing discriminator '{disc}' for oneof '{name}'; set `{disc} = <one of: {values}>`",
                    disc = oneof.discriminator,
                    name = oneof.name,
                    values = valid_values(),
                ))
                .with_code(codes::MISSING_DISCRIMINATOR)
                .with_span(fallback_span()),
            );
            return;
        };

        let Value::String(value) = &discriminator.value.value else {
            diags.push(
                Diagnostic::error(format!(
                    "discriminator '{}' for oneof '{}' must be a string (one of: {})",
                    oneof.discriminator,
                    oneof.name,
                    valid_values(),
                ))
                .with_code(codes::INVALID_DISCRIMINATOR)
                .with_span(discriminator.value.span),
            );
            return;
        };

        let Some((_, model_name)) = oneof.variants.iter().find(|(v, _)| v == value) else {
            let mut diag = Diagnostic::error(format!(
                "unknown {} \"{}\" for oneof '{}'; expected one of: {}",
                oneof.discriminator,
                value,
                oneof.name,
                valid_values(),
            ))
            .with_code(codes::UNKNOWN_DISCRIMINANT)
            .with_span(discriminator.value.span);
            if let Some(v) =
                nml_core::suggest::suggest(value, oneof.variants.iter().map(|(k, _)| k.as_str()))
            {
                // The discriminator is a string literal (guarded above); the
                // fix replaces its content, not its quotes.
                diag = diag.with_suggestion(v, string_content_span(discriminator.value.span));
            }
            diags.push(diag);
            return;
        };

        // The variant model is guaranteed to exist (checked at schema-load
        // time by `find_oneof_errors`); skip silently if the schema was built
        // without that check.
        let Some(variant_model) = self.find_model(model_name) else {
            return;
        };

        // Validate everything except the discriminator against the variant.
        // Derived from `body`, so rebuild via `with_entries` (the RFC 0015
        // convention): the annotation is preserved for any annotation-sensitive
        // logic below oneof validation, never silently stripped.
        let variant_body = body.with_entries(
            body.entries
                .iter()
                .filter(|entry| {
                    !matches!(
                        &entry.kind,
                        BodyEntryKind::Property(prop) if prop.name.name == oneof.discriminator
                    )
                })
                .cloned()
                .collect(),
        );
        self.validate_instance_against_model(
            &variant_body,
            variant_model,
            depth,
            header_span,
            diags,
        );
    }

    /// Validate a modifier's value against the type declared in the model
    /// (e.g. `|allow []string?`).
    fn validate_modifier_value(&self, m: &Modifier, field: &FieldDef, diags: &mut Vec<Diagnostic>) {
        let FieldType::Modifier(declared) = &field.field_type else {
            return;
        };

        match &m.value {
            ModifierValue::Inline(sv) => {
                self.validate_value_against_type(
                    &sv.value,
                    declared,
                    &field.name,
                    "for",
                    sv.span,
                    diags,
                );
            }
            ModifierValue::Block(items) => {
                // A block-form modifier list satisfies a List OR a Set
                // declaration (RFC 0032 — e.g. `|block set<string>?` written
                // as `|block:` + items); sets additionally enforce uniqueness.
                let (inner, is_set) = match declared.as_ref() {
                    FieldType::List(inner) => (inner, false),
                    FieldType::Set(inner) => (inner, true),
                    _ => {
                        diags.push(
                            Diagnostic::error(format!(
                                "type mismatch for '{}': expected {}, got array",
                                field.name, declared
                            ))
                            .with_code(codes::TYPE_MISMATCH)
                            .with_span(m.name.span),
                        );
                        return;
                    }
                };
                for item in items {
                    match &item.kind {
                        ListItemKind::Shorthand { value: sv, .. } => {
                            self.validate_value_against_type(
                                &sv.value,
                                inner,
                                &field.name,
                                "in array",
                                sv.span,
                                diags,
                            );
                        }
                        ListItemKind::Role(role_ref) => {
                            self.validate_value_against_type(
                                &Value::Role(role_ref.clone()),
                                inner,
                                &field.name,
                                "in array",
                                item.span,
                                diags,
                            );
                        }
                        // RFC 0015: the annotation rules apply in a modifier
                        // block list like any other list — gate a union element
                        // type (D2 / unknown-variant), flag a stray annotation
                        // otherwise — never silently carry one. Body-content
                        // validation of modifier items stays as-is (pre-existing
                        // leniency, out of this RFC's scope).
                        ListItemKind::Named { name, body } => {
                            if let Some(variants) = inner.union_variants() {
                                self.check_union_annotation(
                                    variants,
                                    body,
                                    name.span,
                                    Some((name.name.as_str(), name.span)),
                                    diags,
                                );
                            } else {
                                self.flag_stray_annotation(body, diags);
                            }
                        }
                        ListItemKind::Reference(_) => {}
                    }
                }
                if is_set {
                    // RFC 0032 uniqueness — same rule and reporting as every
                    // other set surface: error at the second occurrence,
                    // span-blind value identity.
                    push_duplicate_set_items(&items.iter().collect::<Vec<_>>(), diags);
                }
            }
            ModifierValue::TypeAnnotation { .. } => {}
        }
    }

    fn validate_value_against_type(
        &self,
        value: &Value,
        field_type: &FieldType,
        field_name: &str,
        context: &str,
        span: Span,
        diags: &mut Vec<Diagnostic>,
    ) {
        if let Value::Fallback(primary, fallback) = value {
            self.validate_value_against_type(
                &primary.value,
                field_type,
                field_name,
                context,
                primary.span,
                diags,
            );
            self.validate_value_against_type(
                &fallback.value,
                field_type,
                field_name,
                context,
                fallback.span,
                diags,
            );
            return;
        }

        match field_type {
            FieldType::Primitive(prim) => {
                self.validate_primitive_value(value, prim, field_name, context, span, diags);
            }
            FieldType::ModelRef(ref_name) => {
                if let Some(enum_def) = self.find_enum(ref_name) {
                    self.validate_enum_value(value, enum_def, field_name, span, diags);
                } else {
                    self.validate_model_ref_value(value, ref_name, field_name, span, diags);
                }
            }
            FieldType::List(inner) => match value {
                Value::Array(items) => {
                    for item in items {
                        self.validate_value_against_type(
                            &item.value,
                            inner,
                            field_name,
                            "in array",
                            item.span,
                            diags,
                        );
                    }
                }
                // References (e.g. to consts) and env vars may resolve to arrays.
                Value::Reference(_) | Value::Secret(_) => {}
                _ => {
                    diags.push(
                        Diagnostic::error(format!(
                            "type mismatch {context} '{field_name}': expected {field_type}, got {}",
                            value_type_name(value)
                        ))
                        .with_code(codes::TYPE_MISMATCH)
                        .with_span(span),
                    );
                }
            },
            FieldType::Set(inner) => match value {
                Value::Array(items) => {
                    for item in items {
                        self.validate_value_against_type(
                            &item.value,
                            inner,
                            field_name,
                            "in set",
                            item.span,
                            diags,
                        );
                    }
                    // RFC 0032 uniqueness: duplicates are load errors at the
                    // second occurrence's span; identity is semantic (span- and
                    // union-arm-blind), so the same value admitted via
                    // different union arms is still one element.
                    for (i, item) in items.iter().enumerate() {
                        if let Some(earlier) =
                            items[..i].iter().find(|p| p.value.semantic_eq(&item.value))
                        {
                            diags.push(
                                Diagnostic::error(format!(
                                    "duplicate set element {context} '{field_name}'{}{} — set \
                                     elements must be unique",
                                    value_label(&item.value),
                                    duplicate_clarifier(&earlier.value, &item.value)
                                ))
                                .with_code(codes::DUPLICATE_SET_ELEMENT)
                                .with_span(item.span),
                            );
                        }
                    }
                }
                // References (e.g. to consts) and env vars may resolve to arrays.
                Value::Reference(_) | Value::Secret(_) => {}
                _ => {
                    diags.push(
                        Diagnostic::error(format!(
                            "type mismatch {context} '{field_name}': expected {field_type}, got {}",
                            value_type_name(value)
                        ))
                        .with_code(codes::TYPE_MISMATCH)
                        .with_span(span),
                    );
                }
            },
            FieldType::Union(variants) => {
                if !self.value_matches_type(value, field_type) {
                    let expected = variants
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    diags.push(
                        Diagnostic::error(format!(
                            "type mismatch {context} '{field_name}': expected one of {expected}; got {}",
                            value_type_name(value)
                        ))
                        .with_code(codes::UNION_TYPE_MISMATCH)
                        .with_span(span),
                    );
                }
            }
            FieldType::Modifier(declared) => {
                self.validate_value_against_type(value, declared, field_name, context, span, diags);
            }
            FieldType::Arms { .. } => {
                // An arm set is a block of arms, never a scalar value.
                diags.push(
                    Diagnostic::error(format!(
                        "type mismatch {context} '{field_name}': expected an arm block \
                         ('{field_type}'), got {}",
                        value_type_name(value)
                    ))
                    .with_code(codes::TYPE_MISMATCH)
                    .with_span(span),
                );
            }
        }
    }

    /// Non-emitting check used for union variant matching: does `value`
    /// structurally satisfy `field_type`?
    fn value_matches_type(&self, value: &Value, field_type: &FieldType) -> bool {
        if let Value::Fallback(primary, fallback) = value {
            return self.value_matches_type(&primary.value, field_type)
                && self.value_matches_type(&fallback.value, field_type);
        }
        // References and env vars are resolved later; accept them anywhere.
        if matches!(value, Value::Reference(_) | Value::Secret(_)) {
            return true;
        }

        match field_type {
            FieldType::Primitive(prim) => value_matches_primitive(value, prim),
            FieldType::ModelRef(ref_name) => {
                if let Some(enum_def) = self.find_enum(ref_name) {
                    match value {
                        Value::String(s) => enum_def.variants.iter().any(|v| v == s),
                        // Template strings are resolved later; unverifiable here.
                        Value::TemplateString(_) => true,
                        _ => false,
                    }
                } else {
                    matches!(value, Value::String(_) | Value::TemplateString(_))
                }
            }
            FieldType::List(inner) => match value {
                Value::Array(items) => items
                    .iter()
                    .all(|item| self.value_matches_type(&item.value, inner)),
                _ => false,
            },
            // Matching is shape-only; uniqueness is enforced (with spans) in
            // `validate_value_against_type`, not here.
            FieldType::Set(inner) => match value {
                Value::Array(items) => items
                    .iter()
                    .all(|item| self.value_matches_type(&item.value, inner)),
                _ => false,
            },
            FieldType::Union(variants) => variants
                .iter()
                .any(|variant| self.value_matches_type(value, variant)),
            FieldType::Modifier(declared) => self.value_matches_type(value, declared),
            // An arm set is a block of arms; no scalar value satisfies it.
            FieldType::Arms { .. } => false,
        }
    }

    fn validate_primitive_value(
        &self,
        value: &Value,
        prim: &PrimitiveType,
        field_name: &str,
        context: &str,
        span: Span,
        diags: &mut Vec<Diagnostic>,
    ) {
        if value_matches_primitive(value, prim) {
            return;
        }
        if *prim == PrimitiveType::Duration {
            if let Value::String(text) = value {
                // RFC 0017 §4: a quoted duration is the pre-literal
                // spelling — a migration (NML0001, the replaced-syntax
                // engine), not a type mismatch. Machine-applicable: the
                // replacement spans the WHOLE quoted string (quotes
                // removed), yielding the canonical literal, and `nml fix`
                // applies it in bulk. A string that is not duration text
                // falls through to the ordinary mismatch below.
                if let Ok(d) = nml_core::duration::Duration::parse_text(text) {
                    diags.push(
                        Diagnostic::error(format!(
                            "duration field '{field_name}': a quoted duration was \
                             replaced by the duration literal (drop the quotes)"
                        ))
                        .with_code(codes::REPLACED_SYNTAX)
                        .with_span(span)
                        .with_suggestion(d.to_string(), span),
                    );
                    return;
                }
            }
        }
        if *prim == PrimitiveType::Role {
            if let Value::String(s) = value {
                // Machine-fixable: the replacement spans the WHOLE quoted
                // string (quotes removed), yielding the bare role reference.
                let replacement = if s.starts_with('@') {
                    s.clone()
                } else {
                    format!("@{s}")
                };
                let msg = format!("role field '{field_name}': roles are references, not strings");
                diags.push(
                    Diagnostic::warning(msg)
                        .with_code(codes::ROLE_LITERAL)
                        .with_span(span)
                        .with_suggestion(replacement, span),
                );
                return;
            }
        }
        let expected = if *prim == PrimitiveType::Secret {
            "environment variable ($ENV.VARIABLE_NAME)".to_string()
        } else {
            prim.as_str().to_string()
        };
        let mut diag = Diagnostic::error(format!(
            "type mismatch {context} '{field_name}': expected {expected}, got {}",
            value_type_name(value)
        ))
        .with_span(span);
        diag = if *prim == PrimitiveType::Secret {
            // Literal credential material in a `secret` field — the security
            // invariant the type exists for (see docs/stability.md).
            diag.with_code(codes::SECRET_LITERAL)
        } else {
            diag.with_code(codes::TYPE_MISMATCH)
        };
        diags.push(diag);
    }

    fn validate_enum_value(
        &self,
        value: &Value,
        enum_def: &EnumDef,
        field_name: &str,
        span: Span,
        diags: &mut Vec<Diagnostic>,
    ) {
        let variants = || {
            enum_def
                .variants
                .iter()
                .map(|v| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };

        match value {
            Value::String(s) | Value::Reference(s) => {
                if !enum_def.variants.iter().any(|v| v == s) {
                    // Acceptance stays exact; the near-miss hint comes from
                    // the shared engine (`nml_core::suggest`) and is rendered
                    // centrally (`Diagnostic::rendered_message`).
                    let mut diag = Diagnostic::error(format!(
                        "invalid value \"{s}\" for '{field_name}': expected one of {}",
                        variants()
                    ))
                    .with_code(codes::INVALID_ENUM_VALUE)
                    .with_span(span);
                    if let Some(v) =
                        nml_core::suggest::suggest(s, enum_def.variants.iter().map(String::as_str))
                    {
                        // Machine-applicable fix (RFC 0030): replace the value
                        // *content* with the canonical variant. A string
                        // literal's span includes its quotes, so the content
                        // span excludes them; a bare reference has none.
                        let content_span = match value {
                            Value::String(_) => string_content_span(span),
                            _ => span,
                        };
                        diag = diag.with_suggestion(v, content_span);
                    }
                    diags.push(diag);
                }
            }
            // Resolved later; unverifiable at validation time.
            Value::TemplateString(_) | Value::Secret(_) => {}
            _ => {
                diags.push(
                    Diagnostic::error(format!(
                        "type mismatch for '{field_name}': expected one of {}, got {}",
                        variants(),
                        value_type_name(value)
                    ))
                    .with_code(codes::TYPE_MISMATCH)
                    .with_span(span),
                );
            }
        }
    }

    fn validate_model_ref_value(
        &self,
        value: &Value,
        ref_name: &str,
        field_name: &str,
        span: Span,
        diags: &mut Vec<Diagnostic>,
    ) {
        match value {
            Value::Reference(_)
            | Value::String(_)
            | Value::TemplateString(_)
            | Value::Secret(_) => {}
            _ => {
                diags.push(
                    Diagnostic::error(format!(
                        "type mismatch for '{}': expected {} reference, got {}",
                        field_name,
                        ref_name,
                        value_type_name(value)
                    ))
                    .with_code(codes::TYPE_MISMATCH)
                    .with_span(span),
                );
            }
        }
    }

    fn validate_modifier_content(&self, m: &Modifier, diags: &mut Vec<Diagnostic>) {
        let prefix = match &self.membership.user_ref_prefix {
            Some(p) => p,
            None => return,
        };
        match &m.value {
            ModifierValue::Inline(sv) => {
                self.check_user_ref_in_value(&sv.value, sv.span, prefix, diags);
            }
            ModifierValue::Block(items) => {
                for item in items {
                    if let ListItemKind::Role(role_ref) = &item.kind {
                        if role_ref.starts_with(prefix.as_str()) {
                            diags.push(
                                Diagnostic::warning(format!(
                                    "{prefix} references are intended for members lists, not access control rules"
                                ))
                                .with_code(codes::USER_REF_IN_ACL)
                                .with_span(item.span),
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn check_user_ref_in_value(
        &self,
        value: &Value,
        span: Span,
        prefix: &str,
        diags: &mut Vec<Diagnostic>,
    ) {
        match value {
            Value::Role(r) if r.starts_with(prefix) => {
                diags.push(
                    Diagnostic::warning(format!(
                        "{prefix} references are intended for members lists, not access control rules",
                    ))
                    .with_code(codes::USER_REF_IN_ACL)
                    .with_span(span),
                );
            }
            Value::Array(items) => {
                for item in items {
                    self.check_user_ref_in_value(&item.value, item.span, prefix, diags);
                }
            }
            _ => {}
        }
    }

    fn validate_members_builtin_refs(
        &self,
        body: &Body,
        keyword: &str,
        diags: &mut Vec<Diagnostic>,
    ) {
        if self.membership.member_keywords.is_empty()
            || !self.membership.member_keywords.iter().any(|k| k == keyword)
        {
            return;
        }
        for entry in &body.entries {
            if let BodyEntryKind::NestedBlock(nb) = &entry.kind {
                self.check_builtin_in_nested_members(&nb.body, diags);
            }
        }
    }

    fn check_builtin_in_nested_members(&self, body: &Body, diags: &mut Vec<Diagnostic>) {
        if self.membership.builtin_refs.is_empty() {
            return;
        }
        for entry in &body.entries {
            match &entry.kind {
                BodyEntryKind::ListItem(item) => {
                    if let ListItemKind::Role(role_ref) = &item.kind {
                        if self.membership.builtin_refs.iter().any(|r| r == role_ref) {
                            diags.push(
                                Diagnostic::warning(
                                    "built-in access levels should not appear in members lists",
                                )
                                .with_code(codes::BUILTIN_IN_MEMBERS)
                                .with_span(item.span),
                            );
                        }
                    }
                }
                BodyEntryKind::NestedBlock(nb) => {
                    self.check_builtin_in_nested_members(&nb.body, diags);
                }
                _ => {}
            }
        }
    }

    fn validate_member_cycles(&self, file: &File, diags: &mut Vec<Diagnostic>) {
        if self.membership.member_keywords.is_empty() {
            return;
        }
        let prefixes: Vec<String> = self
            .membership
            .member_keywords
            .iter()
            .map(|kw| format!("@{kw}/"))
            .collect();
        let mut membership: HashMap<String, Vec<String>> = HashMap::new();
        // Declaration-name spans, so a cycle warning anchors at the member
        // that opens the reported cycle (every diagnostic carries a span —
        // the LSP parity invariant).
        let mut decl_spans: HashMap<String, Span> = HashMap::new();

        for decl in &file.declarations {
            match &decl.kind {
                DeclarationKind::Block(block) => {
                    if self
                        .membership
                        .member_keywords
                        .iter()
                        .any(|k| k == &block.keyword.name)
                    {
                        let refs = collect_member_refs(&block.body, &prefixes);
                        decl_spans.insert(block.name.name.clone(), block.name.span);
                        membership.insert(block.name.name.clone(), refs);
                    }
                }
                DeclarationKind::Array(arr)
                    if self
                        .membership
                        .member_keywords
                        .iter()
                        .any(|k| k == &arr.item_keyword.name) =>
                {
                    for item in &arr.body.items {
                        if let ListItemKind::Named { name, body } = &item.kind {
                            let refs = collect_member_refs(body, &prefixes);
                            decl_spans.insert(name.name.clone(), name.span);
                            membership.insert(name.name.clone(), refs);
                        }
                    }
                }
                _ => {}
            }
        }

        // Detect cycles via the shared, stack-safe iterative graph walk (a deep
        // membership chain in an untrusted file must not overflow the stack).
        let edges: HashMap<&str, Vec<&str>> = membership
            .iter()
            .map(|(name, members)| (name.as_str(), members.iter().map(String::as_str).collect()))
            .collect();
        report_graph_cycles(membership.keys().map(String::as_str), &edges, |cycle| {
            let desc = cycle
                .iter()
                .chain(std::iter::once(&cycle[0]))
                .copied()
                .collect::<Vec<_>>()
                .join(" -> ");
            let mut diag = Diagnostic::warning(format!("circular membership detected: {desc}"))
                .with_code(codes::MEMBERSHIP_CYCLE);
            if let Some(span) = decl_spans.get(cycle[0]) {
                diag = diag.with_span(*span);
            }
            diags.push(diag);
        });
    }
}

fn collect_member_refs(body: &Body, prefixes: &[String]) -> Vec<String> {
    let mut refs = Vec::new();
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::ListItem(item) => {
                if let ListItemKind::Role(role_ref) = &item.kind {
                    for prefix in prefixes {
                        if let Some(name) = role_ref.strip_prefix(prefix.as_str()) {
                            refs.push(name.to_string());
                            break;
                        }
                    }
                }
            }
            BodyEntryKind::NestedBlock(nb) => {
                refs.extend(collect_member_refs(&nb.body, prefixes));
            }
            _ => {}
        }
    }
    refs
}

/// The NML2044 advisory pushed when instance validation stops at
/// `MAX_VALIDATION_DEPTH` — one constructor for every truncation site, so
/// the message, code, and span policy can never diverge.
fn truncation_advisory(body: &Body, header_span: Option<Span>) -> Diagnostic {
    let mut diag = Diagnostic::warning(format!(
        "validation truncated: nesting exceeds maximum depth of \
         {MAX_VALIDATION_DEPTH}; deeper entries were not checked"
    ))
    .with_code(codes::VALIDATION_TRUNCATED);
    if let Some(span) = header_span.or_else(|| body.entries.first().map(|e| e.span)) {
        diag = diag.with_span(span);
    }
    diag
}

/// RFC 0032 set uniqueness over list items — the one emitter for every
/// item-set surface: error at the second occurrence, span-blind value
/// identity for scalars, name identity for named items.
fn push_duplicate_set_items(items: &[&ListItem], diags: &mut Vec<Diagnostic>) {
    for (i, item) in items.iter().enumerate() {
        if let Some(earlier) = items[..i].iter().find(|p| set_items_equal(p, item)) {
            let clarifier = match (&earlier.kind, &item.kind) {
                (
                    nml_core::ast::ListItemKind::Shorthand { value: a, .. },
                    nml_core::ast::ListItemKind::Shorthand { value: b, .. },
                ) => duplicate_clarifier(&a.value, &b.value),
                _ => String::new(),
            };
            diags.push(
                Diagnostic::error(format!(
                    "duplicate set element{}{clarifier} — set elements must be unique",
                    set_item_label(item)
                ))
                .with_code(codes::DUPLICATE_SET_ELEMENT)
                .with_span(item.span),
            );
        }
    }
}

fn value_matches_primitive(value: &Value, prim: &PrimitiveType) -> bool {
    if matches!(value, Value::Reference(_) | Value::Secret(_)) {
        return true;
    }
    match prim {
        PrimitiveType::String => matches!(value, Value::String(_) | Value::TemplateString(_)),
        PrimitiveType::Number => matches!(value, Value::Number(_)),
        PrimitiveType::Bool => matches!(value, Value::Bool(_)),
        PrimitiveType::Money => matches!(value, Value::Money(_)),
        // Template strings resolve later and are skipped, like every
        // deferred reference — the consumer's `de` coercion types the
        // resolved text (RFC 0017 §3.1).
        PrimitiveType::Duration => {
            matches!(value, Value::Duration(_) | Value::TemplateString(_))
        }
        PrimitiveType::Path => matches!(value, Value::String(_) | Value::TemplateString(_)),
        PrimitiveType::Secret => false,
        PrimitiveType::Object => false,
        PrimitiveType::Role => matches!(value, Value::Role(_)),
    }
}

/// RFC 0007 §4.3 arm-set shape rules, checked at schema-definition time. An
/// arm set describes a field's **body**, so two compositions the type grammar
/// parses have no instance form and are rejected here rather than silently
/// accepted-and-unvalidated:
///
/// - `(K -> V)` under `[]` (directly or through a union, at any depth) —
///   arms are body entries, not list items, so an array of arm sets can
///   never be written.
/// - `(K -> V)` inside another arm set's key or target — an arm's target is
///   a bare reference identifier, so a nested arm set can never be written.
/// - A union with more than one arm-set variant — the union variant is
///   selected by body *shape*, and an arms-shaped body always selects the
///   first arm-set variant, so a second would be silently unreachable.
///
/// `forbidden_context` names the enclosing position that makes an arm set
/// illegal (`None` at the top of a field type).
fn field_type_shape_errors(
    field_type: &FieldTypeExpr,
    forbidden_context: Option<&'static str>,
    span: Span,
    diags: &mut Vec<Diagnostic>,
) {
    match field_type {
        FieldTypeExpr::Named(_) => {}
        FieldTypeExpr::Array(inner) => {
            field_type_shape_errors(inner, Some("an array element"), span, diags);
        }
        FieldTypeExpr::Set(inner) => {
            // Same positional rules as an array element (an arm set nested in a
            // collection element is unreachable — RFC 0007's placement rule).
            field_type_shape_errors(inner, Some("a set element"), span, diags);
        }
        FieldTypeExpr::Union(variants) => {
            let arm_sets = variants
                .iter()
                .filter(|v| matches!(v, FieldTypeExpr::Arms { .. }))
                .count();
            if arm_sets > 1 {
                diags.push(
                    Diagnostic::error(format!(
                        "'{field_type}': a union may carry at most one arm-set variant — the \
                         variant is selected by body shape, and an arms-shaped body always \
                         selects the first, so the others would be unreachable"
                    ))
                    .with_code(codes::INVALID_TYPE_SHAPE)
                    .with_span(span),
                );
            }
            for variant in variants {
                field_type_shape_errors(variant, forbidden_context, span, diags);
            }
        }
        FieldTypeExpr::Arms { key, target } => {
            if let Some(context) = forbidden_context {
                diags.push(
                    Diagnostic::error(format!(
                        "'{field_type}': an arm set describes a field's body and cannot be \
                         {context} (it has no instance form there)"
                    ))
                    .with_code(codes::INVALID_TYPE_SHAPE)
                    .with_span(span),
                );
            }
            field_type_shape_errors(key, Some("an arm-set key"), span, diags);
            field_type_shape_errors(target, Some("an arm-set target"), span, diags);
        }
    }
}

/// Set-element identity for **body-form** items (RFC 0032): value-level for
/// scalar/shorthand items (span-blind `semantic_eq`), name-level for named /
/// reference items. Mixed kinds are never equal.
fn set_items_equal(a: &nml_core::ast::ListItem, b: &nml_core::ast::ListItem) -> bool {
    use nml_core::ast::ListItemKind as K;
    match (&a.kind, &b.kind) {
        (K::Named { name: an, .. }, K::Named { name: bn, .. }) => an.name == bn.name,
        (K::Shorthand { value: av, .. }, K::Shorthand { value: bv, .. }) => {
            av.value.semantic_eq(&bv.value)
        }
        (K::Reference(ai), K::Reference(bi)) => ai.name == bi.name,
        (K::Role(ar), K::Role(br)) => ar == br,
        _ => false,
    }
}

/// A short identity label for a duplicate-set-element diagnostic (` 'x'`), or
/// empty when the item has no concise rendering — the span already points at
/// the duplicate.
fn set_item_label(item: &nml_core::ast::ListItem) -> String {
    use nml_core::ast::ListItemKind as K;
    match &item.kind {
        K::Named { name, .. } | K::Reference(name) => format!(" '{}'", name.name),
        K::Shorthand { value, .. } => value_label(&value.value),
        K::Role(r) => format!(" '{r}'"),
    }
}

/// Why two set elements that LOOK different are the same element.
///
/// Set identity is semantic, and since RFC 0016 that includes numeric
/// cohorts: `8080` and `8080.0` are one value in two spellings. Telling
/// an author their two visibly-different literals are "duplicate" without
/// saying why is the kind of diagnostic that costs an afternoon — so when
/// the two renderings differ, name the earlier spelling explicitly.
/// Identical spellings need no explanation (the duplication is visible).
fn duplicate_clarifier(earlier: &Value, current: &Value) -> String {
    match (earlier, current) {
        (Value::Number(a), Value::Number(b)) if a.to_string() != b.to_string() => {
            format!(" (the same number as '{a}' above, written differently)")
        }
        // Same case as Number: duration identity is semantic (`30s` and
        // `30000ms` are one value in two spellings — RFC 0017), and
        // storage is faithful, so equal values really do display
        // differently and the duplicate needs its why.
        (Value::Duration(a), Value::Duration(b)) if a.to_string() != b.to_string() => {
            format!(" (the same duration as '{a}' above, written differently)")
        }
        // No money arm, deliberately: money canonicalizes at parse
        // (`19.9 USD` and `19.90 USD` both store 1990 minor units and
        // display `19.90 USD`), so two *equal* amounts can never render
        // differently — the label already shows the one canonical form,
        // and this guard could never fire. Number is different: it
        // preserves the written scale, so equal values really do
        // display differently.
        _ => String::new(),
    }
}

/// A short value rendering for duplicate diagnostics; empty for compound
/// values (the span carries the location).
/// Labels echo AUTHORED literals only (validation runs on the raw AST;
/// `$ENV` refs fall to the empty arm as `Value::Secret`) — and strings,
/// the one unbounded kind, truncate like nml-core's `echo` (32 chars +
/// `…`) so a
/// multi-KB literal cannot balloon a diagnostic. Number/Money Display
/// is bounded by the written literal's own length.
fn value_label(value: &Value) -> String {
    match value {
        Value::String(s) | Value::Role(s) => {
            const MAX_ECHO: usize = 32;
            if s.chars().count() > MAX_ECHO {
                let head: String = s.chars().take(MAX_ECHO).collect();
                format!(" '{head}…'")
            } else {
                format!(" '{s}'")
            }
        }
        Value::Number(n) => format!(" '{n}'"),
        Value::Money(m) => format!(" '{}'", m.format_display()),
        Value::Duration(d) => format!(" '{d}'"),
        Value::Bool(b) => format!(" '{b}'"),
        _ => String::new(),
    }
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::TemplateString(_) => "string",
        Value::Number(_) => "number",
        Value::Money(_) => "money",
        Value::Duration(_) => "duration",
        Value::Bool(_) => "bool",
        Value::Secret(_) => "secret",
        Value::Role(_) => "role reference",
        Value::Reference(_) => "reference",
        Value::Array(_) => "array",
        Value::Fallback(_, _) => "fallback",
    }
}

/// The content span of a string literal: the literal's span includes its
/// quotes, so a machine-applicable replacement targets the inside. Degenerate
/// spans (too short to contain quotes) are returned unchanged.
fn string_content_span(span: Span) -> Span {
    if span.end > span.start + 1 {
        Span::new(span.start + 1, span.end - 1)
    } else {
        span
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nml_core::diagnostic::Severity;

    fn make_validator(schema_source: &str) -> SchemaValidator {
        let schema = nml_core::cst::extract_schema(schema_source).0;
        SchemaValidator::new(schema.models, schema.enums, schema.oneofs)
    }

    fn make_validator_with_modifiers(schema_source: &str, modifiers: &[&str]) -> SchemaValidator {
        let schema = nml_core::cst::extract_schema(schema_source).0;
        SchemaValidator::new(schema.models, schema.enums, schema.oneofs)
            .with_modifiers(modifiers.iter().map(|s| s.to_string()).collect())
    }

    fn diags(schema: &str, source: &str) -> Vec<Diagnostic> {
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        make_validator(schema).validate(&file)
    }

    /// RFC 0016 made set identity numeric, so two spellings of one value
    /// now collide. "Duplicate" is useless when the two literals look
    /// different — the message must name the earlier spelling.
    #[test]
    fn duplicate_set_element_explains_cohort_equal_numbers() {
        let schema = "model svc:\n    name string+\n    ports set<number>\n";
        let d = diags(schema, "svc A:\n    ports = [8080, 8080.0]\n");
        let dup = d
            .iter()
            .find(|x| x.rendered_message().contains("duplicate set element"))
            .unwrap_or_else(|| panic!("expected a duplicate diagnostic, got {d:?}"));
        let msg = dup.rendered_message();
        assert!(
            msg.contains("the same number as '8080' above, written differently"),
            "must explain the non-obvious equivalence: {msg}"
        );

        // An identical spelling needs no explanation — the repetition is
        // visible, and the clarifier would just be noise.
        let d = diags(schema, "svc A:\n    ports = [8080, 8080]\n");
        let dup = d
            .iter()
            .find(|x| x.rendered_message().contains("duplicate set element"))
            .expect("duplicate");
        assert!(
            !dup.rendered_message().contains("written differently"),
            "no clarifier for identical spellings: {}",
            dup.rendered_message()
        );
    }

    /// Money duplicates get a value label but never a clarifier: money
    /// canonicalizes at parse (both spellings below store 1990 minor
    /// units and display `19.90 USD`), so the label alone shows the
    /// collision and a "written differently" note has nothing to add.
    #[test]
    fn duplicate_set_element_labels_money_canonically() {
        let schema = "model svc:\n    name string+\n    prices set<money>\n";
        let d = diags(schema, "svc A:\n    prices = [19.90 USD, 19.9 USD]\n");
        let dup = d
            .iter()
            .find(|x| x.rendered_message().contains("duplicate set element"))
            .unwrap_or_else(|| panic!("expected a duplicate diagnostic, got {d:?}"));
        let msg = dup.rendered_message();
        assert!(
            msg.contains("'19.90 USD'"),
            "must name the canonical amount: {msg}"
        );
        assert!(
            !msg.contains("written differently"),
            "money has no distinct spellings to clarify: {msg}"
        );

        // Body form routes through the OTHER emitter
        // (push_duplicate_set_items -> set_item_label) — same label,
        // same posture.
        let d = diags(
            schema,
            "svc A:\n    prices:\n        - 19.90 USD\n        - 19.9 USD\n",
        );
        let dup = d
            .iter()
            .find(|x| x.rendered_message().contains("duplicate set element"))
            .unwrap_or_else(|| panic!("body-form duplicate expected, got {d:?}"));
        assert!(
            dup.rendered_message().contains("'19.90 USD'"),
            "body form must label canonically: {}",
            dup.rendered_message()
        );
    }

    // ── RFC 0030: structured suggestions ──

    /// The enum did-you-mean carries a machine-applicable suggestion whose
    /// span covers the string's *content* (inside the quotes): applying the
    /// replacement to the source yields the canonical value in place.
    #[test]
    fn enum_did_you_mean_carries_applicable_suggestion() {
        let source = "settings app:\n    mode = \"lax\"\n";
        let d = diags(
            "enum sameSite:\n    - \"Lax\"\n    - \"Strict\"\nmodel settings:\n    name string+\n    mode sameSite\n",
            source,
        );
        let diag = d
            .iter()
            .find(|x| x.rendered_message().contains("did you mean \"Lax\""))
            .expect("did-you-mean diagnostic");
        let sug = diag.suggestions.first().expect("structured suggestion");
        assert_eq!(sug.replacement, "Lax");
        let mut fixed = source.to_string();
        fixed.replace_range(sug.span.start..sug.span.end, &sug.replacement);
        assert!(fixed.contains("mode = \"Lax\""), "applied: {fixed}");
    }

    // ── RFC 0005: identity materialization ──

    #[test]
    fn named_item_satisfies_required_name_no_false_positive() {
        // `- editor:` supplies the role's identity; the required `name` field is
        // present after materialization, so there is no "missing required field".
        let d = diags(
            "model role:\n    name string\n    description string?\n",
            "[]role roles:\n    - editor:\n        description = \"Editing\"\n",
        );
        assert!(
            !d.iter().any(|x| x.message.contains("name")),
            "unexpected name diagnostic: {:?}",
            d.iter().map(|x| &x.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn scalar_shorthand_fills_marked_field() {
        let d = diags(
            "model resource:\n    name string?\n    path path+\n",
            "[]resource resources:\n    - \"/api\"\n",
        );
        assert!(d.is_empty(), "unexpected diagnostics: {d:?}");
    }

    #[test]
    fn name_is_shorthand_named_and_scalar_both_fill_it() {
        // `name string+` — identity *is* the name, so both forms fill `name`:
        // the named key (`- editor:`) and the scalar (`- "viewer"`).
        let d = diags(
            "model role:\n    name string+\n    description string?\n",
            "[]role roles:\n    - editor:\n        description = \"x\"\n    - \"viewer\"\n",
        );
        assert!(d.is_empty(), "unexpected diagnostics: {d:?}");
    }

    #[test]
    fn validator_and_de_agree_on_scalar_shorthand() {
        // Agreement guardrail (RFC §11.10): the *same* instance validates clean AND
        // deserializes clean with matching fields — the de-path closed the transitional
        // "validates but de errors" gap.
        let schema = "model resource:\n    path string+\n    method string?\n\nmodel svc:\n    resources []resource\n";
        let instance = "svc s:\n    resources:\n        - \"/api\"\n        - \"/health\":\n            method = \"GET\"\n";

        // (1) validates clean.
        assert!(diags(schema, instance).is_empty(), "should validate clean");

        // (2) deserializes clean, same fields.
        #[derive(serde::Deserialize)]
        struct Resource {
            path: String,
            method: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct Svc {
            resources: Vec<Resource>,
        }

        let mut ex = nml_core::cst::extract_schema(schema).0;
        nml_core::schema::resolve_model_inheritance(&mut ex);
        let index = nml_core::SchemaIndex::build(ex.models, ex.enums, ex.oneofs);
        let file = nml_core::cst::parse_to_ast(instance).unwrap();
        let nml_core::ast::DeclarationKind::Block(block) = &file.declarations[0].kind else {
            panic!("expected block");
        };
        let svc: Svc = nml_core::from_body_defaulted(
            &index,
            "svc",
            &block.body,
            &nml_core::ValueResolver::env(),
        )
        .expect("should deserialize");
        assert_eq!(svc.resources[0].path, "/api");
        assert_eq!(svc.resources[0].method, None);
        assert_eq!(svc.resources[1].path, "/health");
        assert_eq!(svc.resources[1].method.as_deref(), Some("GET"));
    }

    #[test]
    fn scalar_shorthand_with_body_fills_field_and_validates() {
        // `- "/admin":` + body: the scalar fills `path+`, the body sets `method`.
        let d = diags(
            "enum httpMethod:\n    - \"GET\"\n    - \"POST\"\nmodel resource:\n    path path+\n    method httpMethod = \"GET\"\n",
            "[]resource resources:\n    - \"/admin\":\n        method = \"POST\"\n",
        );
        assert!(d.is_empty(), "scalar-with-body should validate: {d:?}");
    }

    #[test]
    fn scalar_shorthand_with_body_type_checks_the_body() {
        // The body is validated too: an unknown enum value is caught.
        let d = diags(
            "enum httpMethod:\n    - \"GET\"\n    - \"POST\"\nmodel resource:\n    path path+\n    method httpMethod = \"GET\"\n",
            "[]resource resources:\n    - \"/admin\":\n        method = \"BOGUS\"\n",
        );
        assert!(
            !d.is_empty(),
            "invalid method in the body should be flagged"
        );
    }

    #[test]
    fn scalar_without_shorthand_field_is_dropped_key_without_noise() {
        // The dropped-key diagnostic is the *only* one — no spurious "missing
        // required field" piled on from validating an empty body.
        let d = diags(
            "model role:\n    name string\n    label string\n",
            "[]role roles:\n    - \"/api\"\n",
        );
        assert_eq!(
            d.len(),
            1,
            "expected only the dropped-key diagnostic: {d:?}"
        );
        assert!(d[0].message.contains("no shorthand field"), "{d:?}");
    }

    #[test]
    fn scalar_shorthand_on_union_list_is_flagged() {
        let schema = "model a:\n    x string?\nmodel b:\n    y string?\noneof u by kind:\n    \"a\" -> a\n    \"b\" -> b\n";
        let d = diags(schema, "[]u items:\n    - \"foo\"\n");
        assert!(
            d.iter().any(|x| x.message.contains("union-typed lists")),
            "{d:?}"
        );
    }

    #[test]
    fn explicit_name_wins_over_key_lenient() {
        // Lenient (matches `de`): an explicit `name` overrides the key — no error.
        let d = diags(
            "model role:\n    name string\n",
            "[]role roles:\n    - editor:\n        name = \"other\"\n",
        );
        assert!(d.is_empty(), "explicit name should win, not error: {d:?}");
    }

    #[test]
    fn block_declaration_name_satisfies_required_name() {
        // `role editor:` (block) fills `name` from the block name — no false
        // "missing required field 'name'".
        let d = diags(
            "model role:\n    name string\n    description string?\n",
            "role editor:\n    description = \"Editing\"\n",
        );
        assert!(d.is_empty(), "block name should satisfy `name`: {d:?}");
    }

    #[test]
    fn block_explicit_name_wins_over_block_name() {
        // Lenient: an explicit `name` overrides the block identifier (the identifier
        // stays the reference handle) — no error.
        let d = diags(
            "model widget:\n    name string\n    size number?\n",
            "widget Gizmo:\n    name = \"gizmo\"\n    size = 2\n",
        );
        assert!(
            d.is_empty(),
            "explicit name should win over block name: {d:?}"
        );
    }

    #[test]
    fn test_empty_modifiers_accepts_all() {
        let validator = make_validator("");
        let source = "service Svc:\n    |anything = [@public]\n    localMount = \"/\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let modifier_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("modifier"))
            .collect();
        assert!(modifier_diags.is_empty());
    }

    #[test]
    fn test_valid_modifiers() {
        let validator = make_validator_with_modifiers("", &["allow", "deny"]);
        let source =
            "service Svc:\n    |allow = [@public]\n    |deny = []\n    localMount = \"/\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let modifier_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("modifier"))
            .collect();
        assert!(modifier_diags.is_empty());
    }

    #[test]
    fn test_invalid_modifier_name() {
        let validator = make_validator_with_modifiers("", &["allow", "deny"]);
        let source = "service Svc:\n    |forbid = [@public]\n    localMount = \"/\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown modifier '|forbid'"))
        );
    }

    #[test]
    fn test_field_definition_outside_model() {
        let validator = make_validator("");
        let source = "service Svc:\n    name string\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags.iter().any(|d| {
            d.message
                .contains("field definitions are only allowed in model declarations")
        }));
    }

    #[test]
    fn test_field_definition_in_model_ok() {
        let validator = make_validator("");
        let source = "model provider:\n    name string\n    url string?\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let field_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("field definitions"))
            .collect();
        assert!(field_diags.is_empty());
    }

    #[test]
    fn test_unknown_property() {
        let schema = "model mount:\n    path string\n    wasm string?\n";
        let validator = make_validator(schema);

        let source = "mount Test:\n    path = \"/\"\n    unknown = \"value\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown property 'unknown'"))
        );
    }

    #[test]
    fn test_required_field_missing() {
        let schema = "model mount:\n    path string\n    wasm string?\n";
        let validator = make_validator(schema);

        let source = "mount Test:\n    wasm = \"handler.wasm\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("missing required field 'path'"))
        );
    }

    #[test]
    fn test_required_field_with_default_ok() {
        let schema = "model prompt:\n    outputFormat string = \"text\"\n";
        let validator = make_validator(schema);

        let source = "prompt MyPrompt:\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let required_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("missing required"))
            .collect();
        assert!(required_diags.is_empty());
    }

    #[test]
    fn test_type_mismatch() {
        let schema = "model mount:\n    path string\n";
        let validator = make_validator(schema);

        let source = "mount Test:\n    path = 42\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("type mismatch")
                    && d.message.contains("expected string"))
        );
    }

    #[test]
    fn test_type_match_ok() {
        let schema = "model mount:\n    path string\n    port number?\n";
        let validator = make_validator(schema);

        let source = "mount Test:\n    path = \"/api\"\n    port = 8080\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let type_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("type mismatch"))
            .collect();
        assert!(type_diags.is_empty());
    }

    #[test]
    fn test_enum_validation_valid() {
        let schema = "enum providerType:\n    - \"openai\"\n    - \"groq\"\n\nmodel provider:\n    type providerType\n";
        let validator = make_validator(schema);

        let source = "provider Groq:\n    type = \"groq\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let enum_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("invalid value"))
            .collect();
        assert!(enum_diags.is_empty());
    }

    #[test]
    fn test_enum_validation_invalid() {
        let schema = "enum providerType:\n    - \"openai\"\n    - \"groq\"\n\nmodel provider:\n    type providerType\n";
        let validator = make_validator(schema);

        let source = "provider Groq:\n    type = \"gemini\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("invalid value \"gemini\""))
        );
    }

    #[test]
    fn test_enum_invalid_suggests_nearest_variant() {
        // Diagnostics are fuzzy (case-insensitive first, then edit distance);
        // acceptance stays exact — the value is still rejected.
        let schema = "enum sameSite:\n    - \"Strict\"\n    - \"Lax\"\n    - \"None\"\n\nmodel c:\n    policy sameSite\n";
        let validator = make_validator(schema);

        // Wrong casing → suggest the canonical spelling.
        let file = nml_core::cst::parse_to_ast("c C:\n    policy = \"lax\"\n").unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.rendered_message().contains("invalid value \"lax\"")
                    && d.rendered_message().contains("did you mean \"Lax\"?")),
            "case-only miss must suggest the canonical variant: {:?}",
            diags
                .iter()
                .map(|d| d.rendered_message())
                .collect::<Vec<_>>()
        );

        // A light typo → nearest by edit distance.
        let file = nml_core::cst::parse_to_ast("c C:\n    policy = \"Stric\"\n").unwrap();
        assert!(
            validator
                .validate(&file)
                .iter()
                .any(|d| d.rendered_message().contains("did you mean \"Strict\"?"))
        );

        // A transposition — the dominant real-world typo — must be caught
        // even on a short value (OSA distance 1; plain Levenshtein would
        // score it 2 and miss it).
        let file = nml_core::cst::parse_to_ast("c C:\n    policy = \"aLx\"\n").unwrap();
        assert!(
            validator
                .validate(&file)
                .iter()
                .any(|d| d.rendered_message().contains("did you mean \"Lax\"?"))
        );

        // Something far from every variant → no (misleading) suggestion.
        let file = nml_core::cst::parse_to_ast("c C:\n    policy = \"whatever\"\n").unwrap();
        assert!(validator.validate(&file).iter().any(|d| {
            d.rendered_message().contains("invalid value \"whatever\"")
                && !d.rendered_message().contains("did you mean")
        }));
    }

    #[test]
    fn unknown_property_gets_field_suggestion() {
        // Typo'd property → nearest declared field, as a machine-applicable
        // fix on the property-name token (rustc's unknown-field experience).
        let validator = make_validator("model service:\n    provider string?\n    port number?\n");
        let source = "service S:\n    provder = \"x\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let diag = diags
            .iter()
            .find(|d| d.message.contains("unknown property 'provder'"))
            .expect("unknown property flagged");
        let sug = diag.suggestions.first().expect("field suggestion");
        assert_eq!(sug.replacement, "provider");
        // Applying the fix must produce the declared field name exactly.
        let fixed = format!(
            "{}{}{}",
            &source[..sug.span.start],
            sug.replacement,
            &source[sug.span.end..]
        );
        assert!(fixed.contains("provider = \"x\""), "{fixed}");
        assert!(
            diag.rendered_message()
                .contains("did you mean \"provider\"?"),
            "{}",
            diag.rendered_message()
        );
    }

    #[test]
    fn unknown_modifier_gets_suggestion() {
        let validator = make_validator_with_modifiers("", &["allow", "deny"]);
        let file = nml_core::cst::parse_to_ast("service S:\n    |alow = [@admin]\n").unwrap();
        let diags = validator.validate(&file);
        let diag = diags
            .iter()
            .find(|d| d.message.contains("unknown modifier '|alow'"))
            .expect("unknown modifier flagged");
        assert_eq!(
            diag.suggestions.first().map(|s| s.replacement.as_str()),
            Some("allow")
        );
    }

    #[test]
    fn unknown_oneof_discriminator_gets_suggestion() {
        let schema = "model emailLog:\n    path string?\nmodel emailPostmark:\n    apiKey string?\noneof email by kind:\n    \"log\"      -> emailLog\n    \"postmark\" -> emailPostmark\n";
        let validator = make_validator(schema);
        let file = nml_core::cst::parse_to_ast("email E:\n    kind = \"postmrak\"\n").unwrap();
        let diags = validator.validate(&file);
        let diag = diags
            .iter()
            .find(|d| d.message.contains("unknown kind \"postmrak\""))
            .expect("unknown discriminator flagged");
        // Transposition → suggested; span excludes the quotes.
        let sug = diag.suggestions.first().expect("discriminator suggestion");
        assert_eq!(sug.replacement, "postmark");
        assert_eq!(sug.span.end - sug.span.start, "postmrak".len());
    }

    #[test]
    fn strict_unknown_block_keyword_gets_model_suggestion() {
        let validator = make_validator("model service:\n    port number?\n").strict();
        let file = nml_core::cst::parse_to_ast("servce S:\n    port = 1\n").unwrap();
        let diags = validator.validate(&file);
        let diag = diags
            .iter()
            .find(|d| d.message.contains("block keyword 'servce'"))
            .expect("strict unknown keyword flagged");
        assert_eq!(
            diag.suggestions.first().map(|s| s.replacement.as_str()),
            Some("service")
        );
    }

    #[test]
    fn strict_unknown_block_keyword_suggests_oneofs_too() {
        // Keywords resolve to models OR oneofs; the suggestion pool must
        // cover both.
        let schema =
            "model emailLog:\n    path string?\noneof email by kind:\n    \"log\" -> emailLog\n";
        let validator = make_validator(schema).strict();
        let file = nml_core::cst::parse_to_ast("emial E:\n    kind = \"log\"\n").unwrap();
        let diags = validator.validate(&file);
        let diag = diags
            .iter()
            .find(|d| d.message.contains("block keyword 'emial'"))
            .expect("strict unknown keyword flagged");
        assert_eq!(
            diag.suggestions.first().map(|s| s.replacement.as_str()),
            Some("email")
        );
    }

    #[test]
    fn strict_unknown_array_keyword_gets_suggestion() {
        let validator = make_validator("model resource:\n    name string+\n").strict();
        let file =
            nml_core::cst::parse_to_ast("[]resorce R:\n    - a:\n        name = \"x\"\n").unwrap();
        let diags = validator.validate(&file);
        let diag = diags
            .iter()
            .find(|d| d.message.contains("array item keyword 'resorce'"))
            .expect("strict unknown array keyword flagged");
        assert_eq!(
            diag.suggestions.first().map(|s| s.replacement.as_str()),
            Some("resource")
        );
    }

    #[test]
    fn test_array_declaration_modifier_validation() {
        let validator = make_validator_with_modifiers("", &["allow", "deny"]);
        let source =
            "[]mount mounts:\n    |restrict = [@admin]\n    - Test:\n        path = \"/\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown modifier '|restrict'"))
        );
    }

    #[test]
    fn test_all_fields_present_ok() {
        let schema = "model mount:\n    path string\n    wasm string\n";
        let validator = make_validator(schema);

        let source = "mount Root:\n    path = \"/\"\n    wasm = \"handler.wasm\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_secret_type() {
        let schema = "model provider:\n    apiKey secret?\n";
        let validator = make_validator(schema);

        let source = "provider P:\n    apiKey = $ENV.MY_KEY\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let type_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("type mismatch"))
            .collect();
        assert!(type_diags.is_empty());
    }

    #[test]
    fn test_object_type_accepts_nested_block_with_any_keys() {
        let schema = "model plugin:\n    wasm string\n    config object?\n";
        let validator = make_validator(schema);

        let source = "plugin EchoPlugin:\n    wasm = \"echo.wasm\"\n    config:\n        prefix = \"echo\"\n        count = 3\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.is_empty(),
            "object type should accept nested block with any keys; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_nested_validation_catches_typo_in_nested_block() {
        let schema = "model prompt:\n    system string?\n    outputFormat string?\n\nmodel step:\n    prompt prompt?\n";
        let validator = make_validator(schema);

        let source = "step MyStep:\n    prompt:\n        systm = \"typo\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown property 'systm'")),
            "nested validation should catch typo; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_nested_validation_valid_nested_block() {
        let schema = "model prompt:\n    system string?\n    outputFormat string?\n\nmodel step:\n    prompt prompt?\n";
        let validator = make_validator(schema);

        let source = "step MyStep:\n    prompt:\n        system = \"You are helpful\"\n        outputFormat = \"text\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.is_empty(),
            "valid nested block should pass; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_nested_validation_missing_required_in_nested_block() {
        let schema = "model nested:\n    required string\n\nmodel parent:\n    child nested?\n";
        let validator = make_validator(schema);

        let source = "parent P:\n    child:\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("missing required field 'required'")),
            "nested validation should catch missing required; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_list_field_validates_item_properties() {
        let schema = "model prompt:\n    system string?\n    outputFormat string?\n\nmodel step:\n    provider string?\n    prompt prompt?\n    next string?\n\nmodel workflow:\n    entrypoint string\n    steps []step\n";
        let validator = make_validator(schema);

        let source = "workflow W:\n    entrypoint = \"start\"\n    steps:\n        - myStep:\n            provder = \"bad-typo\"\n            next = \"end\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown property 'provder'")),
            "should catch typo inside list item; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_list_field_valid_items_pass() {
        let schema = "model prompt:\n    system string?\n    outputFormat string?\n\nmodel step:\n    provider string?\n    prompt prompt?\n    next string?\n\nmodel workflow:\n    entrypoint string\n    steps []step\n";
        let validator = make_validator(schema);

        let source = "workflow W:\n    entrypoint = \"start\"\n    steps:\n        - s1:\n            provider = \"groq\"\n            next = \"s2\"\n        - s2:\n            provider = \"openai\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.is_empty(),
            "valid list items should pass; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_real_workflow_model_parses_and_validates() {
        let schema = r#"
enum providerType:
    - "anthropic"
    - "openai"
    - "groq"
    - "ollama"

enum outputFormat:
    - "json"
    - "text"
    - "stream"

model provider:
    type providerType
    model string
    temperature number?
    baseUrl string?
    apiKey secret?

model prompt:
    system string?
    template string?
    outputFormat outputFormat = "text"

model condition:
    field string
    equals string?
    pattern string?

model route:
    when condition
    goto string

model plugin:
    |allow []string?
    |deny []string?
    wasm string
    config object?

model step:
    provider string?
    prompt prompt?
    plugin string?
    wasm string?
    routes []route?
    default string?
    next string?
    fixed bool = true

model extensionPoint:
    after string
    allowedCapabilities []string?

model workflow:
    entrypoint string
    steps []step
    extensions []extensionPoint?
"#;

        let parse_result = nml_core::cst::parse_to_ast(schema);
        assert!(
            parse_result.is_ok(),
            "workflow.model.nml should parse; error: {:?}",
            parse_result.err()
        );

        let validator = make_validator(schema);
        let wf_model = validator.find_model("workflow");
        assert!(wf_model.is_some(), "should find 'workflow' model");
        let step_model = validator.find_model("step");
        assert!(step_model.is_some(), "should find 'step' model");

        let source = r#"
workflow W:
    entrypoint = "classify"
    steps:
        - classify:
            provider = "groq"
            blaasdsa = "asdasd"
            next = "end"
"#;
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown property 'blaasdsa'")),
            "should catch blaasdsa; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_list_field_catches_unknown_prop_no_spaces() {
        let schema = "model prompt:\n    system string?\n    outputFormat string?\n\nmodel step:\n    provider string?\n    prompt prompt?\n    next string?\n\nmodel workflow:\n    entrypoint string\n    steps []step\n";
        let validator = make_validator(schema);

        let source = "workflow W:\n    entrypoint = \"start\"\n    steps:\n        - s1:\n            provider = \"groq\"\n            blaasdsa=\"asdasd\"\n            next = \"end\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown property 'blaasdsa'")),
            "should catch unknown prop with no-space equals; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_secret_plain_string_error_message() {
        let schema = "model auth:\n    secret secret?\n";
        let validator = make_validator(schema);

        let source = "auth A:\n    secret = \"dev-secret\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.iter().any(|d| d
                .message
                .contains("expected environment variable ($ENV.VARIABLE_NAME)")),
            "should show helpful secret error message; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_fallback_flags_string_in_secret_field() {
        let schema = "model auth:\n    secret secret?\n";
        let validator = make_validator(schema);

        let source = "auth A:\n    secret = $ENV.AUTH_SECRET | \"dev-secret\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("type mismatch") && d.message.contains("got string")),
            "should flag string fallback in secret field; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_fallback_secret_primary_ok() {
        let schema = "model auth:\n    secret secret?\n";
        let validator = make_validator(schema);

        let source = "auth A:\n    secret = $ENV.AUTH_SECRET\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let type_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("type mismatch"))
            .collect();
        assert!(
            type_diags.is_empty(),
            "env var should match secret type; diags: {:?}",
            type_diags
        );
    }

    #[test]
    fn test_fallback_env_var_for_number_field() {
        let schema = "model server:\n    port number?\n";
        let validator = make_validator(schema);

        let source = "server S:\n    port = $ENV.PORT | 3000\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let type_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("type mismatch"))
            .collect();
        assert!(
            type_diags.is_empty(),
            "env var + number fallback should be valid for number field; diags: {:?}",
            type_diags
        );
    }

    #[test]
    fn test_fallback_string_for_number_field_flagged() {
        let schema = "model server:\n    port number?\n";
        let validator = make_validator(schema);

        let source = "server S:\n    port = $ENV.PORT | \"three\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("type mismatch") && d.message.contains("got string")),
            "string fallback should be flagged for number field; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_fallback_both_env_vars_ok() {
        let schema = "model auth:\n    secret secret?\n";
        let validator = make_validator(schema);

        let source = "auth A:\n    secret = $ENV.PRIMARY | $ENV.FALLBACK\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let type_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("type mismatch"))
            .collect();
        assert!(
            type_diags.is_empty(),
            "two env vars should both be valid; diags: {:?}",
            type_diags
        );
    }

    #[test]
    fn test_list_field_nested_model_ref_in_item() {
        let schema = "model prompt:\n    system string?\n    outputFormat string?\n\nmodel step:\n    provider string?\n    prompt prompt?\n    next string?\n\nmodel workflow:\n    entrypoint string\n    steps []step\n";
        let validator = make_validator(schema);

        let source = "workflow W:\n    entrypoint = \"start\"\n    steps:\n        - s1:\n            provider = \"groq\"\n            prompt:\n                systm = \"typo\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown property 'systm'")),
            "should catch typo in nested model inside list item; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_union_flat_branch_validates() {
        let schema = "model step:\n    provider string?\n    emit string?\n    parallel [](step | []step)?\n";
        let validator = make_validator(schema);

        let source = "step Fork:\n    parallel:\n        - branchA:\n            emit = \"hello\"\n        - branchB:\n            provider = \"fast\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let warnings: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("unknown property"))
            .collect();
        assert!(warnings.is_empty(), "unexpected warnings: {:?}", warnings);
    }

    #[test]
    fn test_union_grouped_thread_validates() {
        let schema = "model step:\n    provider string?\n    emit string?\n    parallel [](step | []step)?\n";
        let validator = make_validator(schema);

        let source = "step Fork:\n    parallel:\n        - pipeline:\n            - stepA:\n                emit = \"starting\"\n            - stepB:\n                emit = \"done\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let warnings: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("unknown property"))
            .collect();
        assert!(warnings.is_empty(), "unexpected warnings: {:?}", warnings);
    }

    #[test]
    fn test_union_grouped_thread_catches_unknown_property() {
        let schema = "model step:\n    provider string?\n    emit string?\n    parallel [](step | []step)?\n";
        let validator = make_validator(schema);

        let source = "step Fork:\n    parallel:\n        - pipeline:\n            - stepA:\n                emit = \"hello\"\n                bogus = \"bad\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown property 'bogus'")),
            "expected warning about 'bogus', got: {:?}",
            diags
        );
    }

    // — Spelling parity + the item-shape decision matrix —

    /// THE parity invariant behind the `FieldTarget` payload redesign: the
    /// inline-array spelling (`f = [v]`) and the dash-item spelling
    /// (`f:` + `- v`) of the same field are INDISTINGUISHABLE in what they
    /// report — code, quick-fix count, message prose, severity, and the
    /// text the span highlights. A spelling can never silently lose a
    /// check, a fix, or the words that name the location. (Before the
    /// redesign the dash spelling skipped ALL leaf/union element
    /// validation: literal credentials in `[]secret` fields passed
    /// `nml check`. Two later rounds found prose-only and context-word
    /// divergences that a codes-only comparison could not see — hence the
    /// full-payload comparison here.)
    #[test]
    fn spelling_parity_is_indistinguishable() {
        let schema = "enum level:\n    - \"debug\"\n    - \"info\"\n\nmodel box:\n    keys []secret?\n    lv []level?\n    tags []string?\n    mix [](string | number)?\n    st set<string>?\n    who []role?\n";
        let cases = [
            ("keys", "\"hardcoded\""),
            ("lv", "\"deubg\""),
            ("tags", "42"),
            ("mix", "true"),
            ("st", "42"),
            ("who", "\"notarole\""),
        ];
        // EVERYTHING a user perceives must match across spellings: code,
        // quick-fix count, message prose ("in set 'tags'" must not become
        // "in array 'tags'"), severity, and the SOURCE TEXT the span points
        // at — line/column legitimately differ between spellings, the
        // highlighted text must not.
        let payload = |ds: &[Diagnostic], src: &str| {
            let mut v: Vec<(String, usize, String, String, String)> = ds
                .iter()
                .map(|d| {
                    (
                        d.code.map(|c| c.to_string()).unwrap_or_default(),
                        d.suggestions.len(),
                        d.rendered_message(),
                        d.severity.to_string(),
                        d.span
                            .map(|sp| src[sp.start..sp.end].to_string())
                            .unwrap_or_default(),
                    )
                })
                .collect();
            v.sort();
            v
        };
        for (field, value) in cases {
            let inline_src = format!("box B:\n    {field} = [{value}]\n");
            let dash_src = format!("box B:\n    {field}:\n        - {value}\n");
            let inline = payload(&diags_for(schema, &inline_src), &inline_src);
            let dash = payload(&diags_for(schema, &dash_src), &dash_src);
            assert!(!inline.is_empty(), "{field}: inline spelling must diagnose");
            assert_eq!(
                inline, dash,
                "{field} = [{value}]: spellings disagree\ninline: {inline:?}\ndash: {dash:?}"
            );
        }
    }

    /// The decision matrix's non-parity cells, one pin per decided outcome
    /// (sub-shapes included: body presence changes the decision). The
    /// grouped-thread cell is pinned POSITIVE — a named item's body under
    /// `[]step`-style elements is a nested group, never a dropped body.
    #[test]
    fn item_shape_matrix_decisions() {
        let schema = "model step:\n    run string?\nmodel box:\n    tags []string?\n    nested [][]string?\n    obj []object?\n    steps [](step | []step)?\n";
        let codes = |src: &str| -> Vec<String> {
            diags_for(schema, src)
                .iter()
                .filter_map(|d| d.code.map(|c| c.to_string()))
                .collect()
        };
        // Leaf × Named{non-empty body} → dropped body (NML2055).
        assert!(
            codes("box B:\n    tags:\n        - foo:\n            k = \"v\"\n")
                .contains(&"NML2055".to_string())
        );
        // Leaf × Named{EMPTY body} → skip (nothing dropped).
        assert!(codes("box B:\n    tags:\n        - foo:\n").is_empty());
        // Leaf × Reference → skip (resolves later).
        assert!(codes("box B:\n    tags:\n        - foo\n").is_empty());
        // ListOf × scalar → type mismatch (a scalar cannot fill a nested list).
        assert!(!codes("box B:\n    nested:\n        - \"scalar\"\n").is_empty());
        // ListOf × Named{body} → grouped thread: recurse, NO NML2055.
        assert!(
            !codes("box B:\n    steps:\n        - Group:\n            - Step:\n                run = \"x\"\n")
                .contains(&"NML2055".to_string())
        );
        // Object × anything → free-form skip.
        assert!(codes("box B:\n    obj:\n        - foo:\n            k = 1\n").is_empty());
    }

    // ── RFC 0015 nominal union disambiguation ────────────────────────────────

    const SAME_CLASS_SCHEMA: &str = "model modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel host:\n    slot (modelA | modelB)?\n    slots [](modelA | modelB)?\n";

    fn diags_for(schema: &str, source: &str) -> Vec<Diagnostic> {
        let validator = make_validator(schema);
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        validator.validate(&file)
    }

    #[test]
    fn nominal_annotation_selects_variant_and_validates_against_it() {
        // `as modelB` selects modelB; a modelB-legal body is clean.
        let diags = diags_for(
            SAME_CLASS_SCHEMA,
            "host H:\n    slot as modelB:\n        b = \"x\"\n",
        );
        assert!(
            diags.is_empty(),
            "valid annotation must be clean: {diags:?}"
        );
    }

    #[test]
    fn nominal_annotation_is_checked_not_a_cast() {
        // `as modelB` but the body carries modelA's field → validated against
        // modelB, so `a` is an unknown property. `as` narrows, never bypasses.
        let diags = diags_for(
            SAME_CLASS_SCHEMA,
            "host H:\n    slot as modelB:\n        a = \"x\"\n",
        );
        assert!(
            diags.iter().any(|d| d.message.contains("unknown property")),
            "annotation must validate the body against the named variant: {diags:?}"
        );
    }

    #[test]
    fn d2_same_class_anonymous_is_a_hard_error() {
        // No annotation, keyed body, two model variants → ambiguous → D2.
        let diags = diags_for(SAME_CLASS_SCHEMA, "host H:\n    slot:\n        a = \"x\"\n");
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::AMBIGUOUS_UNION_INSTANCE)),
            "same-class anonymous must be a hard error: {diags:?}"
        );
    }

    #[test]
    fn list_shape_on_model_only_union_is_flagged_not_dropped() {
        // A list body under a same-class MODEL union matches no variant — it must
        // be flagged, never silently resolved to nothing (would otherwise be an
        // un-validated, un-diagnosed drop).
        let diags = diags_for(
            SAME_CLASS_SCHEMA,
            "host H:\n    slot:\n        - foo:\n            a = \"x\"\n",
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::UNION_TYPE_MISMATCH)),
            "a list body on a model-only union must be flagged: {diags:?}"
        );
    }

    #[test]
    fn unknown_variant_annotation_errors_with_did_you_mean() {
        let diags = diags_for(
            SAME_CLASS_SCHEMA,
            "host H:\n    slot as modelC:\n        b = \"x\"\n",
        );
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::UNKNOWN_UNION_VARIANT))
            .expect("unknown-variant error");
        assert!(
            !d.suggestions.is_empty(),
            "unknown variant must carry a machine-applicable did-you-mean: {d:?}"
        );
        // No noise: after the union-level error, the body must NOT be validated
        // against a guessed variant (no spurious unknown-property pile-on).
        assert_eq!(
            diags.len(),
            1,
            "exactly the union-level finding, no guessed-variant noise: {diags:?}"
        );
    }

    #[test]
    fn d2_and_annotation_apply_at_list_element_level_too() {
        // Ambiguous at a `[](A | B)` element → D2.
        let ambiguous = diags_for(
            SAME_CLASS_SCHEMA,
            "host H:\n    slots:\n        - one:\n            a = \"x\"\n",
        );
        assert!(
            ambiguous
                .iter()
                .any(|d| d.code == Some(codes::AMBIGUOUS_UNION_INSTANCE)),
            "list-element same-class anonymous must error: {ambiguous:?}"
        );
        // `- one as modelB:` disambiguates the element cleanly.
        let annotated = diags_for(
            SAME_CLASS_SCHEMA,
            "host H:\n    slots:\n        - one as modelB:\n            b = \"x\"\n",
        );
        assert!(
            annotated.is_empty(),
            "annotated list element must be clean: {annotated:?}"
        );
    }

    #[test]
    fn stray_annotation_on_non_union_field_is_flagged() {
        // `as` is only meaningful on a union field; on a plain model field it is
        // flagged, never silently ignored.
        let schema = "model inner:\n    x string?\nmodel host:\n    slot inner?\n";
        let diags = diags_for(schema, "host H:\n    slot as other:\n        x = \"v\"\n");
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::STRAY_TYPE_ANNOTATION)),
            "stray annotation on a non-union field must be flagged: {diags:?}"
        );
    }

    #[test]
    fn stray_annotation_on_non_union_list_element_is_flagged() {
        // The stray check covers list elements too, not just field-level blocks.
        let schema = "model inner:\n    x string?\nmodel host:\n    items []inner?\n";
        let diags = diags_for(
            schema,
            "host H:\n    items:\n        - one as other:\n            x = \"v\"\n",
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::STRAY_TYPE_ANNOTATION)),
            "stray annotation on a non-union list element must be flagged: {diags:?}"
        );
    }

    #[test]
    fn modifier_wrapped_union_is_gated_like_a_plain_union() {
        // `|slot (modelA | modelB)` written as a nested block takes the SAME
        // gated path as a plain union: D2 on anonymous same-class, NML2051 on a
        // bogus annotation, and full body validation on a valid one. Previously
        // Modifier(Union) fell to the wildcard arm — zero diagnostics.
        let schema = "model modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel mhost:\n    |slot (modelA | modelB)?\n";
        let anonymous = diags_for(schema, "mhost H:\n    slot:\n        a = \"x\"\n");
        assert!(
            anonymous
                .iter()
                .any(|d| d.code == Some(codes::AMBIGUOUS_UNION_INSTANCE)),
            "modifier-wrapped same-class anonymous must D2: {anonymous:?}"
        );
        let bogus = diags_for(schema, "mhost H:\n    slot as gamma:\n        a = \"x\"\n");
        assert!(
            bogus
                .iter()
                .any(|d| d.code == Some(codes::UNKNOWN_UNION_VARIANT)),
            "modifier-wrapped bogus annotation must NML2051: {bogus:?}"
        );
        let wrong_body = diags_for(schema, "mhost H:\n    slot as modelB:\n        a = \"x\"\n");
        assert!(
            wrong_body
                .iter()
                .any(|d| d.message.contains("unknown property")),
            "modifier-wrapped valid annotation must VALIDATE the body: {wrong_body:?}"
        );
    }

    /// F4 (interaction audit): a BLOCK-form shared property under a union
    /// element type must have its content validated (subset semantics — valid
    /// for at least one declaring variant). Previously only scalar shared
    /// props were checked; block content shipped to consumers unvalidated.
    #[test]
    fn union_block_shared_property_content_is_validated() {
        let schema = "model sub:\n    x string?\nmodel modelA:\n    sub sub?\nmodel modelB:\n    sub sub?\n    b string?\nmodel host:\n    slots [](modelA | modelB)?\n";
        let bad = diags_for(
            schema,
            "host H:\n    slots:\n        .sub:\n            bogus = \"v\"\n        - one as modelB:\n            b = \"x\"\n",
        );
        assert!(
            bad.iter().any(|d| d.message.contains("unknown property")),
            "bogus block shared-prop content under a union must be flagged: {bad:?}"
        );
        let good = diags_for(
            schema,
            "host H:\n    slots:\n        .sub:\n            x = \"v\"\n        - one as modelB:\n            b = \"x\"\n",
        );
        assert!(
            good.is_empty(),
            "valid block shared-prop content must stay clean: {good:?}"
        );
    }

    /// Round 9: annotation rules reach MODIFIER block lists too — a stray `as`
    /// on a `|allow:`-style item is flagged, never silently carried (the last
    /// Body-bearing grammar position in the enumeration).
    #[test]
    fn stray_annotation_on_modifier_block_item_is_flagged() {
        let schema = "model host:\n    |allow []string?\n";
        let diags = diags_for(
            schema,
            "host H:\n    |allow:\n        - one as modelB:\n            x = \"v\"\n",
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::STRAY_TYPE_ANNOTATION)),
            "a stray annotation on a modifier block item must be flagged: {diags:?}"
        );
    }

    /// Round-10 F5: a block shared property with NO block-capable declaring
    /// variant (all declarers scalar) is a type mismatch — never a silent
    /// accept; and a UNION-typed declarer resolves body-aware (its model
    /// variant validates the content).
    #[test]
    fn union_block_shared_property_without_block_capable_declarer_errors() {
        // Both variants declare `sub` as a SCALAR: a block can't fill it.
        let scalar_only = "model modelA:\n    sub string?\nmodel modelB:\n    sub string?\nmodel host:\n    slots [](modelA | modelB)?\n";
        let diags = diags_for(
            scalar_only,
            "host H:\n    slots:\n        .sub:\n            bogus = \"v\"\n        - one as modelB:\n            sub = \"s\"\n",
        );
        assert!(
            diags.iter().any(|d| d.code == Some(codes::TYPE_MISMATCH)),
            "a block on scalar-only declarers must be a type mismatch: {diags:?}"
        );
        // A union-typed declarer: `(sub | other)` resolves the block's shape to
        // its model variant, and the content validates against it.
        let union_declarer = "model sub:\n    x string?\nmodel other:\n    y string?\nmodel modelA:\n    sub (sub | other)?\nmodel modelB:\n    b string?\nmodel host:\n    slots [](modelA | modelB)?\n";
        let bad = diags_for(
            union_declarer,
            "host H:\n    slots:\n        .sub:\n            bogus = \"v\"\n        - one as modelB:\n            b = \"s\"\n",
        );
        assert!(
            bad.iter().any(|d| d.message.contains("unknown property")),
            "a union-typed declarer must validate block content body-aware: {bad:?}"
        );
    }

    /// Round-13 F1: "block-capable" means anything a body can validate against
    /// — a list variant selected by item-shaped content, a plain `[]sub`
    /// declarer, and a oneof declarer are ALL legal (previously Model-only,
    /// which false-errored TYPE_MISMATCH on each).
    #[test]
    fn union_block_shared_property_accepts_list_and_oneof_declarers() {
        // (a) union declarer whose LIST variant matches the item-shaped block.
        let list_variant = "model sub:\n    x string?\nmodel modelA:\n    sub (sub | []sub)?\nmodel modelB:\n    b string?\nmodel host:\n    slots [](modelA | modelB)?\n";
        let d = diags_for(
            list_variant,
            "host H:\n    slots:\n        .sub:\n            - i:\n                x = \"v\"\n        - one as modelB:\n            b = \"s\"\n",
        );
        assert!(
            d.is_empty(),
            "an item-shaped block matching the list variant is legal: {d:?}"
        );
        // (b) plain `[]sub` declarers.
        let list_only = "model sub:\n    x string?\nmodel modelA:\n    sub []sub?\nmodel modelB:\n    sub []sub?\nmodel host:\n    slots [](modelA | modelB)?\n";
        let d2 = diags_for(
            list_only,
            "host H:\n    slots:\n        .sub:\n            - i:\n                x = \"v\"\n        - one as modelB:\n            sub:\n                - j:\n                    x = \"w\"\n",
        );
        assert!(
            d2.iter().all(|x| x.code != Some(codes::TYPE_MISMATCH)),
            "[]sub declarers are block-capable: {d2:?}"
        );
        // (c) oneof declarers with a keyed discriminated block.
        let oneof_decl = "model logM:\n    level string?\n\noneof mail by kind:\n    \"log\" -> logM\n\nmodel modelA:\n    sub mail?\nmodel modelB:\n    sub mail?\nmodel host:\n    slots [](modelA | modelB)?\n";
        let d3 = diags_for(
            oneof_decl,
            "host H:\n    slots:\n        .sub:\n            kind = \"log\"\n            level = \"info\"\n        - one as modelB:\n            sub:\n                kind = \"log\"\n",
        );
        assert!(
            d3.iter().all(|x| x.code != Some(codes::TYPE_MISMATCH)),
            "oneof declarers are block-capable: {d3:?}"
        );
    }

    /// Round-14: `object`-typed declarers are block-capable (free-form) —
    /// symmetric with the non-union path, no false TYPE_MISMATCH.
    #[test]
    fn union_block_shared_property_accepts_object_declarers() {
        let schema = "model modelA:\n    sub object?\nmodel modelB:\n    sub object?\nmodel host:\n    slots [](modelA | modelB)?\n";
        let d = diags_for(
            schema,
            "host H:\n    slots:\n        .sub:\n            anything = \"v\"\n        - one as modelB:\n            sub:\n                more = 1\n",
        );
        assert!(
            d.is_empty(),
            "object declarers accept free-form blocks: {d:?}"
        );
    }

    /// F4: D2 carries one mutually exclusive `Fix` per candidate — anchored at
    /// the NAME token (field header / Named item) — and NO fixes where `as`
    /// cannot be written in place (reference items steer to the block form).
    #[test]
    fn d2_carries_anchored_fix_alternatives() {
        use nml_core::diagnostic::SuggestionKind;
        // Field header: two fixes replacing the name token.
        let diags = diags_for(SAME_CLASS_SCHEMA, "host H:\n    slot:\n        a = \"x\"\n");
        let d2 = diags
            .iter()
            .find(|d| d.code == Some(codes::AMBIGUOUS_UNION_INSTANCE))
            .expect("D2");
        let fixes: Vec<&str> = d2
            .suggestions
            .iter()
            .filter(|s| s.kind == SuggestionKind::Fix)
            .map(|s| s.replacement.as_str())
            .collect();
        assert_eq!(
            fixes,
            vec!["slot as modelA", "slot as modelB"],
            "one fix per candidate, name-anchored: {d2:?}"
        );

        // Named element: fixes anchored at the ITEM name token.
        let diags = diags_for(
            SAME_CLASS_SCHEMA,
            "host H:\n    slots:\n        - one:\n            a = \"x\"\n",
        );
        let d2 = diags
            .iter()
            .find(|d| d.code == Some(codes::AMBIGUOUS_UNION_INSTANCE))
            .expect("element D2");
        let fixes: Vec<&str> = d2
            .suggestions
            .iter()
            .filter(|s| s.kind == SuggestionKind::Fix)
            .map(|s| s.replacement.as_str())
            .collect();
        assert_eq!(fixes, vec!["one as modelA", "one as modelB"]);

        // Reference item: D2 still fires, but NO fixes (unwritable in place)
        // and the message steers to the block form.
        let diags = diags_for(
            SAME_CLASS_SCHEMA,
            "host H:\n    slots:\n        - SomeRef\n",
        );
        let d2 = diags
            .iter()
            .find(|d| d.code == Some(codes::AMBIGUOUS_UNION_INSTANCE))
            .expect("reference-item D2");
        assert!(
            d2.suggestions.is_empty(),
            "no in-place fix is expressible for a reference item: {d2:?}"
        );
        assert!(
            d2.message.contains("block form"),
            "the message steers to the block form: {}",
            d2.message
        );
    }

    #[test]
    fn disjoint_union_never_triggers_d2() {
        // The shipping `(step | []step)` shape has ONE nameable variant, so a
        // keyed body is unambiguous — D2 must NOT fire (no regression).
        let schema = "model step:\n    emit string?\n    parallel (step | []step)?\n";
        let diags = diags_for(schema, "step Fork:\n    parallel:\n        emit = \"x\"\n");
        assert!(
            diags
                .iter()
                .all(|d| d.code != Some(codes::AMBIGUOUS_UNION_INSTANCE)),
            "disjoint union must never trigger D2: {diags:?}"
        );
    }

    #[test]
    fn test_circular_model_ref_no_infinite_recursion() {
        let schema = "model nodeA:\n    name string\n    child nodeB?\n\nmodel nodeB:\n    name string\n    parent nodeA?\n";
        let validator = make_validator(schema);

        let source = "nodeA Root:\n    name = \"root\"\n    child:\n        name = \"leaf\"\n        parent:\n            name = \"back-ref\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("unknown property 'name'")),
            "circular models should validate without infinite recursion; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_self_referencing_model_no_infinite_recursion() {
        let schema = "model tree:\n    value string\n    left tree?\n    right tree?\n";
        let validator = make_validator(schema);

        let source = "tree Root:\n    value = \"root\"\n    left:\n        value = \"left\"\n    right:\n        value = \"right\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.is_empty(),
            "self-referencing model should validate without infinite recursion; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_deeply_nested_circular_models_validates_without_hang() {
        let schema = "model nodeA:\n    name string\n    child nodeB?\n\nmodel nodeB:\n    name string\n    parent nodeA?\n";
        let validator = make_validator(schema);

        // Build deeply nested alternating A/B instances
        let source = r#"nodeA Root:
    name = "r"
    child:
        name = "c1"
        parent:
            name = "p1"
            child:
                name = "c2"
                parent:
                    name = "p2"
                    child:
                        name = "c3"
                        parent:
                            name = "p3"
"#;
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let start = std::time::Instant::now();
        let _diags = validator.validate(&file);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 1000,
            "deep circular nesting validation should complete in <1s; took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_circular_and_self_referencing_mixed() {
        let schema = "model node:\n    value string\n    self_ref node?\n    partner peer?\n\nmodel peer:\n    name string\n    back node?\n";
        let validator = make_validator(schema);

        let source = "node N:\n    value = \"hello\"\n    self_ref:\n        value = \"nested\"\n    partner:\n        name = \"p\"\n        back:\n            value = \"back\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        // Should validate without crashing or hanging
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("unknown property 'value'")),
            "mixed circular + self-ref models should validate correctly; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_validation_catches_errors_in_circular_models() {
        let schema = "model nodeA:\n    name string\n    child nodeB?\n\nmodel nodeB:\n    name string\n    parent nodeA?\n";
        let validator = make_validator(schema);

        let source = "nodeA Root:\n    name = \"root\"\n    child:\n        name = \"leaf\"\n        typo_field = \"bad\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown property 'typo_field'")),
            "should still catch errors inside circular model instances; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_large_schema_validation_performance() {
        let mut schema = String::new();
        for i in 0..30 {
            schema.push_str(&format!(
                "model type{}:\n    name string\n    ref type{}?\n\n",
                i,
                (i + 1) % 30
            ));
        }
        let validator = make_validator(&schema);

        let source = "type0 Instance:\n    name = \"test\"\n    ref:\n        name = \"nested\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let start = std::time::Instant::now();
        let _diags = validator.validate(&file);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 1000,
            "validation with 30-model circular schema should complete in <1s; took {:?}",
            elapsed
        );
    }

    // --- Role type validation tests ---

    #[test]
    fn test_role_type_quoted_string_warning() {
        let schema = "model service:\n    access role?\n";
        let validator = make_validator(schema);

        let source = "service Svc:\n    access = \"@public\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("roles are references, not strings")
                    && d.suggestions.first().map(|s| s.replacement.as_str()) == Some("@public")),
            "should suggest removing quotes for role field; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_role_type_unquoted_string_warning() {
        let schema = "model service:\n    access role?\n";
        let validator = make_validator(schema);

        let source = "service Svc:\n    access = \"admin\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("roles are references, not strings")
                    && d.suggestions.first().map(|s| s.replacement.as_str()) == Some("@admin")),
            "should suggest adding @ prefix for role field; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_role_type_valid_role_ref_ok() {
        let schema = "model service:\n    access role?\n";
        let validator = make_validator(schema);

        let source = "service Svc:\n    access = @public\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let role_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("role field"))
            .collect();
        assert!(
            role_diags.is_empty(),
            "valid role ref should not warn; diags: {:?}",
            role_diags
        );
    }

    #[test]
    fn test_role_type_in_array_string_warning() {
        let schema = "model service:\n    roles []role?\n";
        let validator = make_validator(schema);

        let source = "service Svc:\n    roles = [\"@admin\"]\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("roles are references, not strings")
                    && d.suggestions.first().map(|s| s.replacement.as_str()) == Some("@admin")),
            "should warn about quoted string in role array; diags: {:?}",
            diags
        );
    }

    // --- Unknown parent model tests ---

    #[test]
    fn test_unknown_parent_model() {
        let schema = "model base:\n    name string\n";
        let validator = make_validator(schema);

        let source = "model child is nonexistent:\n    value string\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown `is` target 'nonexistent'")
                    && d.code.map(|c| c.to_string()).as_deref() == Some("NML2020")),
            "should detect unknown parent model; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_known_parent_model_ok() {
        let schema = "model base:\n    name string\n";
        let validator = make_validator(schema);

        let source = "model child is base:\n    value string\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let extends_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("unknown `is` target"))
            .collect();
        assert!(
            extends_diags.is_empty(),
            "known parent should not produce errors; diags: {:?}",
            extends_diags
        );
    }

    #[test]
    fn test_multiple_unknown_parents() {
        let schema = "model base:\n    name string\n";
        let validator = make_validator(schema);

        let source = "model child is foo, bar:\n    value string\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown `is` target 'foo'")),
            "should detect 'foo' as unknown; diags: {:?}",
            diags
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown `is` target 'bar'")),
            "should detect 'bar' as unknown; diags: {:?}",
            diags
        );
    }

    // --- Circular member detection tests ---

    #[test]
    fn test_circular_member_detection() {
        let validator = make_validator_with_membership("");

        let source = "role Admin:\n    members:\n        - @role/Editor\n\nrole Editor:\n    members:\n        - @role/Admin\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let cycle = diags
            .iter()
            .find(|d| d.message.contains("circular membership"))
            .unwrap_or_else(|| panic!("should detect circular membership; diags: {diags:?}"));
        // Anchored at the declaration that opens the cycle (the LSP parity
        // invariant: every diagnostic carries a span — span-less warnings
        // would be silently dropped by the editor).
        assert!(cycle.span.is_some(), "{cycle:?}");
    }

    #[test]
    fn test_no_circular_members_ok() {
        let validator = make_validator_with_membership("");

        let source = "role Admin:\n    members:\n        - @role/Editor\n\nrole Editor:\n    members:\n        - @role/Viewer\n\nrole Viewer:\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let cycle_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("circular membership"))
            .collect();
        assert!(
            cycle_diags.is_empty(),
            "non-circular members should not warn; diags: {:?}",
            cycle_diags
        );
    }

    #[test]
    fn test_circular_member_in_array_decl() {
        let validator = make_validator_with_membership("");

        let source = "[]role roles:\n    - Admin:\n        members:\n            - @role/Editor\n    - Editor:\n        members:\n            - @role/Admin\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("circular membership")),
            "should detect circular membership in array declarations; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_self_referencing_member() {
        let validator = make_validator_with_membership("");

        let source = "role Admin:\n    members:\n        - @role/Admin\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("circular membership")),
            "should detect self-referencing membership; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_no_membership_no_cycle_check() {
        let validator = make_validator("");

        let source = "role Admin:\n    members:\n        - @role/Editor\n\nrole Editor:\n    members:\n        - @role/Admin\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("circular membership")),
            "without membership semantics, cycle detection should be off; diags: {:?}",
            diags
        );
    }

    // --- @user/ in access control tests ---

    fn nudge_membership() -> MembershipSemantics {
        MembershipSemantics {
            member_keywords: vec!["role".into(), "plan".into()],
            builtin_refs: vec!["@public".into(), "@authenticated".into()],
            user_ref_prefix: Some("@user/".into()),
        }
    }

    fn make_validator_with_membership(schema_source: &str) -> SchemaValidator {
        let schema = nml_core::cst::extract_schema(schema_source).0;
        SchemaValidator::new(schema.models, schema.enums, schema.oneofs)
            .with_membership_semantics(nudge_membership())
    }

    #[test]
    fn test_no_membership_semantics_accepts_all() {
        let validator = make_validator("");

        let source = "service Svc:\n    |allow = [@user/john]\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            !diags.iter().any(|d| d.message.contains("@user/")),
            "without membership semantics, @user/ should not be warned: {:?}",
            diags
        );
    }

    #[test]
    fn test_user_ref_in_allow_inline_warning() {
        let validator = make_validator_with_membership("");

        let source = "service Svc:\n    |allow = [@user/john]\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.iter().any(|d| d
                .message
                .contains("@user/ references are intended for members lists")),
            "should warn about @user/ in allow; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_user_ref_in_deny_block_warning() {
        let validator = make_validator_with_membership("");

        let source = "service Svc:\n    |deny:\n        - @user/john\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.iter().any(|d| d
                .message
                .contains("@user/ references are intended for members lists")),
            "should warn about @user/ in deny block; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_role_ref_in_allow_no_user_warning() {
        let validator = make_validator_with_membership("");

        let source = "service Svc:\n    |allow = [@role/admin]\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            !diags.iter().any(|d| d.message.contains("@user/")),
            "@role/ in allow should not trigger @user/ warning; diags: {:?}",
            diags
        );
    }

    // --- @public/@authenticated in members tests ---

    #[test]
    fn test_public_in_members_warning() {
        let validator = make_validator_with_membership("");

        let source = "role Admin:\n    members:\n        - @public\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.iter().any(|d| d
                .message
                .contains("built-in access levels should not appear in members lists")),
            "should warn about @public in members; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_authenticated_in_members_warning() {
        let validator = make_validator_with_membership("");

        let source = "role Admin:\n    members:\n        - @authenticated\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.iter().any(|d| d
                .message
                .contains("built-in access levels should not appear in members lists")),
            "should warn about @authenticated in members; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_public_in_allow_no_builtin_warning() {
        let validator = make_validator_with_membership("");

        let source = "service Svc:\n    |allow = [@public]\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("built-in access levels")),
            "@public in allow should not trigger members warning; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_builtin_in_plan_includes_warning() {
        let validator = make_validator_with_membership("");

        let source = "plan Pro:\n    includes:\n        - @public\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.iter().any(|d| d
                .message
                .contains("built-in access levels should not appear in members lists")),
            "should warn about @public in plan includes; diags: {:?}",
            diags
        );
    }

    // --- ModelRef bare identifier / string tests ---

    #[test]
    fn test_model_ref_accepts_bare_identifier() {
        let schema = "model step:\n    provider string?\n\nmodel workflow:\n    next step?\n    entrypoint step?\n";
        let validator = make_validator(schema);

        let source = "workflow W:\n    next = classify\n    entrypoint = start\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let type_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("type mismatch"))
            .collect();
        assert!(
            type_diags.is_empty(),
            "bare identifier should be accepted for ModelRef field; diags: {:?}",
            type_diags
        );
    }

    #[test]
    fn test_model_ref_accepts_string() {
        let schema = "model step:\n    provider string?\n\nmodel workflow:\n    next step?\n    entrypoint step?\n";
        let validator = make_validator(schema);

        let source = "workflow W:\n    next = \"classify\"\n    entrypoint = \"start\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let type_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("type mismatch"))
            .collect();
        assert!(
            type_diags.is_empty(),
            "string should be accepted for ModelRef field; diags: {:?}",
            type_diags
        );
    }

    #[test]
    fn test_model_ref_rejects_number() {
        let schema = "model step:\n    provider string?\n\nmodel workflow:\n    next step?\n";
        let validator = make_validator(schema);

        let source = "workflow W:\n    next = 42\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("expected step reference")),
            "number should be rejected for ModelRef field; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_model_ref_list_accepts_bare_identifiers() {
        let schema = "model tool:\n    wasm string?\n\nmodel workflow:\n    tools []tool?\n";
        let validator = make_validator(schema);

        let source = "workflow W:\n    tools = [myTool, anotherTool]\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let type_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("type mismatch"))
            .collect();
        assert!(
            type_diags.is_empty(),
            "bare identifiers in array should be accepted for ModelRef list; diags: {:?}",
            type_diags
        );
    }

    // --- Strict mode tests ---

    fn make_strict_validator(schema_source: &str) -> SchemaValidator {
        let schema = nml_core::cst::extract_schema(schema_source).0;
        SchemaValidator::new(schema.models, schema.enums, schema.oneofs).strict()
    }

    #[test]
    fn test_strict_unknown_property_is_error() {
        let schema = "model server:\n    port number?\n";
        let validator = make_strict_validator(schema);

        let source = "server S:\n    port = 3000\n    bogus = true\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let unknown = diags
            .iter()
            .find(|d| d.message.contains("unknown property 'bogus'"))
            .expect("should detect unknown property");
        assert!(
            matches!(unknown.severity, Severity::Error),
            "strict mode should emit Error, not Warning"
        );
    }

    #[test]
    fn test_nested_list_field_materializes_item_name() {
        // A `[]step` *field* written as `steps:\n - classify:` must materialize each
        // item's `name` from its key — exactly like a top-level array — so a required
        // shorthand `name` is not falsely reported missing. Regression guard for the
        // nudge workflow-step pattern (the `FieldType::List` arm once skipped this).
        let schema = "model step:\n    name string+\n    run string?\n    next step?\nmodel workflow:\n    entrypoint step\n    steps []step\n";
        let validator = make_strict_validator(schema);
        let source = "workflow W:\n    entrypoint = classify\n    steps:\n        - classify:\n            run = \"x\"\n            next = respond\n        - respond:\n            run = \"y\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("missing required field 'name'")),
            "nested list field must inject each item's name; diags: {diags:?}"
        );
        // A genuinely-missing *required non-identity* field is still caught,
        // proving the arm validates rather than skipping wholesale.
        let bad = "workflow W:\n    entrypoint = classify\n    steps:\n        - classify:\n            next = respond\n";
        let bad_file = nml_core::cst::parse_to_ast(bad).unwrap();
        let bad_diags = validator.validate(&bad_file);
        assert!(
            bad_diags.is_empty()
                || !bad_diags
                    .iter()
                    .any(|d| d.message.contains("missing required field 'name'")),
            "name is supplied by the key, never missing; diags: {bad_diags:?}"
        );
    }

    #[test]
    fn test_default_unknown_property_is_warning() {
        let schema = "model server:\n    port number?\n";
        let validator = make_validator(schema);

        let source = "server S:\n    port = 3000\n    bogus = true\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let unknown = diags
            .iter()
            .find(|d| d.message.contains("unknown property 'bogus'"))
            .expect("should detect unknown property");
        assert!(
            matches!(unknown.severity, Severity::Warning),
            "default mode should emit Warning"
        );
    }

    #[test]
    fn test_strict_unmodeled_block_keyword_is_error() {
        let schema = "model server:\n    port number?\n";
        let validator = make_strict_validator(schema);

        let source = "bogusBlock Thing:\n    key = \"value\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.iter().any(|d| d
                .message
                .contains("block keyword 'bogusBlock' has no model definition")),
            "strict mode should reject unmodeled block keyword; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_default_unmodeled_block_keyword_silent() {
        let schema = "model server:\n    port number?\n";
        let validator = make_validator(schema);

        let source = "bogusBlock Thing:\n    key = \"value\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("has no model definition")),
            "default mode should not flag unmodeled block; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_strict_unmodeled_array_keyword_with_named_items() {
        let schema = "model server:\n    port number?\n";
        let validator = make_strict_validator(schema);

        let source = "[]bogus items:\n    - Item1:\n        key = \"value\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.iter().any(|d| d
                .message
                .contains("array item keyword 'bogus' has no model or oneof definition")),
            "strict mode should reject unmodeled array with named items; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_strict_unmodeled_array_keyword_with_scalar_items_is_ok() {
        // A scalar-only list under a label keyword (no model) is a valid list of
        // *values* (e.g. plugin-name strings) — even in strict mode it must NOT be
        // flagged as "no model definition". Regression guard for `[]plugin` lists.
        let validator = make_strict_validator("model server:\n    port number?\n");
        let file = nml_core::cst::parse_to_ast("[]plugin plugins:\n    - \"echo.v1\"\n").unwrap();
        let diags = validator.validate(&file);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("has no model or oneof definition")),
            "a scalar value list must not require a model definition; diags: {diags:?}"
        );
    }

    #[test]
    fn test_strict_shorthand_array_no_false_positive() {
        let schema = "model server:\n    port number?\n";
        let validator = make_strict_validator(schema);

        let source = "[]plugin plugins:\n    - \"echo\"\n    - \"telnyx\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("has no model definition")),
            "shorthand-only arrays should not trigger unmodeled diagnostic; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_strict_object_field_stays_permissive() {
        let schema = "model plugin:\n    wasm string\n    config object?\n";
        let validator = make_strict_validator(schema);

        let source = "plugin P:\n    wasm = \"echo.wasm\"\n    config:\n        anyKey = \"value\"\n        nested:\n            deep = true\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.is_empty(),
            "object? fields should accept arbitrary keys even in strict mode; diags: {:?}",
            diags
        );
    }

    // --- Union property type validation tests ---

    #[test]
    fn test_union_property_mismatch_reports_variants() {
        let schema = "model cfg:\n    value (string | number)\n";
        let validator = make_validator(schema);

        let source = "cfg C:\n    value = true\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("type mismatch for 'value'")
                    && d.message.contains("expected one of string, number")
                    && d.message.contains("got bool")),
            "union mismatch should name the expected variants; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_union_property_matching_variants_ok() {
        let schema = "model cfg:\n    value (string | number)\n";
        let validator = make_validator(schema);

        for source in ["cfg C:\n    value = \"text\"\n", "cfg C:\n    value = 42\n"] {
            let file = nml_core::cst::parse_to_ast(source).unwrap();
            let diags = validator.validate(&file);
            assert!(
                diags.is_empty(),
                "matching union variant should pass for {source:?}; diags: {:?}",
                diags
            );
        }
    }

    #[test]
    fn test_union_property_with_list_variant() {
        let schema = "model cfg:\n    value (string | []number)\n";
        let validator = make_validator(schema);

        let file = nml_core::cst::parse_to_ast("cfg C:\n    value = [1, 2]\n").unwrap();
        assert!(validator.validate(&file).is_empty());

        let file = nml_core::cst::parse_to_ast("cfg C:\n    value = [true]\n").unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("expected one of string, []number")),
            "array of wrong element type should not match union; diags: {:?}",
            diags
        );
    }

    // --- RFC 0007: typed arm-set fields `(K -> V)` ---

    /// §4.3 shape rules at schema-definition time: the type grammar parses
    /// arm sets under `[]`, inside other arm sets, and duplicated in a union
    /// — but none of those have an instance form, so declaring them is a
    /// schema error, not a silently-unvalidated field.
    #[test]
    fn arm_set_type_shapes_without_an_instance_form_are_rejected() {
        let diags_for = |schema: &str| {
            let file = nml_core::cst::parse_to_ast(schema).unwrap();
            make_validator(schema).validate(&file)
        };
        // Arms under an array — directly, and through a union.
        for schema in [
            "model m:\n    f [](role -> denial)?\n",
            "model m:\n    f [](string | (role -> denial))?\n",
        ] {
            let d = diags_for(schema);
            assert!(
                d.iter()
                    .any(|d| d.message.contains("cannot be an array element")),
                "{schema:?}: {d:?}"
            );
        }
        // Arms nested inside an arm set's target.
        let d = diags_for("model m:\n    f (role -> (plan -> x))?\n");
        assert!(
            d.iter().any(|d| d.message.contains("an arm-set target")),
            "{d:?}"
        );
        // A union with two arm-set variants — the second is unreachable.
        let d = diags_for("model m:\n    f ((role -> a) | (plan -> b))?\n");
        assert!(
            d.iter()
                .any(|d| d.message.contains("at most one arm-set variant")),
            "{d:?}"
        );
        // Arms anywhere in a MODIFIER's declared type — modifier values are
        // inline values or list blocks, so an arm body can never be written
        // under one (top-level and via a union).
        for schema in [
            "model m:\n    |gate (role -> denial)?\n",
            "model m:\n    |gate (string | (role -> denial))?\n",
        ] {
            let d = diags_for(schema);
            assert!(
                d.iter()
                    .any(|d| d.message.contains("a modifier's declared type")),
                "{schema:?}: {d:?}"
            );
        }
        // Shorthand (+) on an arm-set type is now SUPPORTED (RFC 0007 §4.3 ⑤:
        // the canonical `s ⇒ [else -> s]` fill) — bare and union-wrapped alike.
        for schema in [
            "model m:\n    f (role -> path)+\n",
            "model m:\n    f (string | (role -> denial))?\n",
            "model m:\n    f (string | (role -> denial))+\n",
            "model m:\n    f (role -> (a | b))\n",
            "model m:\n    f []string?\n    g (a | []b)?\n",
            "model m:\n    |allow []role?\n    f string?\n",
            "model m:\n    f string+\n    g (role -> denial)?\n",
        ] {
            let d = diags_for(schema);
            assert!(d.is_empty(), "{schema:?} must be clean: {d:?}");
        }
    }

    /// The full happy path: a `(string | (role -> denial))?` union accepts the
    /// scalar form, and an arm-shaped body selects the arm-set variant and
    /// validates cleanly — including a REFERENCE target that resolves nowhere
    /// in this file (negative existence-check pin, §4.1: consumer-resolved,
    /// cross-scope refs must not false-positive).
    #[test]
    fn arm_set_union_accepts_scalar_and_arm_forms() {
        let schema = "model mount:\n    path string\n    denial (string | (role -> denial))?\n";
        let scalar = diags(
            schema,
            "mount M:\n    path = \"/x\"\n    denial = \"ProUpsell\"\n",
        );
        assert!(scalar.is_empty(), "scalar form: {scalar:?}");

        let arms = diags(
            schema,
            "mount M:\n    path = \"/x\"\n    denial:\n        @plan/Pro -> ProUpsell\n        else -> Generic\n",
        );
        assert!(
            arms.is_empty(),
            "arm form (with an unresolvable reference target) must validate: {arms:?}"
        );
    }

    /// §4.3: `else` is single and last — a duplicate or non-final `else` errors.
    #[test]
    fn arm_set_else_must_be_single_and_last() {
        let schema = "model mount:\n    denial (role -> denial)?\n";
        let after_else = diags(
            schema,
            "mount M:\n    denial:\n        else -> Generic\n        @plan/Pro -> ProUpsell\n",
        );
        assert!(
            after_else.iter().any(|d| d.message.contains("unreachable")
                && d.message.contains("'else' must be the final arm")),
            "an arm after 'else' is dead code: {after_else:?}"
        );

        let dup_else = diags(
            schema,
            "mount M:\n    denial:\n        else -> A\n        else -> B\n",
        );
        assert!(
            dup_else
                .iter()
                .any(|d| d.message.contains("duplicate 'else' arm")),
            "{dup_else:?}"
        );
    }

    /// §4.3: exact-duplicate keys error; distinct keys pass (semantic overlap
    /// is the consumer's domain, not nml's).
    #[test]
    fn arm_set_duplicate_keys_error() {
        let schema = "model mount:\n    denial (role -> denial)?\n";
        let d = diags(
            schema,
            "mount M:\n    denial:\n        @plan/Pro -> A\n        @plan/Pro -> B\n",
        );
        assert!(
            d.iter()
                .any(|d| d.message.contains("duplicate arm key '@plan/Pro'")),
            "{d:?}"
        );
    }

    /// §4.3: a role selector only conforms to a `role` key type.
    #[test]
    fn arm_set_key_must_conform_to_declared_key_type() {
        let schema = "model mount:\n    handlers (string -> handler)?\n";
        let d = diags(schema, "mount M:\n    handlers:\n        @plan/Pro -> A\n");
        assert!(
            d.iter().any(|d| d.message.contains("does not conform")
                && d.message.contains("key type 'string'")),
            "{d:?}"
        );
    }

    /// RFC 0007 §6 arm targets: a string LITERAL (`-> "path"`) is legal only
    /// for a scalar-capable `V`; a reference (`-> Name`) is legal for any `V`
    /// (never existence-checked, §4.1). A literal where a model/oneof target
    /// is expected is a category error.
    #[test]
    fn arm_literal_targets_require_a_scalar_target_type() {
        // V = path → a literal path target validates; a reference is also fine.
        let ok = diags(
            "model route:\n    dispatch (role -> path)?\n",
            "route R:\n    dispatch:\n        @role/admin -> \"admin.workflow.nml\"\n        else -> Fallback\n",
        );
        assert!(
            ok.is_empty(),
            "literal + reference on a path target: {ok:?}"
        );

        // V = a oneof (denial) → a literal target is a category error; a
        // reference is the natural form.
        let schema = "model denialCard:\n    title string?\noneof denial by kind = \"card\":\n    \"card\" -> denialCard\nmodel mount:\n    denial (role -> denial)?\n";
        let bad = diags(
            schema,
            "mount M:\n    denial:\n        @role/admin -> \"oops\"\n",
        );
        assert!(
            bad.iter().any(|d| d
                .message
                .contains("string-literal arm target requires a scalar")),
            "{bad:?}"
        );
        let good = diags(
            schema,
            "mount M:\n    denial:\n        @role/admin -> ProCard\n",
        );
        assert!(good.is_empty(), "reference target on a oneof V: {good:?}");
    }

    /// §4.2 placement: an arm inside a model-typed block (not arm-typed)
    /// errors instead of silently doing nothing.
    #[test]
    fn arm_outside_an_arm_typed_field_errors() {
        let schema =
            "model mount:\n    path string\n    pipeline pipe?\nmodel pipe:\n    input string?\n";
        let d = diags(
            schema,
            "mount M:\n    path = \"/x\"\n    pipeline:\n        @plan/Pro -> A\n",
        );
        assert!(
            d.iter()
                .any(|d| d.message.contains("routing arms are not allowed here")),
            "{d:?}"
        );
    }

    /// A non-arm entry inside an arm-typed block errors (the type says the
    /// body holds only arms).
    #[test]
    fn arm_set_rejects_non_arm_entries() {
        let schema = "model mount:\n    denial (role -> denial)?\n";
        let d = diags(schema, "mount M:\n    denial:\n        title = \"nope\"\n");
        assert!(
            d.iter()
                .any(|d| d.message.contains("expected a routing arm")),
            "{d:?}"
        );
    }

    /// A scalar value on an arms-only (non-union) field is a type mismatch.
    #[test]
    fn arm_set_scalar_value_mismatch() {
        let schema = "model mount:\n    denial (role -> denial)?\n";
        let d = diags(schema, "mount M:\n    denial = 42\n");
        assert!(
            d.iter()
                .any(|d| d.message.contains("expected an arm block")),
            "{d:?}"
        );
    }

    // --- List type validation tests ---

    #[test]
    fn test_list_field_non_array_value_is_error() {
        let schema = "model svc:\n    tags []string\n";
        let validator = make_validator(schema);

        let source = "svc S:\n    tags = \"oops\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("type mismatch for 'tags'")
                    && d.message.contains("expected []string, got string")),
            "non-array value for list field should be an error; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_list_field_reference_value_ok() {
        let schema = "model svc:\n    tags []string\n";
        let validator = make_validator(schema);

        let source = "svc S:\n    tags = sharedTags\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let type_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("type mismatch"))
            .collect();
        assert!(
            type_diags.is_empty(),
            "references may resolve to arrays and should pass; diags: {:?}",
            type_diags
        );
    }

    // --- Enum type mismatch tests ---

    #[test]
    fn test_enum_non_string_value_is_error() {
        let schema = "enum providerType:\n    - \"openai\"\n    - \"groq\"\n\nmodel provider:\n    type providerType\n";
        let validator = make_validator(schema);

        let source = "provider P:\n    type = 42\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("type mismatch for 'type'")
                    && d.message.contains("expected one of \"openai\", \"groq\"")
                    && d.message.contains("got number")),
            "non-string enum value should be a type error; diags: {:?}",
            diags
        );
    }

    // --- Depth truncation tests ---

    #[test]
    fn test_depth_truncation_emits_diagnostic() {
        let schema = "model tree:\n    child tree?\n";
        let validator = make_validator(schema);

        let span = Span::empty(0);
        let mut body = Body::fresh(vec![]);
        for _ in 0..(MAX_VALIDATION_DEPTH + 4) {
            body = Body::fresh(vec![BodyEntry {
                kind: BodyEntryKind::NestedBlock(NestedBlock {
                    name: Identifier::new("child", span),
                    body,
                }),
                span,
            }]);
        }
        let file = File {
            declarations: vec![Declaration {
                kind: DeclarationKind::Block(BlockDecl {
                    keyword: Identifier::new("tree", span),
                    name: Identifier::new("Root", span),
                    extends: vec![],
                    body,
                }),
                span,
            }],
        };

        let diags = validator.validate(&file);
        let truncated = diags
            .iter()
            .find(|d| d.message.contains("validation truncated"))
            .expect("hitting the depth limit should emit a diagnostic");
        assert!(
            matches!(truncated.severity, Severity::Warning),
            "truncation should be a warning"
        );
    }

    #[test]
    fn test_shallow_nesting_no_truncation_diagnostic() {
        let schema = "model tree:\n    child tree?\n    value string?\n";
        let validator = make_validator(schema);

        let source = "tree Root:\n    child:\n        value = \"leaf\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("validation truncated")),
            "shallow nesting should not be truncated; diags: {:?}",
            diags
        );
    }

    // --- Typed modifier value validation tests ---

    #[test]
    fn test_modifier_inline_value_valid_ok() {
        let schema = "model plugin:\n    wasm string\n    |allow []string?\n";
        let validator = make_validator(schema);

        let source = "plugin P:\n    wasm = \"a.wasm\"\n    |allow = [\"fs:read\"]\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.is_empty(),
            "well-typed modifier value should pass; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_modifier_inline_value_type_mismatch() {
        let schema = "model plugin:\n    wasm string\n    |allow []string?\n";
        let validator = make_validator(schema);

        let source = "plugin P:\n    wasm = \"a.wasm\"\n    |allow = [42]\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("type mismatch in array 'allow'")
                    && d.message.contains("expected string, got number")),
            "mistyped modifier array element should be an error; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_modifier_block_value_type_mismatch() {
        let schema = "model plugin:\n    wasm string\n    |caps []number?\n";
        let validator = make_validator(schema);

        let source = "plugin P:\n    wasm = \"a.wasm\"\n    |caps:\n        - \"high\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("type mismatch in array 'caps'")
                    && d.message.contains("expected number, got string")),
            "mistyped modifier block item should be an error; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_modifier_block_value_for_scalar_type_mismatch() {
        let schema = "model svc:\n    name string\n    |limit number?\n";
        let validator = make_validator(schema);

        let source = "svc S:\n    name = \"s\"\n    |limit:\n        - \"high\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("type mismatch for 'limit'")
                    && d.message.contains("expected number, got array")),
            "block value for scalar modifier should be an error; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_modifier_scalar_value_type_mismatch() {
        let schema = "model svc:\n    name string\n    |limit number?\n";
        let validator = make_validator(schema);

        let source = "svc S:\n    name = \"s\"\n    |limit = \"high\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("type mismatch for 'limit'")
                    && d.message.contains("expected number, got string")),
            "mistyped scalar modifier should be an error; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_modifier_role_list_accepts_roles() {
        let schema = "model svc:\n    name string\n    |allow []role?\n";
        let validator = make_validator(schema);

        let source = "svc S:\n    name = \"s\"\n    |allow = [@public]\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.is_empty(),
            "role refs should match a []role modifier; diags: {:?}",
            diags
        );
    }

    // --- Missing-required-field span tests ---

    #[test]
    fn test_missing_required_span_points_at_block_name() {
        let schema = "model mount:\n    path string\n    wasm string?\n";
        let validator = make_validator(schema);

        let source = "mount Test:\n    wasm = \"handler.wasm\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let missing = diags
            .iter()
            .find(|d| d.message.contains("missing required field 'path'"))
            .expect("should report missing required field");
        let span = missing.span.expect("diagnostic should carry a span");
        assert_eq!(
            &source[span.start..span.end],
            "Test",
            "missing-required diagnostic should point at the block name"
        );
    }

    // --- structured modifier type tests ---

    #[test]
    fn test_modifier_field_type_is_structured() {
        // `schema` must produce a structured inner type for typed
        // modifiers, including nested lists and unions -- no string
        // round-trip involved.
        let schema = "model route:\n    |allow []string?\n    |variant (step | []step)?\n";
        let extracted = nml_core::cst::extract_schema(schema).0;
        let model = &extracted.models[0];

        let FieldType::Modifier(inner) = &model.fields[0].field_type else {
            panic!("expected modifier type for |allow");
        };
        let FieldType::List(elem) = inner.as_ref() else {
            panic!("expected list inside modifier");
        };
        assert!(matches!(
            elem.as_ref(),
            FieldType::Primitive(PrimitiveType::String)
        ));

        let FieldType::Modifier(inner) = &model.fields[1].field_type else {
            panic!("expected modifier type for |variant");
        };
        let FieldType::Union(variants) = inner.as_ref() else {
            panic!("expected union inside modifier");
        };
        assert_eq!(variants.len(), 2);
        assert!(matches!(&variants[0], FieldType::ModelRef(n) if n == "step"));
        assert!(matches!(&variants[1], FieldType::List(_)));
    }

    #[test]
    fn test_strict_nested_unknown_property_is_error() {
        let schema = "model prompt:\n    system string?\n\nmodel step:\n    prompt prompt?\n";
        let validator = make_strict_validator(schema);

        let source = "step S:\n    prompt:\n        systm = \"typo\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let unknown = diags
            .iter()
            .find(|d| d.message.contains("unknown property 'systm'"))
            .expect("should detect unknown nested property");
        assert!(
            matches!(unknown.severity, Severity::Error),
            "strict mode should emit Error for nested unknown properties"
        );
    }

    // ---- oneof (discriminated union) validation ----

    const ONEOF_SCHEMA: &str = concat!(
        "model emailLog:\n    fromAddress string?\n\n",
        "model emailPostmark:\n    fromAddress string?\n    serverToken secret\n\n",
        "oneof email by provider:\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n\n",
        "model server:\n    email email?\n\n",
        "model providers:\n    items []email?\n",
    );

    fn oneof_errors(source: &str) -> Vec<String> {
        let validator = make_strict_validator(ONEOF_SCHEMA);
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        validator
            .validate(&file)
            .into_iter()
            .filter(|d| matches!(d.severity, Severity::Error))
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn test_oneof_block_keyword_valid_variant() {
        let errs = oneof_errors(
            "email Cfg:\n    provider = \"postmark\"\n    fromAddress = \"a@b.co\"\n    serverToken = $ENV.TOK\n",
        );
        assert!(
            errs.is_empty(),
            "valid postmark variant should pass: {errs:?}"
        );
    }

    #[test]
    fn test_oneof_rejects_cross_variant_field() {
        // serverToken belongs to the postmark variant, not log.
        let errs = oneof_errors("email Cfg:\n    provider = \"log\"\n    serverToken = $ENV.TOK\n");
        assert!(
            errs.iter()
                .any(|m| m.contains("unknown property 'serverToken'")),
            "log variant must reject postmark-only field: {errs:?}"
        );
    }

    #[test]
    fn test_oneof_missing_discriminator() {
        let errs = oneof_errors("email Cfg:\n    fromAddress = \"a@b.co\"\n");
        assert!(
            errs.iter()
                .any(|m| m.contains("missing discriminator 'provider'")),
            "missing discriminator should be flagged: {errs:?}"
        );
    }

    #[test]
    fn test_oneof_unknown_discriminator_value() {
        let errs = oneof_errors("email Cfg:\n    provider = \"sendgrid\"\n");
        assert!(
            errs.iter()
                .any(|m| m.contains("unknown provider \"sendgrid\"")),
            "unknown discriminator value should be flagged: {errs:?}"
        );
    }

    #[test]
    fn test_oneof_enforces_variant_required_field() {
        // postmark requires serverToken.
        let errs = oneof_errors("email Cfg:\n    provider = \"postmark\"\n");
        assert!(
            errs.iter()
                .any(|m| m.contains("missing required field 'serverToken'")),
            "postmark variant must enforce serverToken: {errs:?}"
        );
    }

    #[test]
    fn test_oneof_nested_block_ref_context() {
        let errs = oneof_errors(
            "server S:\n    email:\n        provider = \"postmark\"\n        serverToken = $ENV.TOK\n",
        );
        assert!(
            errs.is_empty(),
            "oneof referenced as a nested-block field should validate: {errs:?}"
        );
        let bad = oneof_errors(
            "server S:\n    email:\n        provider = \"log\"\n        serverToken = $ENV.TOK\n",
        );
        assert!(
            bad.iter()
                .any(|m| m.contains("unknown property 'serverToken'")),
            "nested oneof must enforce per-variant fields: {bad:?}"
        );
    }

    #[test]
    fn test_oneof_top_level_array_context() {
        // A top-level `[]<oneof>` declaration validates each named item against
        // the union (parity with the block-keyword surface).
        let errs = oneof_errors(
            "[]email mailers:\n    - primary:\n        provider = \"postmark\"\n        serverToken = $ENV.TOK\n    - fallback:\n        provider = \"log\"\n",
        );
        assert!(
            errs.is_empty(),
            "top-level []oneof should validate per-variant: {errs:?}"
        );
        let bad = oneof_errors(
            "[]email mailers:\n    - primary:\n        provider = \"log\"\n        serverToken = $ENV.TOK\n",
        );
        assert!(
            bad.iter()
                .any(|m| m.contains("unknown property 'serverToken'")),
            "top-level []oneof must enforce per-variant fields: {bad:?}"
        );
    }

    #[test]
    fn test_oneof_list_context() {
        let errs = oneof_errors(
            "providers P:\n    items:\n        - log:\n            provider = \"log\"\n        - pm:\n            provider = \"postmark\"\n            serverToken = $ENV.TOK\n",
        );
        assert!(
            errs.is_empty(),
            "[]oneof list items should validate per-variant: {errs:?}"
        );
    }

    #[test]
    fn oneof_omitted_discriminator_with_default_validates() {
        // A `oneof` with a default arm: omitting the discriminator is valid (the
        // defaulter injects it), so the validator must agree and check the default
        // variant rather than reporting a missing discriminator.
        let schema = "model emailLog:\n    level string?\n\nmodel emailPostmark:\n    serverToken string\n\noneof email by provider = \"log\":\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n";
        let validator = make_validator(schema);
        let doc = nml_core::cst::parse_to_ast("email Outbound:\n    level = \"info\"\n").unwrap();
        let errors: Vec<_> = validator
            .validate(&doc)
            .into_iter()
            .filter(|d| matches!(d.severity, Severity::Error))
            .collect();
        assert!(
            errors.is_empty(),
            "omitted discriminator with a default should validate: {errors:?}"
        );
    }

    #[test]
    fn oneof_omitted_discriminator_without_default_still_errors() {
        // Without a default, an omitted discriminator remains an error.
        let schema = "model emailLog:\n    level string?\n\noneof email by provider:\n    \"log\" -> emailLog\n";
        let validator = make_validator(schema);
        let doc = nml_core::cst::parse_to_ast("email Outbound:\n    level = \"info\"\n").unwrap();
        assert!(
            validator
                .validate(&doc)
                .iter()
                .any(|d| d.message.contains("missing discriminator")),
            "omitted discriminator without a default must error"
        );
    }

    #[test]
    fn type_mismatched_default_is_rejected() {
        let src = "model cfg:\n    count number = \"high\"\n";
        let file = nml_core::cst::parse_to_ast(src).unwrap();
        let schema = nml_core::cst::extract_schema(src).0;
        let validator = SchemaValidator::new(schema.models, schema.enums, schema.oneofs);
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("as the default for") && d.message.contains("count")),
            "expected a default type-mismatch diagnostic; got {diags:?}"
        );
    }

    #[test]
    fn valid_typed_defaults_pass() {
        // duration takes a duration literal (RFC 0017 — defaults run the
        // same value decoder instances do); an `$ENV` secret default is
        // lenient; a numeric default matches a number field — all reuse
        // the value check.
        let src = "model cfg:\n    sessionDuration duration = 24h\n    apiKey secret = $ENV.KEY\n    retries number = 3\n";
        let file = nml_core::cst::parse_to_ast(src).unwrap();
        let schema = nml_core::cst::extract_schema(src).0;
        let validator = SchemaValidator::new(schema.models, schema.enums, schema.oneofs);
        let errors: Vec<_> = validator
            .validate(&file)
            .into_iter()
            .filter(|d| matches!(d.severity, Severity::Error))
            .collect();
        assert!(
            errors.is_empty(),
            "valid typed defaults should pass: {errors:?}"
        );
    }

    #[test]
    fn inherited_default_not_double_reported() {
        // A bad default on a parent is reported once (on the parent), not again
        // on each child that inherits it.
        let src = "model base:\n    count number = \"high\"\n\nmodel child is base:\n    extra string = \"x\"\n";
        let file = nml_core::cst::parse_to_ast(src).unwrap();
        let mut schema = nml_core::cst::extract_schema(src).0;
        nml_core::schema::resolve_model_inheritance(&mut schema);
        let validator = SchemaValidator::new(schema.models, schema.enums, schema.oneofs);
        let count = validator
            .validate(&file)
            .iter()
            .filter(|d| d.message.contains("as the default for") && d.message.contains("count"))
            .count();
        assert_eq!(
            count, 1,
            "inherited bad default must be reported exactly once"
        );
    }

    /// Schema DEFAULTS are set-checked too: a default carrying duplicate
    /// elements is a schema-load error (a schema shipping a bad default would
    /// otherwise poison every instance).
    #[test]
    fn set_default_with_duplicates_is_rejected_at_schema_load() {
        let check = |src: &str| {
            let file = nml_core::cst::parse_to_ast(src).unwrap();
            make_validator(src).validate(&file)
        };
        let d = check("model m:\n    xs set<string> = [\"a\", \"a\"]\n");
        assert!(
            d.iter()
                .any(|d| d.message.contains("duplicate set element")),
            "duplicate default elements must be rejected: {:?}",
            d.iter().map(|x| &x.message).collect::<Vec<_>>()
        );
        let ok = check("model m:\n    xs set<string> = [\"a\", \"b\"]\n");
        assert!(
            !ok.iter().any(|d| d.message.contains("duplicate")),
            "unique defaults are legal: {ok:?}"
        );
    }

    /// The P1-critical shape: a MODIFIER field declared as a set
    /// (`|block set<string>?` — nudge's reloadable egress denylist) accepts
    /// block-form items, enforces uniqueness on them, and keeps working
    /// inline. Before this fix, the Block arm required `List` and would have
    /// REJECTED the set-typed declaration outright.
    #[test]
    fn modifier_set_block_form_accepts_and_dedups() {
        let schema = "model ceiling:\n    |block set<string>?\n";
        let dup = diags(
            schema,
            "ceiling c:\n    |block:\n        - \"10.0.0.0/8\"\n        - \"10.9.0.0/16\"\n        - \"10.0.0.0/8\"\n",
        );
        assert!(
            dup.iter()
                .any(|d| d.message.contains("duplicate set element")),
            "block-form modifier set must dedup: {:?}",
            dup.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        let ok = diags(
            schema,
            "ceiling c:\n    |block:\n        - \"10.0.0.0/8\"\n        - \"10.9.0.0/16\"\n",
        );
        assert!(
            !ok.iter()
                .any(|d| d.message.contains("duplicate") || d.message.contains("type mismatch")),
            "unique block-form items against a set declaration are legal: {:?}",
            ok.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        // Inline form flows through the Set arm of value validation.
        let inline_dup = diags(schema, "ceiling c:\n    |block = [\"a\", \"a\"]\n");
        assert!(
            inline_dup
                .iter()
                .any(|d| d.message.contains("duplicate set element")),
            "inline modifier set must dedup: {:?}",
            inline_dup.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    // ── RFC 0032: `set<T>` uniqueness ──

    const SET_SCHEMA: &str = "model server:\n    cidrs set<string>\n    order []string?\n";

    /// Inline-array form: a duplicate element is a load error at the second
    /// occurrence; unique elements pass; element typing still applies.
    #[test]
    fn set_inline_duplicates_are_rejected_and_unique_pass() {
        let dup = diags(
            SET_SCHEMA,
            "server s:\n    cidrs = [\"10.0.5.0/24\", \"10.0.9.0/24\", \"10.0.5.0/24\"]\n",
        );
        assert!(
            dup.iter()
                .any(|d| d.message.contains("duplicate set element")
                    && d.message.contains("10.0.5.0/24")),
            "duplicate must be rejected with its value named: {:?}",
            dup.iter().map(|d| &d.message).collect::<Vec<_>>()
        );

        let ok = diags(
            SET_SCHEMA,
            "server s:\n    cidrs = [\"10.0.5.0/24\", \"10.0.9.0/24\"]\n",
        );
        assert!(
            !ok.iter().any(|d| d.message.contains("duplicate")),
            "unique elements are legal: {:?}",
            ok.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    /// Body-form items: duplicates are caught across LINES (span-blind value
    /// identity — the exact cosmetic-difference case `semantic_eq` exists for).
    #[test]
    fn set_body_form_duplicates_are_rejected_span_blind() {
        let d = diags(
            SET_SCHEMA,
            "server s:\n    cidrs:\n        - \"10.0.5.0/24\"\n        - \"10.0.9.0/24\"\n        - \"10.0.5.0/24\"\n",
        );
        assert!(
            d.iter()
                .any(|d| d.message.contains("duplicate set element")),
            "same value on a different line is still a duplicate: {:?}",
            d.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    /// Control: a plain `[]T` list keeps allowing duplicates — uniqueness is
    /// the SET type's semantics, never a blanket list rule.
    #[test]
    fn plain_lists_still_allow_duplicates() {
        let d = diags(
            SET_SCHEMA,
            "server s:\n    cidrs = [\"a\"]\n    order = [\"x\", \"x\", \"x\"]\n",
        );
        assert!(
            !d.iter().any(|d| d.message.contains("duplicate")),
            "list duplicates are legal: {:?}",
            d.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod trait_instance_tests {
    //! RFC 0011 instance-side enforcement: traits cannot be instantiated,
    //! and the editor surface never suggests them as keywords.

    use super::*;
    use nml_core::diagnostic::Severity;

    fn loaded_validator(schema_source: &str) -> SchemaValidator {
        // Through the loader, so inheritance is resolved like production.
        let (schema, diags) = crate::loader::load_schema(&[("t.model.nml", schema_source)]);
        assert!(diags.is_empty(), "schema must load clean: {diags:?}");
        SchemaValidator::new(schema.models, schema.enums, schema.oneofs)
    }

    const SCHEMA: &str = "trait monitored:\n    timeout duration = \"5s\"\n\n\
                          model endpoint is monitored:\n    url string+\n";

    fn check(source: &str, strict: bool) -> Vec<Diagnostic> {
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let v = loaded_validator(SCHEMA);
        let v = if strict { v.strict() } else { v };
        v.validate(&file)
    }

    #[test]
    fn instantiating_a_trait_errors_even_in_lenient_mode() {
        let diags = check("monitored Probe:\n    timeout = \"9s\"\n", false);
        assert_eq!(diags.len(), 1, "{diags:?}");
        let d = &diags[0];
        assert_eq!(d.code.map(|c| c.to_string()).as_deref(), Some("NML2024"));
        assert_eq!(d.severity, Severity::Error);
        assert!(
            d.message.contains("cannot be instantiated"),
            "{}",
            d.message
        );
    }

    #[test]
    fn trait_as_array_item_keyword_errors() {
        let diags = check(
            "[]monitored probes:\n    - A:\n        timeout = \"9s\"\n",
            false,
        );
        assert_eq!(
            diags[0].code.map(|c| c.to_string()).as_deref(),
            Some("NML2024"),
            "{diags:?}"
        );
    }

    #[test]
    fn inherited_trait_field_is_validated_on_instances() {
        // Wrong kind for the trait-declared field: the model merged it, so
        // the validator types it.
        let diags = check(
            "endpoint api:\n    url = \"https://x.dev\"\n    timeout = 9\n",
            false,
        );
        assert!(
            diags.iter().any(
                |d| d.code.map(|c| c.to_string()).as_deref() == Some("NML2008")
                    && d.message.contains("'timeout'")
            ),
            "{diags:?}"
        );
    }

    #[test]
    fn strict_unknown_keyword_never_suggests_a_trait() {
        // "monitred" is nearest to trait "monitored" — but a trait is not a
        // legal keyword, so no suggestion is offered at all.
        let diags = check("monitred Probe:\n    timeout = \"9s\"\n", true);
        let d = diags
            .iter()
            .find(|d| d.code.map(|c| c.to_string()).as_deref() == Some("NML2004"))
            .expect("strict unknown-keyword diagnostic");
        assert!(d.suggestions.is_empty(), "{d:?}");
    }

    #[test]
    fn self_contained_file_is_targets_resolve_against_its_own_declarations() {
        // A checked file declaring both the trait and the model that mixes it
        // in must not be flagged against a foreign schema set (its
        // definitions are fully checked by the loader pipeline instead).
        let diags = check(
            "trait audited:\n    auditedBy string?\n\nmodel widget is audited:\n    name string?\n",
            false,
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn composition_checked_at_load_silences_the_in_file_twin() {
        // One finding, one owner: a caller that routes definitions through
        // the loader turns the validator's in-file `is` twin off.
        let source = "model child is nonexistent:\n    value string\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = loaded_validator(SCHEMA)
            .composition_checked_at_load()
            .validate(&file);
        assert!(
            diags
                .iter()
                .all(|d| d.code.map(|c| c.to_string()).as_deref() != Some("NML2020")),
            "{diags:?}"
        );
    }

    #[test]
    fn trait_declarations_in_checked_files_are_schema_defs() {
        // A `trait` block in a checked file is a schema definition, not an
        // unknown instance keyword — even under strict.
        let diags = check("trait audited:\n    auditedBy string?\n", true);
        assert!(diags.is_empty(), "{diags:?}");
    }
}

#[cfg(test)]
mod closed_vocabulary_tests {
    //! RFC 0012: a package-bound validator is a closed vocabulary — in-file
    //! definitions are inert and say so; tenant files can neither shadow nor
    //! extend the operator's schema set.

    use super::*;
    use nml_core::diagnostic::Severity;

    fn closed(strict: bool) -> SchemaValidator {
        let schema = nml_core::cst::extract_schema("model server:\n    port number\n").0;
        let v =
            SchemaValidator::new(schema.models, schema.enums, schema.oneofs).closed_vocabulary();
        if strict { v.strict() } else { v }
    }

    fn codes_of(diags: &[Diagnostic]) -> Vec<(String, Severity)> {
        diags
            .iter()
            .filter_map(|d| d.code.map(|c| (c.to_string(), d.severity.clone())))
            .collect()
    }

    #[test]
    fn in_file_definitions_draw_nml2026_warning_in_lenient() {
        let file = nml_core::cst::parse_to_ast("model smuggled:\n    x string?\n").unwrap();
        let diags = closed(false).validate(&file);
        assert_eq!(
            codes_of(&diags),
            vec![("NML2026".to_string(), Severity::Warning)],
            "{diags:?}"
        );
    }

    #[test]
    fn strict_closed_mode_errors_and_still_walls_the_minted_keyword() {
        // The vocabulary-extension attack: define a model, use its keyword.
        // The definition is refused AND the instance still hits strict's
        // unknown-keyword wall — the tenant gains nothing.
        let file = nml_core::cst::parse_to_ast(
            "model smuggled:\n    x string?\n\nsmuggled Foo:\n    x = \"boo\"\n",
        )
        .unwrap();
        let diags = closed(true).validate(&file);
        let codes: Vec<String> = diags
            .iter()
            .filter_map(|d| d.code.map(|c| c.to_string()))
            .collect();
        assert!(codes.contains(&"NML2026".to_string()), "{diags:?}");
        assert!(codes.contains(&"NML2004".to_string()), "{diags:?}");
        assert!(
            diags.iter().all(|d| d.severity == Severity::Error),
            "strict closed mode is all-errors: {diags:?}"
        );
    }

    #[test]
    fn oneof_and_trait_definitions_are_refused_too() {
        let file = nml_core::cst::parse_to_ast(
            "trait cap:\n    x string?\n\noneof n by kind:\n    \"a\" -> server\n",
        )
        .unwrap();
        let diags = closed(false).validate(&file);
        let n = diags
            .iter()
            .filter(|d| d.code.map(|c| c.to_string()).as_deref() == Some("NML2026"))
            .count();
        assert_eq!(n, 2, "one per definition kind: {diags:?}");
    }

    #[test]
    fn open_validators_never_emit_nml2026() {
        let schema = nml_core::cst::extract_schema("model server:\n    port number\n").0;
        let v = SchemaValidator::new(schema.models, schema.enums, schema.oneofs).strict();
        let file = nml_core::cst::parse_to_ast("model extra:\n    x string?\n").unwrap();
        assert!(
            v.validate(&file)
                .iter()
                .all(|d| d.code.map(|c| c.to_string()).as_deref() != Some("NML2026")),
            "open mode: definitions are legitimate"
        );
    }
}

#[cfg(test)]
mod duration_tests {
    //! Duration typing (RFC 0017): a duration-typed field holds a duration
    //! LITERAL; a quoted duration is the pre-literal spelling and gets the
    //! NML0001 migration fix; everything else is the ordinary NML2008
    //! mismatch. (NML2029, the old format check, is retired — format
    //! defects now surface at decode as NML3004/NML3005/NML3006.)

    use super::*;

    fn diags_for(value: &str) -> Vec<Diagnostic> {
        let schema = nml_core::cst::extract_schema("model job:\n    timeout duration\n").0;
        let v = SchemaValidator::new(schema.models, schema.enums, schema.oneofs);
        let src = format!("job Nightly:\n    timeout = {value}\n");
        v.validate(&nml_core::cst::parse_to_ast(&src).unwrap())
    }

    fn code_of(d: &Diagnostic) -> Option<String> {
        d.code.map(|c| c.to_string())
    }

    #[test]
    fn literals_type_check_and_strings_classify() {
        for ok in ["72h", "30m", "5s", "500ms", "0s"] {
            assert!(diags_for(ok).is_empty(), "{ok} is a typed duration");
        }
        // The migration: a QUOTED duration is NML0001 with the literal as
        // the machine-applicable fix (spanning the whole quoted string).
        let diags = diags_for("\"30s\"");
        let hit = diags
            .iter()
            .find(|d| code_of(d).as_deref() == Some("NML0001"))
            .expect("quoted duration is the replaced-syntax migration");
        assert_eq!(
            hit.suggestions.first().map(|s| s.replacement.as_str()),
            Some("30s")
        );
        // A string that is NOT duration text — and every other wrong kind —
        // is the ordinary type mismatch, never a special case.
        for bad in [
            "\"30x\"", "\"1.5h\"", "\"-3s\"", "\"s\"", "\"12\"", "30", "true",
        ] {
            let diags = diags_for(bad);
            assert!(
                diags
                    .iter()
                    .any(|d| code_of(d).as_deref() == Some("NML2008")),
                "{bad} must be NML2008: {diags:?}"
            );
        }
    }

    #[test]
    fn set_duplicates_are_semantic_with_clarifier() {
        // `30s` and `30000ms` are ONE value in two spellings (RFC 0017 §2)
        // — a set holding both is a duplicate, and the diagnostic says why
        // the two visibly different literals collide (the Number pattern).
        let schema = nml_core::cst::extract_schema("model job:\n    retries set<duration>\n").0;
        let v = SchemaValidator::new(schema.models, schema.enums, schema.oneofs);
        let src = "job Nightly:\n    retries = [30s, 30000ms]\n";
        let diags = v.validate(&nml_core::cst::parse_to_ast(src).unwrap());
        let hit = diags
            .iter()
            .find(|d| code_of(d).as_deref() == Some("NML2030"))
            .unwrap_or_else(|| panic!("semantic duplicate must be flagged: {diags:?}"));
        assert!(
            hit.message.contains("the same duration as '30s' above"),
            "clarifier names the earlier spelling: {}",
            hit.message
        );
    }
}

#[cfg(test)]
mod shared_property_validation_tests {
    //! 6c: shared properties and modifiers stop being invisible — unknown
    //! names get the unknown-property treatment at the `.prop`/`|name`
    //! token, values type-check, and model-declared modifier fields are the
    //! per-block vocabulary.

    use super::*;
    use nml_core::diagnostic::Severity;

    const SCHEMA: &str = "trait monitored:\n    timeout duration = \"5s\"\n\n\
                          trait accessControlled:\n    |allow []role?\n    |deny []role?\n\n\
                          model endpoint is monitored, accessControlled:\n    url string+\n\n\
                          model service:\n    endpoints []endpoint\n";

    fn check(source: &str, strict: bool) -> Vec<Diagnostic> {
        let (schema, diags) = crate::loader::load_schema(&[("t.model.nml", SCHEMA)]);
        assert!(diags.is_empty(), "{diags:?}");
        let v = SchemaValidator::new(schema.models, schema.enums, schema.oneofs);
        let v = if strict { v.strict() } else { v };
        v.validate(&nml_core::cst::parse_to_ast(source).unwrap())
    }

    #[test]
    fn unknown_shared_property_warns_with_suggestion_and_errors_strict() {
        let src = "service Api:\n    endpoints:\n        .timeuot = \"10s\"\n\n        - A:\n            url = \"https://a\"\n";
        let diags = check(src, false);
        let hit = diags
            .iter()
            .find(|d| d.message.contains("unknown shared property '.timeuot'"))
            .expect("flagged at the shared token");
        assert_eq!(hit.severity, Severity::Warning);
        assert_eq!(
            hit.suggestions.first().map(|s| s.replacement.as_str()),
            Some("timeout")
        );
        assert!(check(src, true).iter().any(
            |d| d.message.contains("unknown shared property") && d.severity == Severity::Error
        ));
    }

    #[test]
    fn shared_property_value_type_checks() {
        let src = "service Api:\n    endpoints:\n        .timeout = 9\n\n        - A:\n            url = \"https://a\"\n";
        assert!(
            check(src, false)
                .iter()
                .any(|d| d.message.contains("shared property")
                    && d.message.contains("'timeout'")
                    && d.code.map(|c| c.to_string()).as_deref() == Some("NML2008")),
            "{:?}",
            check(src, false)
        );
    }

    #[test]
    fn array_level_shared_properties_are_validated_too() {
        let src =
            "[]endpoint eps:\n    .timeuot = \"10s\"\n\n    - A:\n        url = \"https://a\"\n";
        assert!(
            check(src, false)
                .iter()
                .any(|d| d.message.contains("unknown shared property '.timeuot'"))
        );
    }

    #[test]
    fn model_declared_modifier_fields_are_the_vocabulary() {
        // `|alow` typo against endpoint's accessControlled-inherited fields:
        // flagged with a did-you-mean even with NO manifest modifier list.
        let bad =
            "[]endpoint eps:\n    |alow = [@public]\n\n    - A:\n        url = \"https://a\"\n";
        let diags = check(bad, false);
        let hit = diags
            .iter()
            .find(|d| d.code.map(|c| c.to_string()).as_deref() == Some("NML2002"))
            .expect("unknown modifier flagged from model vocabulary");
        assert!(hit.message.contains("model 'endpoint' declares"), "{hit:?}");
        assert_eq!(
            hit.suggestions.first().map(|s| s.replacement.as_str()),
            Some("allow")
        );
        // A correct modifier stays silent.
        let good =
            "[]endpoint eps:\n    |allow = [@public]\n\n    - A:\n        url = \"https://a\"\n";
        assert!(
            check(good, false)
                .iter()
                .all(|d| d.code.map(|c| c.to_string()).as_deref() != Some("NML2002"))
        );
    }
}

#[cfg(test)]
mod union_shared_property_tests {
    //! Union-element subset semantics: a shared property is known if ANY
    //! model variant defines it; values check against the defining
    //! variants' types through the standard union value check.

    use super::*;
    use nml_core::diagnostic::Severity;

    const SCHEMA: &str = "model alpha:\n    speed number?\n    size number?\n\n\
                          model beta:\n    label string?\n    size string?\n\n\
                          model service:\n    parts [](alpha | beta)\n";

    fn check(source: &str, strict: bool) -> Vec<Diagnostic> {
        let (schema, diags) = crate::loader::load_schema(&[("u.model.nml", SCHEMA)]);
        assert!(diags.is_empty(), "{diags:?}");
        let v = SchemaValidator::new(schema.models, schema.enums, schema.oneofs);
        let v = if strict { v.strict() } else { v };
        v.validate(&nml_core::cst::parse_to_ast(source).unwrap())
    }

    fn codes(diags: &[Diagnostic]) -> Vec<String> {
        diags
            .iter()
            .filter_map(|d| d.code.map(|c| c.to_string()))
            .collect()
    }

    #[test]
    fn name_defined_on_any_variant_is_known() {
        // `.label` lives only on beta — the first variant must not gatekeep.
        let src = "service Api:\n    parts:\n        .label = \"x\"\n\n        - A:\n            speed = 1\n";
        let diags = check(src, false);
        assert!(!codes(&diags).contains(&"NML2001".to_string()), "{diags:?}");
    }

    #[test]
    fn name_on_no_variant_warns_with_cross_variant_suggestion() {
        let src = "service Api:\n    parts:\n        .labl = \"x\"\n\n        - A:\n            speed = 1\n";
        let diags = check(src, false);
        let hit = diags
            .iter()
            .find(|d| d.message.contains("no union variant"))
            .expect("flagged at the shared token");
        assert_eq!(hit.severity, Severity::Warning);
        assert!(
            hit.message.contains("'alpha'") && hit.message.contains("'beta'"),
            "{hit:?}"
        );
        assert_eq!(
            hit.suggestions.first().map(|s| s.replacement.as_str()),
            Some("label")
        );
        assert!(
            check(src, true)
                .iter()
                .any(|d| d.message.contains("no union variant") && d.severity == Severity::Error)
        );
    }

    #[test]
    fn value_checks_against_defining_variants() {
        // `size` differs by variant (number | string): either shape passes…
        for ok in ["3", "\"big\""] {
            let src = format!(
                "service Api:\n    parts:\n        .size = {ok}\n\n        - A:\n            speed = 1\n"
            );
            let diags = check(&src, false);
            assert!(
                !codes(&diags)
                    .iter()
                    .any(|c| c == "NML2032" || c == "NML2008"),
                "{ok}: {diags:?}"
            );
        }
        // …a bool satisfies neither → the standard union mismatch.
        let src = "service Api:\n    parts:\n        .size = true\n\n        - A:\n            speed = 1\n";
        assert!(codes(&check(src, false)).contains(&"NML2032".to_string()));
        // A single-defining name type-checks directly against that field.
        let src =
            "service Api:\n    parts:\n        .label = 9\n\n        - A:\n            speed = 1\n";
        assert!(codes(&check(src, false)).contains(&"NML2008".to_string()));
    }
}
