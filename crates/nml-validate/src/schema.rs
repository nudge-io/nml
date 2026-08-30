use std::collections::{HashMap, HashSet};

use nml_core::ast::*;
use nml_core::model::{EnumDef, FieldDef, FieldType, ModelDef, OneOfDef, PrimitiveFacets};
use nml_core::resolve::ValueResolver;
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
    /// RFC 0047 resolved lane: when set, deferred values (`$ENV.KEY`,
    /// fallback chains) on FACETED fields are resolved during validation
    /// and their facets enforced — diagnostics name the variable and the
    /// bound, never the resolved content. `None` (the default) keeps
    /// deferral: literals are judged everywhere, `$ENV` where it exists.
    /// See [`Self::with_env_resolution`] for the lane-ownership rules.
    env_resolution: Option<ValueResolver>,
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

/// Inputs shared by list items and inline arm targets when validating an
/// inline instance body against a resolved target.
struct InlineBodyValidation<'a> {
    name: Option<&'a Identifier>,
    body: &'a Body,
    elem: &'a FieldTarget<'a>,
    label: ElemLabel<'a>,
    depth: u32,
    header_span: Option<Span>,
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
            env_resolution: None,
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

    /// RFC 0047 resolved lane: resolve deferred values (`$ENV.KEY` and
    /// fallback chains) during validation and enforce declared facets on
    /// what they resolve to. Diagnostics name the VARIABLE and the bound
    /// (`'pollInterval' from $ENV.P resolved to a value below the schema's
    /// min = 60s`) — never the resolved content, which is
    /// secret-provenance text.
    ///
    /// **Lane ownership is a security property, not a preference.** Set
    /// this only at the boundary whose deserialization will use the SAME
    /// resolver — the process that owns the file's resolution (a tenant
    /// child booting its own `nudge.nml`; a deploy CLI baking its own
    /// deploy config). A tenant-facing surface that resolved against its
    /// own environment would hand tenants an existence oracle over
    /// operator variables ("does `$ENV.OPERATOR_SECRET` exist?" answered
    /// by whether a facet diagnostic appears). Resolver-free validators
    /// (the default) keep deferral: literals are judged everywhere,
    /// `$ENV` values where they exist, which is only the owning boundary.
    ///
    /// Unset variables stay silent here — a missing variable is
    /// deserialization's error to report (with its fallback semantics),
    /// not a validation failure. Text that does not parse as the field's
    /// domain is likewise left to deserialization's provenance-aware
    /// coercion errors; this lane adds exactly the facet checks that
    /// pre-resolution validation must defer.
    pub fn with_env_resolution(mut self, resolver: ValueResolver) -> Self {
        self.env_resolution = Some(resolver);
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

    /// RFC 0047 wiring for callers that own the resolution of SOME blocks
    /// of a file but not others: enforce `model_name`'s Number/Duration
    /// facets on what `body`'s deferred values (`$ENV.KEY`, fallback
    /// chains) resolve to under `resolver`. The deploy CLI is the shape
    /// this exists for — `build:`/`deploy:` blocks are CLI-baked (the
    /// CLI's environment IS the authority) while the same file's server
    /// blocks are child-authoritative, so a whole-file
    /// [`Self::with_env_resolution`] pass would judge foreign lanes
    /// against the wrong environment.
    ///
    /// Same leaf as the whole-file resolved lane, so the two wirings
    /// cannot diverge: variable-naming, content-redacting diagnostics;
    /// silent on unset variables and unparseable text (deserialization's
    /// errors); literal values skipped (the resolver-free whole-file pass
    /// already judged them, including facet checks on literal fallback
    /// legs).
    ///
    /// The walk is deliberately SHALLOW — top-level properties only. Its
    /// contract is models whose faceted fields are all top-level scalars;
    /// the embedder pins that shape (nudge-schemas' flatness pin), so a
    /// future nested faceted field fails a test instead of silently
    /// escaping the walk.
    pub fn validate_resolved_facets(
        &self,
        body: &Body,
        model_name: &str,
        resolver: &ValueResolver,
    ) -> Vec<Diagnostic> {
        let mut diags = Vec::new();
        let Some(model) = self.find_model(model_name) else {
            return diags;
        };
        for entry in &body.entries {
            let BodyEntryKind::Property(prop) = &entry.kind else {
                continue;
            };
            let Some(field) = model.fields.iter().find(|f| f.name == prop.name.name) else {
                continue;
            };
            let Some(facets) = scalar_facets(&field.field_type) else {
                continue;
            };
            check_resolved_facets(
                resolver,
                facets,
                &prop.value.value,
                &field.name,
                prop.value.span,
                &mut diags,
            );
        }
        diags
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
                if nml_core::symbols::is_schema_keyword(keyword) {
                    self.validate_body(&block.body, true, keyword, &mut diagnostics);
                    if matches!(keyword, "model" | "trait") {
                        // Declared defaults are NOT checked here. Both
                        // their facet rules (NML2058, via
                        // `extract_schema`) and their type rules (via
                        // `default_diagnostics`, below) emit from paths
                        // every schema consumer passes through — this
                        // verb reaches them through `load_schema`. A
                        // second check here would print one defect twice.
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
        let is_schema_def = nml_core::symbols::is_schema_keyword(keyword.as_str());

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
            // Defaults: see `default_diagnostics` — checked once, where
            // the schema is loaded, not once per validating surface.
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
        let is_schema_def = nml_core::symbols::is_schema_keyword(keyword.as_str());
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
                K::Named { name, body } => {
                    self.validate_inline_body(
                        InlineBodyValidation {
                            name: Some(name),
                            body,
                            elem: &FieldTarget::Model(m),
                            label,
                            depth,
                            header_span: header,
                        },
                        diags,
                    );
                }
                K::Shorthand { .. } => {
                    let result = nml_core::identity::materialize_item(item, m);
                    self.validate_materialized(result, m, depth, header, diags);
                }
                K::Reference(_) | K::Role(_) => {}
            },
            FieldTarget::OneOf(_) => match &item.kind {
                K::Named { name, body } => {
                    self.validate_inline_body(
                        InlineBodyValidation {
                            name: Some(name),
                            body,
                            elem,
                            label,
                            depth,
                            header_span: header,
                        },
                        diags,
                    );
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
                diags.push(self.index.unknown_union_variant(variants, ann));
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
                self.validate_instance_against_arms(body, key, target, depth, diags);
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
        depth: u32,
        diags: &mut Vec<Diagnostic>,
    ) {
        if depth >= MAX_VALIDATION_DEPTH {
            diags.push(truncation_advisory(body, None));
            return;
        }
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
            self.validate_arm_target(&arm.target, target, depth + 1, diags);
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
                    if !matches!(
                        key,
                        FieldType::Primitive {
                            ty: PrimitiveType::Role,
                            ..
                        }
                    ) {
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
                ArmSelector::Literal(selector) => {
                    if else_seen {
                        diags.push(
                            Diagnostic::error(format!(
                                "arm '{selector:?}' is unreachable: arms match first-to-last, \
                                 so 'else' must be the final arm"
                            ))
                            .with_code(codes::UNREACHABLE_ARM)
                            .with_span(arm.selector_span),
                        );
                    }
                    if let Some(enum_def) = self.index.arm_key_enum_def(key) {
                        self.validate_enum_value(
                            &Value::String(selector.clone()),
                            enum_def,
                            "arm key",
                            arm.selector_span,
                            diags,
                        );
                    } else if !self.index.arm_literal_key_admits(key, selector) {
                        diags.push(
                            Diagnostic::error(format!(
                                "arm key {selector:?} does not conform to the declared key \
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

    /// Validate an inline instance body against a resolved target — shared by
    /// list items and inline arm targets so both spellings agree by construction.
    fn validate_inline_body(&self, ctx: InlineBodyValidation<'_>, diags: &mut Vec<Diagnostic>) {
        match ctx.elem {
            FieldTarget::Model(m) => {
                let result = match ctx.name {
                    Some(name) => nml_core::identity::materialize_arm_inline(name, ctx.body, m),
                    None => nml_core::identity::Materialized {
                        body: ctx.body.clone(),
                        diagnostics: Vec::new(),
                        validatable: true,
                    },
                };
                self.validate_materialized(result, m, ctx.depth, ctx.header_span, diags);
            }
            FieldTarget::OneOf(oneof) => {
                // A NAMED item under a oneof element materializes its
                // name into the arm's `+` field exactly like a model
                // element's item (the arm the body states, else the
                // schema default — the same consult composition makes),
                // or the arm's required positional field reads as
                // missing on every raw block.
                let arm = ctx.name.and_then(|_| {
                    let stated = ctx.body.entries.iter().find_map(|e| match &e.kind {
                        BodyEntryKind::Property(p) if p.name.name == oneof.discriminator => {
                            p.value.value.as_str().map(str::to_string)
                        }
                        _ => None,
                    });
                    let disc = stated.or_else(|| oneof.default_discriminator.clone())?;
                    oneof
                        .variants
                        .iter()
                        .find(|(v, _)| *v == disc)
                        .and_then(|(_, m)| self.find_model(m))
                });
                let materialized = match (ctx.name, arm) {
                    (Some(name), Some(arm)) => {
                        nml_core::identity::materialize_arm_inline(name, ctx.body, arm)
                    }
                    _ => nml_core::identity::Materialized {
                        body: ctx.body.clone(),
                        diagnostics: Vec::new(),
                        validatable: true,
                    },
                };
                diags.extend(materialized.diagnostics);
                if materialized.validatable {
                    self.validate_target_instance(
                        ctx.elem,
                        &materialized.body,
                        ctx.depth,
                        ctx.header_span,
                        ctx.label,
                        diags,
                    );
                }
            }
            FieldTarget::Leaf(ty) | FieldTarget::Union(ty) => {
                if !ctx.body.entries.is_empty() {
                    diags.push(
                        Diagnostic::error(format!(
                            "this {}'s body has nowhere to go: the element type \
                             `{ty}` has no fields to fill",
                            ctx.label.field
                        ))
                        .with_code(codes::DROPPED_ITEM_BODY)
                        .with_span(ctx.header_span.unwrap_or_else(|| Span::empty(0))),
                    );
                }
            }
            _ => {
                if !ctx.body.entries.is_empty() {
                    diags.push(
                        Diagnostic::error(format!(
                            "this {} requires a model or oneof element type",
                            ctx.label.field
                        ))
                        .with_code(codes::ARM_TARGET_MISMATCH)
                        .with_span(ctx.header_span.unwrap_or_else(|| Span::empty(0))),
                    );
                }
            }
        }
    }

    /// Validate one arm target against the arm set's `V` (RFC 0007 §6):
    /// - a **reference** (`-> Name`) is never existence-checked (§4.1,
    ///   consumer-resolved cross-scope) — its form is legal for any `V`;
    /// - a **literal** (`-> "path/url"`) requires a *scalar-capable* `V`;
    /// - an **inline** (`-> Name:` + body) requires a model/`oneof` `V` and
    ///   is fully validated against it (§4.1, §6.2).
    fn validate_arm_target(
        &self,
        arm_target: &ArmTarget,
        v: &FieldType,
        depth: u32,
        diags: &mut Vec<Diagnostic>,
    ) {
        match arm_target {
            ArmTarget::Reference(_) => {}
            ArmTarget::Literal { value, span } => {
                if self.index.field_type_admits_a_literal(v) {
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
                            "a string-literal arm target requires a scalar target type, but \
                             this arm set targets '{v}'; use a declared name ('-> Name') or an \
                             inline block ('-> Name:')"
                        ))
                        .with_code(codes::ARM_TARGET_MISMATCH)
                        .with_span(*span),
                    );
                }
            }
            ArmTarget::Inline { name, body } => {
                if !self.index.field_type_admits_inline(v) {
                    diags.push(
                        Diagnostic::error(format!(
                            "an inline arm target requires a model or oneof target type, but \
                             this arm set targets '{v}'; use a reference ('-> Name') or a \
                             string literal ('-> \"…\"') instead"
                        ))
                        .with_code(codes::ARM_TARGET_MISMATCH)
                        .with_span(name.span),
                    );
                    return;
                }
                let elem = self.index.resolve_type_in_body(v, body);
                self.validate_inline_body(
                    InlineBodyValidation {
                        name: Some(name),
                        body,
                        elem: &elem,
                        label: ElemLabel {
                            field: "arm target",
                            container: "for",
                        },
                        depth,
                        header_span: Some(name.span),
                    },
                    diags,
                );
            }
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
                                self.validate_instance_against_arms(
                                    &nb.body,
                                    key,
                                    target,
                                    depth + 1,
                                    diags,
                                );
                            }
                            // Union fields — plain or modifier-wrapped — were
                            // dispatched by `union_variants` above, before this
                            // match.
                            FieldType::Primitive {
                                ty: PrimitiveType::Object,
                                ..
                            } => {}
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
                    // A type-annotation modifier (`|slot (a | b)`) is a
                    // declaration, never a value (RFC 0019 errata E12):
                    // it satisfies no required field.
                    if !matches!(m.value, ModifierValue::TypeAnnotation { .. }) {
                        seen_fields.push(&m.name.name);
                    }

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
        // EVERY later entry of that name must be a string too: composition
        // re-adds the effective string discriminator ahead of a dependent's
        // `kind = 5`, so a first-only check laundered the dependent's type
        // error through the composed view (NML2042 raw, silence composed).
        for prop in body
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                BodyEntryKind::Property(prop) if prop.name.name == oneof.discriminator => {
                    Some(prop)
                }
                _ => None,
            })
            .skip(1)
        {
            if !matches!(prop.value.value, Value::String(_)) {
                diags.push(
                    Diagnostic::error(format!(
                        "discriminator '{}' for oneof '{}' must be a string (one of: {})",
                        oneof.discriminator,
                        oneof.name,
                        valid_values(),
                    ))
                    .with_code(codes::INVALID_DISCRIMINATOR)
                    .with_span(prop.value.span),
                );
            }
        }

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
        // The RFC 0047 resolved lane runs exactly ONCE per declared value,
        // here — before the fallback split below. Resolution
        // short-circuits on the first leg that succeeds, so resolving the
        // whole chain once is precisely what deserialization does;
        // judging legs individually would reject a config the runtime
        // happily runs (`$ENV.OVERRIDE | $ENV.STALE` with a valid
        // override). Nested values (list elements, block fields) re-enter
        // through this same door and so get their own single check.
        // Only FACETED primitives have anything to judge here, and the
        // guard is load-bearing rather than an optimization: without it
        // an unfaceted `secret` field would have its `$ENV` READ and its
        // plaintext materialized on every validation, purely to be
        // dropped. Credentials stay outside the lane's resolution, not
        // just outside its judgment.
        // Wrapper layers are looked THROUGH (`scalar_facets` unwraps
        // `Modifier`), because the lane's "once per declared value" rule
        // is about the VALUE, not about how many type layers the schema
        // spells around it. Getting this wrong is not a missed check but
        // an inverted one: the wrapper arms below re-enter with the SAME
        // value, so a lane guard that only recognized a bare `Primitive`
        // would skip the whole-chain check here and then run it once per
        // FALLBACK LEG underneath — resolving legs the runtime never
        // reads and reporting a violation in a leg that never wins.
        if let Some(resolver) = &self.env_resolution {
            if let Some(facets) = scalar_facets(field_type) {
                if !matches!(facets, PrimitiveFacets::None) {
                    check_resolved_facets(resolver, facets, value, field_name, span, diags);
                }
            }
        }

        self.validate_value_type_only(value, field_type, field_name, context, span, diags);
    }

    /// The TYPE/shape half, with the resolved lane already run once for
    /// the whole declared value. Fallback chains split here — every leg
    /// must be a legal spelling for the field, whichever wins at runtime
    /// — and the split RECURSES, because `a | b | c` parses
    /// right-associative as `Fallback(a, Fallback(b, c))`: a non-recursive
    /// split would hand the nested tail to the type match and report a
    /// bogus "expected number, got fallback" on every chain of three or
    /// more legs.
    fn validate_value_type_only(
        &self,
        value: &Value,
        field_type: &FieldType,
        field_name: &str,
        context: &str,
        span: Span,
        diags: &mut Vec<Diagnostic>,
    ) {
        if let Value::Fallback(primary, fallback) = value {
            self.validate_value_type_only(
                &primary.value,
                field_type,
                field_name,
                context,
                primary.span,
                diags,
            );
            self.validate_value_type_only(
                &fallback.value,
                field_type,
                field_name,
                context,
                fallback.span,
                diags,
            );
            return;
        }
        self.validate_non_fallback_value(value, field_type, field_name, context, span, diags);
    }

    /// The type/shape half of [`Self::validate_value_against_type`], with
    /// fallback chains already split and the resolved lane already run.
    /// Recursive descent re-enters through the public door, so nested
    /// values keep their own single resolved check.
    fn validate_non_fallback_value(
        &self,
        value: &Value,
        field_type: &FieldType,
        field_name: &str,
        context: &str,
        span: Span,
        diags: &mut Vec<Diagnostic>,
    ) {
        match field_type {
            FieldType::Primitive { ty: prim, facets } => {
                self.validate_primitive_value(value, prim, field_name, context, span, diags);
                // Facets are domain-tagged: number bounds fire only on
                // number values, duration bounds only on durations — a
                // type-mismatched value already has its type diagnostic
                // from `validate_primitive_value`, and firing a facet on
                // it would measure across domains.
                match (facets, value) {
                    (PrimitiveFacets::Number(fs), Value::Number(n)) => {
                        validate_facets(fs, n, field_name, span, diags);
                    }
                    (PrimitiveFacets::Duration(fs), Value::Duration(d)) => {
                        validate_facets(fs, d, field_name, span, diags);
                    }
                    // Deferred values (`$ENV.KEY`, const references) are
                    // judged by the RFC 0047 resolved lane at the top of
                    // `validate_value_against_type` — once per declared
                    // value, so a fallback chain resolves the way
                    // deserialization resolves it. A `secret`-typed field
                    // carries no facets, so credentials are structurally
                    // outside that lane.
                    _ => {}
                }
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
                } else {
                    // RFC 0018: union semantics are ANY-variant-admits.
                    // Every structurally-matching variant gets a chance;
                    // the value passes if any accepts it (facets
                    // included). Only when every matching variant
                    // rejects do the FIRST one's findings surface —
                    // deterministic, and authoring order is the natural
                    // priority. Pre-facet unions keep byte-identical
                    // behavior: an unfaceted matching variant admits
                    // unconditionally.
                    let mut first_rejection: Option<Vec<Diagnostic>> = None;
                    let mut admitted = false;
                    for v in variants
                        .iter()
                        .filter(|v| self.value_matches_type(value, v))
                    {
                        if !type_has_facets(v) {
                            admitted = true;
                            break;
                        }
                        // Non-emitting admission first: a variant that
                        // accepts the value needs no diagnostics at
                        // all, and speculatively formatting messages we
                        // then discard cost ~193ns each (a 16x constant
                        // on faceted unions).
                        if let FieldType::Primitive { facets, .. } = v {
                            let verdict = match (facets, value) {
                                (PrimitiveFacets::Number(fs), Value::Number(n)) => {
                                    Some(fs.admits(n))
                                }
                                (PrimitiveFacets::Duration(fs), Value::Duration(d)) => {
                                    Some(fs.admits(d))
                                }
                                _ => None,
                            };
                            if let Some(admits) = verdict {
                                if admits {
                                    admitted = true;
                                    break;
                                }
                                if first_rejection.is_some() {
                                    continue;
                                }
                            }
                        }
                        // Type-only, for the same reason as `Modifier`:
                        // a variant is another spelling of THIS value,
                        // so re-entering the public door would re-run
                        // the resolved lane once per candidate variant
                        // — resolving an `$ENV` repeatedly to judge one
                        // declared value.
                        let mut scratch = Vec::new();
                        self.validate_value_type_only(
                            value,
                            v,
                            field_name,
                            context,
                            span,
                            &mut scratch,
                        );
                        if scratch.is_empty() {
                            admitted = true;
                            break;
                        }
                        if first_rejection.is_none() {
                            first_rejection = Some(scratch);
                        }
                    }
                    if !admitted {
                        if let Some(d) = first_rejection {
                            diags.extend(d);
                        }
                    }
                }
            }
            FieldType::Modifier(declared) => {
                // Type-only re-entry: this is the SAME value one layer
                // down, not a nested one, and the public door already ran
                // its resolved-lane check through this wrapper.
                self.validate_value_type_only(value, declared, field_name, context, span, diags);
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
            FieldType::Primitive { ty: prim, .. } => value_matches_primitive(value, prim),
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
                // applies it in bulk. The acceptance grammar is
                // `parse_text` — the SAME coercion grammar the de-layer
                // used to type these strings — so every spelling that
                // ever worked gets the migration fix; text outside it
                // falls through to the ordinary mismatch below. The
                // "drop the quotes" hint appears only when de-quoting IS
                // the whole fix (canonical Display == the quoted text);
                // for coercion-only spellings ("1.5h" → 1h30m) the
                // suggestion carries the rewrite and the hint would lie.
                if let Ok(d) = nml_core::duration::Duration::parse_text(text) {
                    let canonical = d.to_string();
                    let hint = if canonical == *text {
                        " (drop the quotes)"
                    } else {
                        ""
                    };
                    diags.push(
                        Diagnostic::error(format!(
                            "duration field '{field_name}': a quoted duration was \
                             replaced by the duration literal{hint}"
                        ))
                        .with_code(codes::REPLACED_SYNTAX)
                        .with_span(span)
                        .with_suggestion(canonical, span),
                    );
                    return;
                }
            }
        }
        // The same migration teaching for the OTHER typed literals: a
        // quoted number or bool against its typed field is the legacy
        // spelling, not a mere mismatch — the generic NML2008 told an
        // author WHAT failed but not the one-keystroke fix. Same
        // machine-applicable shape as the duration arm, including the
        // grammar: `parse_coercion` (RFC 0016 §1.4) is what the de-layer
        // used to type these strings, so exponent spellings ("1e-6")
        // that worked through the old coercion get the migration fix too
        // — the suggestion is the canonical literal (Display folds the
        // exponent away), which is always valid source. Anything outside
        // the coercion grammar falls through to the ordinary mismatch.
        if *prim == PrimitiveType::Number {
            if let Value::String(text) = value {
                if let Ok(n) = nml_core::types::Number::parse_coercion(text) {
                    let canonical = n.to_string();
                    let hint = if canonical == *text {
                        " (drop the quotes)"
                    } else {
                        ""
                    };
                    diags.push(
                        Diagnostic::error(format!(
                            "number field '{field_name}': a quoted number was \
                             replaced by the number literal{hint}"
                        ))
                        .with_code(codes::REPLACED_SYNTAX)
                        .with_span(span)
                        .with_suggestion(canonical, span),
                    );
                    return;
                }
            }
        }
        if *prim == PrimitiveType::Bool {
            if let Value::String(text) = value {
                if matches!(text.as_str(), "true" | "false") {
                    diags.push(
                        Diagnostic::error(format!(
                            "bool field '{field_name}': a quoted bool was \
                             replaced by the bool literal (drop the quotes)"
                        ))
                        .with_code(codes::REPLACED_SYNTAX)
                        .with_span(span)
                        .with_suggestion(text.clone(), span),
                    );
                    return;
                }
            }
        }
        if *prim == PrimitiveType::Role {
            if let Value::String(s) = value {
                // The did-you-mean is offered ONLY when its result would be
                // a valid bare role token. A value carrying characters
                // outside the RolePath charset (spaces, quotes,
                // backslashes) splits two ways:
                // * `@`-prefixed — the consumer's quoted-value form (e.g.
                //   nudge RFC 0055 D11 `@user/"fred & wilma@example.com"`):
                //   DELIBERATELY string-form, the only possible spelling —
                //   no diagnostic at all. A warning would also echo the
                //   value through the suggestion into consumer boot logs,
                //   and these values can be PII-class (mailboxes).
                // * anything else — still worth teaching, but with NO
                //   suggestion: prepending `@` to a non-token value would
                //   suggest an unlexable spelling AND echo the (possibly
                //   PII-class) value.
                let bare_expressible = s.chars().all(|c| {
                    c.is_ascii_alphanumeric()
                        || matches!(c, '_' | '/' | ':' | '@' | '{' | '}' | '.' | '+' | '-')
                });
                if !bare_expressible {
                    if !s.starts_with('@') {
                        diags.push(
                            Diagnostic::warning(format!(
                                "role field '{field_name}': roles are references, not strings"
                            ))
                            .with_code(codes::ROLE_LITERAL)
                            .with_span(span),
                        );
                    }
                    return;
                }
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
        FieldTypeExpr::Named { .. } => {}
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
/// Does this type carry RFC 0018 facets anywhere (itself or a nested
/// element type)? Gates the union arm's variant re-validation so
/// pre-facet schemas keep byte-identical union behavior.
fn type_has_facets(t: &FieldType) -> bool {
    match t {
        FieldType::Primitive { facets, .. } => !facets.is_none(),
        FieldType::List(inner) | FieldType::Set(inner) | FieldType::Modifier(inner) => {
            type_has_facets(inner)
        }
        FieldType::Union(vs) => vs.iter().any(type_has_facets),
        FieldType::Arms { key, target } => type_has_facets(key) || type_has_facets(target),
        FieldType::ModelRef(_) => false,
    }
}

/// Every declared default measured against its own field type — the
/// **single** default-checking entry. The loader (and therefore the CLI
/// verbs, schema packages, and downstream boots), the LSP's registry
/// pass, and the covered-schema pass all call this, so a default is
/// judged identically wherever a schema is loaded instead of once per
/// surface that happens to remember.
///
/// Reuses `validate_value_against_type` rather than walking the type
/// again: parallel traversals of exactly this shape produced three
/// separate facet regressions in this RFC's review history.
///
/// Facet violations are dropped — `cst::extract_schema` already emits
/// them for every consumer (RFC 0018 §1.2), so keeping them would
/// double-report. Safe on a PARTIAL schema view (the LSP's open-buffer
/// registry): an unresolvable model or enum reference degrades to a
/// value-shape check, never a false "unknown definition".
pub fn default_diagnostics(schema: &ExtractedSchema) -> Vec<Diagnostic> {
    // The clone is deliberate. `SchemaValidator` owns its definitions,
    // and threading a borrowed one through would widen its API for a
    // measured 46 µs on a real 85-model schema set — a quarter of one
    // keystroke's diagnostics work, linear, and not on any hot loop.
    // Revisit only with a schema set large enough to feel it.
    let validator = SchemaValidator::new(
        schema.models.clone(),
        schema.enums.clone(),
        schema.oneofs.clone(),
    )
    .composition_checked_at_load();
    let mut out = Vec::new();
    for model in &schema.models {
        for field in &model.fields {
            let Some(default) = &field.default_value else {
                continue;
            };
            let mut scratch = Vec::new();
            validator.validate_value_against_type(
                &default.value,
                &field.field_type,
                &field.name,
                "as the default for",
                default.span,
                &mut scratch,
            );
            scratch.retain(|d| d.code != Some(codes::FACET_VIOLATION));
            if let Some(src) = model.source.as_deref() {
                for d in &mut scratch {
                    *d = d.clone().with_source(src);
                }
            }
            out.extend(scratch);
        }
    }
    out
}

/// RFC 0018 facet enforcement (NML2057). Exact comparisons through
/// `Number`'s numeric `Ord` and `is_multiple_of` — a boundary can never
/// lie the way an f64 comparison does. Values echoed are authored
/// literals on both sides (config value, schema facet): raw-AST band,
/// bounded by their own written length.
fn validate_facets<T: nml_core::model::FacetDomain>(
    facets: &nml_core::model::Facets<T>,
    value: &T,
    field_name: &str,
    span: Span,
    diags: &mut Vec<Diagnostic>,
) {
    for tail in facets.violations(value) {
        diags.push(
            Diagnostic::error(format!("'{field_name}' {tail}"))
                .with_code(codes::FACET_VIOLATION)
                .with_span(span),
        );
    }
}

/// The RFC 0047 resolved-lane leaf, shared by the whole-file walk (the
/// facet hook above, via [`SchemaValidator::with_env_resolution`]) and the
/// per-model shallow walk ([`SchemaValidator::validate_resolved_facets`]).
///
/// Resolves `value` with the caller's resolver and, when that yields
/// env-provenance text, interprets the text with the SAME parsers the
/// de-layer's typed coercions use — `Number::parse_coercion` /
/// `Duration::parse_text` (RFC 0016 §1.4's machine-data grammar) — so
/// validation and the runtime can never disagree about which env values
/// are valid or what they denote. Anything else stays silent by design:
/// unset variables and unparseable text are deserialization's errors
/// (which name the variable), literals were judged by the normal facet
/// arm, and an unresolvable reference is still deferred.
fn check_resolved_facets(
    resolver: &ValueResolver,
    facets: &PrimitiveFacets,
    value: &Value,
    field_name: &str,
    span: Span,
    diags: &mut Vec<Diagnostic>,
) {
    // A value that needs no resolution was already judged by the literal
    // facet arm; re-checking it here would double-report.
    if !matches!(
        value,
        Value::Secret(_) | Value::Reference(_) | Value::Fallback(..)
    ) {
        return;
    }
    // Whether a resolution that lands on an ordinary LITERAL may be
    // judged here. Only a `const` reference may: nothing else in the walk
    // ever sees the value a const stands for, so without this its facets
    // would go unchecked. A fallback chain is different — its legs are
    // type- and facet-checked individually by the split, so judging the
    // literal leg it resolves to would report the same violation twice,
    // at two different spans.
    let literal_is_unjudged_elsewhere = matches!(value, Value::Reference(_));
    let Ok(resolved) = resolver.resolve(value) else {
        return;
    };
    match (facets, &resolved) {
        // Env-provenance text: interpret with the de-layer's own coercion
        // parsers, and name the VARIABLE rather than the content.
        (PrimitiveFacets::Number(fs), Value::Resolved(text)) => {
            if let Ok(n) = nml_core::types::Number::parse_coercion(text.as_str()) {
                validate_facets_resolved(fs, &n, text.var(), field_name, span, diags);
            }
        }
        (PrimitiveFacets::Duration(fs), Value::Resolved(text)) => {
            if let Ok(d) = nml_core::duration::Duration::parse_text(text.as_str()) {
                validate_facets_resolved(fs, &d, text.var(), field_name, span, diags);
            }
        }
        // A const reference resolved to a typed literal: the value is
        // authored in-file and carries no secret provenance, so it takes
        // the ORDINARY value-echoing diagnostic — the same one the
        // literal would have drawn had it been written inline. Without
        // this, a faceted field could dodge its bounds behind a `const`.
        (PrimitiveFacets::Number(fs), Value::Number(n)) if literal_is_unjudged_elsewhere => {
            validate_facets(fs, n, field_name, span, diags);
        }
        (PrimitiveFacets::Duration(fs), Value::Duration(d)) if literal_is_unjudged_elsewhere => {
            validate_facets(fs, d, field_name, span, diags);
        }
        _ => {}
    }
}

/// [`validate_facets`]' provenance-redacting twin: the value drives the
/// comparison (via `violation_descriptions`) but never appears in the
/// message — resolved env content is secret-provenance text, so the
/// diagnostic names the variable and the violated bound instead. Fully
/// actionable (`echo $VAR` shows the operator their value) without
/// reopening per-message "is echoing safe here?" reasoning.
fn validate_facets_resolved<T: nml_core::model::FacetDomain>(
    facets: &nml_core::model::Facets<T>,
    value: &T,
    var: &str,
    field_name: &str,
    span: Span,
    diags: &mut Vec<Diagnostic>,
) {
    for desc in facets.violation_descriptions(value) {
        diags.push(
            Diagnostic::error(format!(
                "'{field_name}' from {var} resolved to a value {desc}"
            ))
            .with_code(codes::FACET_VIOLATION)
            .with_span(span),
        );
    }
}

/// The facets of a field whose declared type is a scalar primitive at top
/// level (through wrapper layers) — the positions the shallow resolved
/// walk covers. Anything structured returns `None` and is skipped there;
/// the embedder's flatness pin keeps that skip vacuous.
fn scalar_facets(ty: &FieldType) -> Option<&PrimitiveFacets> {
    match ty {
        FieldType::Primitive { facets, .. } => Some(facets),
        FieldType::Modifier(inner) => scalar_facets(inner),
        _ => None,
    }
}

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
        // Unreachable in practice — validation runs on the raw AST and
        // only the resolver mints Resolved — but the defensive answer is
        // its string shape.
        Value::Resolved(_) => "string",
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

    /// RFC 0018 §2 config-side fixtures: exact facet enforcement —
    /// inclusive boundaries hold, value-based numbers count (`80.0` IS
    /// 80), elements of collections are checked, and `multipleOf` is
    /// decided exactly (the `0.3 / 0.1` f64 lie cannot happen).
    #[test]
    fn facets_enforce_exactly() {
        let schema = "model svc:\n    name string+\n    port number(min = 1, max = 65535)?\n    weight number(min = 0, exclusiveMax = 1)?\n    step number(multipleOf = 0.1)?\n    ports set<number(min = 1)>?\n";
        let ok = |src: &str| {
            let d = diags(schema, src);
            assert!(
                d.iter().all(|x| x.severity != Severity::Error),
                "expected clean, got {d:?}"
            );
        };
        let bad = |src: &str, needle: &str| {
            let d = diags(schema, src);
            let hit = d.iter().find(|x| {
                x.code == Some(nml_core::diagnostic::codes::FACET_VIOLATION)
                    && x.rendered_message().contains(needle)
            });
            assert!(
                hit.is_some(),
                "expected violation containing {needle:?}, got {d:?}"
            );
        };
        ok("svc A:\n    port = 1\n");
        ok("svc A:\n    port = 65535\n");
        ok("svc A:\n    port = 80.0\n"); // value-based: 80.0 IS 80
        bad("svc A:\n    port = 0\n", "below the schema's min = 1");
        bad(
            "svc A:\n    port = 65536\n",
            "above the schema's max = 65535",
        );
        ok("svc A:\n    weight = 0\n");
        // Equality on an exclusive bound reads "at", not "above" — the
        // value does not exceed the bound, it violates exclusivity.
        bad(
            "svc A:\n    weight = 1\n",
            "at the schema's exclusiveMax = 1",
        );
        ok("svc A:\n    weight = 0.9999999999999999999999999999999999\n");
        ok("svc A:\n    step = 0.3\n"); // the f64 lie, pinned at the validator
        ok("svc A:\n    step = -0.2\n");
        ok("svc A:\n    step = 0\n");
        bad("svc A:\n    step = 0.25\n", "not a multiple");
        ok("svc A:\n    ports = [1, 80, 65535]\n");
        bad(
            "svc A:\n    ports = [80, 0]\n",
            "below the schema's min = 1",
        );

        // Union variants bind their facets: matching is type-shaped
        // (0 IS a number, so the union admits it), then the matched
        // variant's range applies.
        let schema_u = "model svc:\n    name string+\n    lim (number(min = 1) | string)?\n";
        let d = diags(schema_u, "svc A:\n    lim = \"unbounded\"\n");
        assert!(
            d.iter().all(|x| x.severity != Severity::Error),
            "string variant must satisfy the union: {d:?}"
        );
        let d = diags(schema_u, "svc A:\n    lim = 5\n");
        assert!(
            d.iter().all(|x| x.severity != Severity::Error),
            "in-range number must pass: {d:?}"
        );
        // ANY-variant-admits: -5 violates the first variant's min but
        // satisfies the second's max — the union admits it.
        let schema_2 =
            "model svc:\n    name string+\n    lim2 (number(min = 1) | number(max = 0))?\n";
        let d = diags(schema_2, "svc A:\n    lim2 = -5\n");
        assert!(
            d.iter().all(|x| x.severity != Severity::Error),
            "a variant admits -5; the union must: {d:?}"
        );
        let d = diags(schema_2, "svc A:\n    lim2 = 0.5\n");
        assert!(
            d.iter()
                .any(|x| x.code == Some(nml_core::diagnostic::codes::FACET_VIOLATION)),
            "no variant admits 0.5: {d:?}"
        );

        let d = diags(schema_u, "svc A:\n    lim = 0\n");
        assert!(
            d.iter().any(|x| {
                x.code == Some(nml_core::diagnostic::codes::FACET_VIOLATION)
                    && x.rendered_message().contains("below the schema's min = 1")
            }),
            "union-matched number must still honor facets: {d:?}"
        );
    }

    /// The union fast path (`NumberFacets::admits`) short-circuits on
    /// the first ADMITTING variant and skips diagnostic construction
    /// for later rejections. The regression it must never introduce:
    /// a variant that admits must still be reached when earlier ones
    /// rejected — otherwise faceted unions silently reject valid
    /// config. Exercised at every position, including last.
    #[test]
    fn union_fast_path_reaches_a_late_admitting_variant() {
        // Four disjoint faceted bands; only one admits each probe.
        let schema = "model svc:\n    name string+\n    v (number(min = 100) | number(min = 30, max = 39) | number(min = 20, max = 29) | number(min = 10, max = 19))?\n";
        // 15 admits only the LAST variant — the position the
        // short-circuit would break if it stopped early.
        for (value, want_ok) in [
            ("150", true), // first
            ("35", true),  // second
            ("25", true),  // third
            ("15", true),  // LAST
            ("5", false),  // none
        ] {
            let d = diags(schema, &format!("svc A:\n    v = {value}\n"));
            let errs: Vec<_> = d.iter().filter(|x| x.severity == Severity::Error).collect();
            if want_ok {
                assert!(errs.is_empty(), "v = {value} must be admitted: {errs:?}");
            } else {
                assert!(
                    errs.iter()
                        .any(|x| x.code == Some(nml_core::diagnostic::codes::FACET_VIOLATION)),
                    "v = {value} matches no band: {d:?}"
                );
            }
        }
    }

    /// **The parity invariant, generated.** A declared default and the
    /// same literal written as a config value face the SAME type, so
    /// they must produce the SAME facet violations — not merely the
    /// same COUNT, but the same bound named in the same words.
    ///
    /// Two traversals decide that: the definition-side walk in nml-core
    /// and enforcement's walk here. Every facet defect in rounds 24-26
    /// was them disagreeing about which sub-type a value faces (scalar
    /// unions, unions of collections, fallback legs) — and each was
    /// missed because the guard was a hand-picked list, i.e. only the
    /// shapes someone thought of. This sweeps the CROSS-PRODUCT so a
    /// divergence in a shape nobody enumerated still fails here.
    ///
    /// Pairs whose value-side schema or config does not parse cleanly
    /// are skipped: they exercise the parse band, not the facet walks.
    #[test]
    fn default_checking_and_value_checking_agree() {
        const TYPES: &[&str] = &[
            "number(min = 10)",
            "number(max = 0)",
            "number(exclusiveMin = 10)",
            "number(exclusiveMax = 0)",
            "number(min = 1, max = 20)",
            "number(multipleOf = 0.1)",
            "[]number(min = 10)",
            "[][]number(min = 10)",
            "set<number(min = 10)>",
            "set<[]number(min = 10)>",
            "[]set<number(min = 10)>",
            "(number(min = 10) | string)",
            "(string | number(min = 10))",
            "(number(min = 10) | bool)",
            "(number(min = 10) | money)",
            "(number(min = 10) | duration)",
            "(number(min = 10) | number)",
            "(number(min = 10) | number(max = 0))",
            "(number(min = 10) | []number(min = 5))",
            "([]number(min = 10) | []string)",
            "([]string | []number(min = 10))",
            "(set<number(min = 10)> | set<string>)",
            "([][]number(min = 10) | []string)",
            "[](number(min = 10) | string)",
            "set<number(min = 10) | string>",
            // Duration domain (RFC 0017 literals under the RFC 0018
            // facet grammar) — same parity obligation, same walk.
            "duration(min = 1s)",
            "duration(max = 1m)",
            "duration(exclusiveMin = 1s)",
            "duration(multipleOf = 250ms)",
            "duration(min = 1s, max = 2h)",
            "[]duration(min = 1s)",
            "(duration(min = 1s) | string)",
            "(duration(min = 1s) | number(min = 10))",
        ];
        const VALUES: &[&str] = &[
            "5",
            "50",
            "-5",
            "0",
            "0.25",
            "0.3",
            "10",
            "20",
            "\"x\"",
            "true",
            "[]",
            "[5]",
            "[50]",
            "[5, 3]",
            "[5, 50]",
            "[5, \"x\"]",
            "[[5]]",
            "[[5], [50]]",
            "3 | 5",
            "3 | 50",
            "50 | 60",
            "[3] | [5]",
            // Durations, mixed units deliberately: `1000ms` must
            // satisfy `min = 1s` (semantic comparison), `1500ms` must
            // fail `multipleOf = 250ms`'s neighbor `1s` but not
            // `250ms` — unit-blind, nanos-exact.
            "500ms",
            "1s",
            "1000ms",
            "1500ms",
            "90s",
            "2h",
            "3h",
            "[1s, 500ms]",
            "500ms | 2s",
        ];
        // The two surfaces prefix the subject differently; the RULE
        // text after it is what must match.
        let tails = |ds: &[Diagnostic]| -> Vec<String> {
            let mut v: Vec<String> = ds
                .iter()
                .filter(|d| d.code == Some(nml_core::diagnostic::codes::FACET_VIOLATION))
                .map(|d| {
                    let m = d.rendered_message();
                    m.strip_prefix("default for ").unwrap_or(&m).to_string()
                })
                .collect();
            v.sort();
            v
        };
        let mut compared = 0usize;
        for ty in TYPES {
            for lit in VALUES {
                // B: the literal as a config VALUE. Skip shapes whose
                // schema or config cannot parse — not our subject.
                let schema = format!("model m:\n    p {ty}?\n");
                let Ok(cfg) = nml_core::cst::parse_to_ast(&format!("m X:\n    p = {lit}\n")) else {
                    continue;
                };
                let b = tails(&make_validator(&schema).validate(&cfg));
                // A: the same literal as a declared DEFAULT.
                let as_default = format!("model m:\n    p {ty} = {lit}\n");
                let (_s, a_diags) = nml_core::cst::extract_schema(&as_default);
                if a_diags.iter().any(|d| {
                    d.severity == Severity::Error
                        && d.code != Some(nml_core::diagnostic::codes::FACET_VIOLATION)
                }) {
                    continue; // the default itself failed to parse/declare
                }
                let a = tails(&a_diags);
                assert_eq!(
                    a, b,
                    "parity broken for `{ty}` = `{lit}`:\n  default: {a:?}\n  value:   {b:?}"
                );
                compared += 1;
            }
        }
        assert!(
            compared >= 400,
            "expected a broad sweep, only compared {compared} pairs"
        );
    }

    /// RFC 0018 §1.2's by-construction promise, pinned at the path
    /// that actually broke it: `load_schema` — what packages, the
    /// loader and downstream boots use. A violating DEFAULT used to
    /// load clean here (only the `validate` verb caught it) and then
    /// materialize into runtime config silently.
    #[test]
    fn violating_defaults_are_caught_by_the_loader_not_just_the_cli() {
        let src = "model root:\n    retries number(min = 0) = -1\n";
        let (_schema, diags) = crate::loader::load_schema(&[("s.nml", src)]);
        assert!(
            diags.iter().any(|d| {
                d.code == Some(nml_core::diagnostic::codes::FACET_VIOLATION)
                    && d.rendered_message().contains("below the schema's min = 0")
            }),
            "load_schema must reject a facet-violating default: {diags:?}"
        );
        // Element-wise for collections, and a satisfying default is clean.
        let (_s, arr) = crate::loader::load_schema(&[(
            "s.nml",
            "model root:\n    ports []number(min = 1) = [1, 0]\n",
        )]);
        assert!(
            arr.iter()
                .any(|d| d.code == Some(nml_core::diagnostic::codes::FACET_VIOLATION)),
            "collection defaults are checked element-wise: {arr:?}"
        );
        let (_s, ok) = crate::loader::load_schema(&[(
            "s.nml",
            "model root:\n    retries number(min = 0) = 3\n",
        )]);
        assert!(
            ok.iter()
                .all(|d| d.code != Some(nml_core::diagnostic::codes::FACET_VIOLATION)),
            "a satisfying default stays clean: {ok:?}"
        );
    }

    /// RFC 0018 §1.1 lists modifier fields as a facet position, and a
    /// typed modifier lowers to `FieldType::Modifier(inner)` — so
    /// enforcement must recurse into it. Pinned beside the nml-core pin
    /// for the DECLARATION rules on the same construct: the two halves
    /// have to agree, or a modifier is checked by one and not the other.
    #[test]
    fn modifier_facets_are_enforced() {
        let schema = "model svc:\n    name string+\n    |cap number(min = 1)?\n";
        let d = diags(schema, "svc A:\n    |cap = 0\n");
        assert!(
            d.iter().any(|x| {
                x.code == Some(nml_core::diagnostic::codes::FACET_VIOLATION)
                    && x.rendered_message().contains("below the schema's min = 1")
            }),
            "modifier facets must bind values: {d:?}"
        );
        let ok = diags(schema, "svc A:\n    |cap = 5\n");
        assert!(
            ok.iter().all(|x| x.severity != Severity::Error),
            "in-range modifier value must pass: {ok:?}"
        );
    }

    /// RFC 0018 §2 definition-side fixtures (NML2058) — plus the
    /// violating-default case, which reports through the SHARED
    /// enforcement pass as NML2057.
    #[test]
    fn facet_definitions_are_validated() {
        // Facet definition rules emit from extract_schema — the single
        // choke point every surface constructs through.
        let def_diags = |schema: &str| nml_core::cst::extract_schema(schema).1;
        let bad_def = |schema: &str, needle: &str| {
            let d = def_diags(schema);
            let hit = d.iter().find(|x| {
                x.code == Some(nml_core::diagnostic::codes::FACET_DEFINITION)
                    && x.rendered_message().contains(needle)
            });
            assert!(
                hit.is_some(),
                "expected NML2058 containing {needle:?}, got {d:?}"
            );
        };
        bad_def(
            "model m:\n    s string(min = 1)\n",
            "facets attach only to `number`",
        );
        bad_def(
            "model m:\n    r denial(min = 1)\n",
            "facets attach only to `number`",
        );
        bad_def(
            "model m:\n    x number(min = 2, max = 1)\n",
            "unsatisfiable",
        );
        bad_def(
            "model m:\n    x number(exclusiveMin = 1, max = 1)\n",
            "unsatisfiable",
        );
        bad_def(
            "model m:\n    x number(multipleOf = 0)\n",
            "must be positive",
        );
        bad_def(
            "model m:\n    x number(multipleOf = -0.5)\n",
            "must be positive",
        );
        bad_def(
            "model m:\n    x number(min = 1, exclusiveMin = 0)\n",
            "mutually exclusive",
        );
        bad_def(
            "model m:\n    x number(min = 1, min = 2)\n",
            "duplicate facet",
        );
        bad_def("model m:\n    x number(floor = 1)\n", "unknown facet");
        bad_def(
            "model m:\n    xs set<number(multipleOf = 0)>\n",
            "must be positive",
        );
        // min == max INCLUSIVE is satisfiable (the single-point range).
        let d = def_diags("model m:\n    x number(min = 5, max = 5)\n");
        assert!(
            d.iter()
                .all(|x| x.code != Some(nml_core::diagnostic::codes::FACET_DEFINITION)),
            "single-point range is legal: {d:?}"
        );
        // A default violating its own facets reports as NML2057 —
        // ONCE. It is owned by the by-construction pass
        // (`facet_definition_diagnostics`, which every consumer gets);
        // the definitions verb deliberately drops its own facet
        // findings on defaults so one defect is not printed twice with
        // two phrasings.
        let src = "model m:\n    x number(min = 0) = -1\n";
        let by_construction: Vec<_> = nml_core::cst::extract_schema(src)
            .1
            .into_iter()
            .filter(|x| x.code == Some(nml_core::diagnostic::codes::FACET_VIOLATION))
            .collect();
        assert_eq!(
            by_construction.len(),
            1,
            "the choke point owns it: {by_construction:?}"
        );
        assert!(
            by_construction[0]
                .rendered_message()
                .contains("default for 'x' is -1, below the schema's min = 0"),
            "{by_construction:?}"
        );
        let file = nml_core::cst::parse_to_ast(src).unwrap();
        let from_verb = make_validator(src).validate_definitions(&file);
        assert!(
            from_verb
                .iter()
                .all(|x| x.code != Some(nml_core::diagnostic::codes::FACET_VIOLATION)),
            "the verb must not duplicate the by-construction finding: {from_verb:?}"
        );
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
        // Leaf × Named{non-empty body} → dropped body (NML2057).
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
    fn a_type_annotation_modifier_satisfies_no_required_field() {
        // RFC 0019 errata E12: `|deny []string` inside an instance body
        // is a declaration, never a value — the required field stays
        // missing until a real value is written.
        let schema = "model m:\n    |deny []string\n    name string\n";
        let decl_only = diags(schema, "m a:\n    |deny []string\n    name = \"n\"\n");
        assert!(
            decl_only
                .iter()
                .any(|d| d.code == Some(codes::MISSING_REQUIRED_FIELD)
                    && d.message.contains("'deny'")),
            "{decl_only:?}"
        );
        let with_value = diags(
            schema,
            "m b:\n    |deny []string\n    |deny = [\"a\"]\n    name = \"n\"\n",
        );
        assert!(
            with_value.is_empty(),
            "a real value satisfies it: {with_value:?}"
        );
    }

    #[test]
    fn a_named_item_under_a_oneof_element_credits_the_arms_positional_field() {
        // `- a: kind = "a"` under `steps []oo` where the arm declares
        // `name string+`: the name is the `+` field, exactly as under a
        // model element — no phantom NML2007 on the raw block.
        let schema = "model arma:\n    name string+\n    kind string?\n\n\
                      model armb:\n    name string+\n    kind string?\n    z string\n\n\
                      oneof oo by kind = \"a\":\n    \"a\" -> arma\n    \"b\" -> armb\n\n\
                      model flow:\n    steps []oo\n";
        let d = diags(
            schema,
            "flow f:\n    steps:\n        - a:\n            kind = \"a\"\n        - b:\n            kind = \"b\"\n            z = \"1\"\n        - c:\n",
        );
        assert!(d.is_empty(), "{d:?}");
        // The arm's other required field is still required.
        let d = diags(
            schema,
            "flow g:\n    steps:\n        - b:\n            kind = \"b\"\n",
        );
        assert_eq!(
            d.iter()
                .filter(|d| d.code == Some(codes::MISSING_REQUIRED_FIELD))
                .count(),
            1,
            "{d:?}"
        );
        assert!(d[0].message.contains("'z'"), "{}", d[0].message);
    }

    #[test]
    fn every_discriminator_entry_must_be_a_string() {
        // A first-only check laundered a dependent's `kind = 5` through the
        // composed view (the effective string discriminator is re-added
        // ahead of it): every entry of that name is checked.
        let schema = "model arma:\n    kind string\n    a string\n\n\
                      oneof oo by kind:\n    \"a\" -> arma\n\nmodel h:\n    cfg oo\n";
        let d = diags(
            schema,
            "h x:\n    cfg:\n        kind = \"a\"\n        a = \"1\"\n        kind = 5\n",
        );
        assert_eq!(
            d.iter()
                .filter(|d| d.code == Some(codes::INVALID_DISCRIMINATOR))
                .count(),
            1,
            "{d:?}"
        );
        // The count contract: every non-string entry, first or later,
        // exactly once; a duplicate STRING is not this finding.
        for (body, want) in [
            ("kind = \"a\"\n        kind = 5\n        kind = true\n", 2),
            ("kind = \"a\"\n        kind = \"a\"\n", 0),
            ("kind = 5\n        kind = \"a\"\n", 1),
            ("kind = 5\n        kind = 6\n", 2),
        ] {
            let d = diags(
                schema,
                &format!("h x:\n    cfg:\n        {body}        a = \"1\"\n"),
            );
            assert_eq!(
                d.iter()
                    .filter(|d| d.code == Some(codes::INVALID_DISCRIMINATOR))
                    .count(),
                want,
                "{body:?}: {d:?}"
            );
        }
    }

    #[test]
    fn unknown_variant_naming_a_list_element_gets_the_honest_form() {
        // `as ub` where `ub` is only a list variant's ELEMENT: not a
        // nameable variant, and "did you mean ua" would mislead — the
        // shared builder says so, with no suggestion (one message, one
        // home, byte-identical with the compose engine's).
        let diags = diags_for(
            "model ua:\n    x string\n\nmodel ub:\n    k string\n\n\
             model host:\n    slot (ua | []ub)\n",
            "host H:\n    slot as ub:\n        x = \"1\"\n",
        );
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::UNKNOWN_UNION_VARIANT))
            .expect("unknown-variant error");
        assert!(
            d.message.contains("names a list variant's element") && d.suggestions.is_empty(),
            "honest form, no did-you-mean: {d:?}"
        );
        assert_eq!(diags.len(), 1, "no guessed-variant noise: {diags:?}");
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

    /// A selector string that CANNOT be a bare role token (spaces/quotes —
    /// a consumer's quoted-value form, e.g. nudge RFC 0055 D11) is the
    /// deliberate, only-possible spelling: no warning, and crucially no
    /// suggestion echoing the (possibly PII-class) value into consumer
    /// boot logs.
    #[test]
    fn test_role_type_quoted_selector_string_is_deliberate() {
        let schema = "model service:\n    access role?\n";
        let validator = make_validator(schema);

        let source = "service Svc:\n    access = \"@user/\\\"fred & wilma@example.com\\\"\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("roles are references, not strings")),
            "the string form is the only spelling — no warning; diags: {diags:?}"
        );
        assert!(
            !format!("{diags:?}").contains("wilma"),
            "no diagnostic may echo the mailbox: {diags:?}"
        );
    }

    /// The sibling arm of the same leak class: a NON-`@` string that cannot
    /// be a bare token (a raw mailbox pasted into a role field) still gets
    /// the teaching warning, but with NO suggestion — prepending `@` would
    /// suggest an unlexable spelling AND echo the PII-class value.
    #[test]
    fn test_role_type_nontoken_string_warns_without_echo() {
        let schema = "model service:\n    access role?\n";
        let validator = make_validator(schema);

        let source = "service Svc:\n    access = \"fred & wilma@example.com\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        let warn = diags
            .iter()
            .find(|d| d.message.contains("roles are references, not strings"))
            .expect("still teaches");
        assert!(
            warn.suggestions.is_empty(),
            "no suggestion for an unlexable fix: {warn:?}"
        );
        assert!(
            !format!("{diags:?}").contains("wilma"),
            "no diagnostic may echo the mailbox: {diags:?}"
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

    /// RFC 0007 §6.2: inline arm targets validate structurally against model `V`.
    #[test]
    fn arm_inline_targets_validate_against_model_v() {
        let schema = "model landingPage:\n    label number\nmodel service:\n    routing (role -> landingPage)?\n";
        let ok = diags(
            schema,
            "service Api:\n    routing:\n        @role/admin -> adminLanding:\n            label = 4\n",
        );
        assert!(ok.is_empty(), "happy-path inline arm: {ok:?}");

        let bad_field = diags(
            schema,
            "service Api:\n    routing:\n        @role/admin -> adminLanding:\n            label = \"nope\"\n",
        );
        assert!(
            bad_field.iter().any(|d| d.message.contains("label")),
            "wrong field type in inline body: {bad_field:?}"
        );

        let bad_scalar_v = diags(
            "model service:\n    routing (role -> string)?\n",
            "service Api:\n    routing:\n        @role/admin -> adminLanding:\n            label = 4\n",
        );
        assert!(
            bad_scalar_v
                .iter()
                .any(|d| d.message.contains("inline arm target requires a model")),
            "{bad_scalar_v:?}"
        );
    }

    /// RFC 0007 §6.1: string-literal selectors validate against enum-typed `K`.
    #[test]
    fn arm_string_selector_validates_against_enum_key() {
        let schema = "enum planKind:\n    - \"free\"\n    - \"pro\"\nmodel service:\n    routing (planKind -> string)?\n";
        let validator = make_validator(schema);
        let enum_def = validator
            .find_enum("planKind")
            .expect("planKind enum in schema");
        assert!(enum_def.variants.iter().any(|v| v == "pro"));
        let ok = diags(
            schema,
            "service Api:\n    routing:\n        \"pro\" -> \"upsell\"\n",
        );
        assert!(ok.is_empty(), "valid enum key: {ok:?}");

        let bad = diags(
            schema,
            "service Api:\n    routing:\n        \"enterprise\" -> \"upsell\"\n",
        );
        assert!(
            bad.iter()
                .any(|d| d.message.contains("invalid value \"enterprise\"")),
            "unknown enum variant key: {bad:?}"
        );
    }

    /// Parser + validator: `-> "name":` is not an inline block — quotes mean literal.
    #[test]
    fn arm_quoted_target_with_colon_errors() {
        let schema = "model landingPage:\n    label number\nmodel service:\n    routing (role -> landingPage)?\n";
        let _ = schema;
        let parse = nml_core::cst::parse(
            "service Api:\n    routing:\n        @role/admin -> \"adminLanding\":\n            label = 4\n",
        );
        assert!(
            parse
                .errors()
                .iter()
                .any(|e| e.message().contains("inline block uses an unquoted name")),
            "parse should teach the quoted-colon mistake: {:?}",
            parse.errors()
        );
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
                    uses: vec![],
                    uses_span: None,
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
            FieldType::Primitive {
                ty: PrimitiveType::String,
                ..
            }
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
        // Defaults are judged where the schema is LOADED — one check
        // for every consumer (CLI verbs, packages, boots) instead of
        // one per validating surface.
        let src = "model cfg:\n    count number = \"high\"\n";
        let diags = crate::loader::load_schema(&[("s.nml", src)]).1;
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
        // The loader checks defaults BEFORE `resolve_model_inheritance`
        // copies a parent's fields (defaults included) into every child
        // — which is precisely what keeps one declaration to one
        // finding, however many models inherit it.
        let src = "model base:\n    count number = \"high\"\n\nmodel child is base:\n    extra string = \"x\"\n";
        let count = crate::loader::load_schema(&[("s.nml", src)])
            .1
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
        let check = |src: &str| crate::loader::load_schema(&[("s.nml", src)]).1;
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

    const SCHEMA: &str = "trait monitored:\n    timeout duration = 5s\n\n\
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
        // The migration: a QUOTED duration is NML0001 with the canonical
        // literal as the machine-applicable fix (spanning the whole quoted
        // string). The coercion grammar judges the string, so compound
        // spellings and exact fractions migrate too — to the canonical
        // compound literal.
        for (quoted, fix) in [
            ("\"30s\"", "30s"),
            ("\"1h 30m\"", "1h30m"),
            ("\"1.5h\"", "1h30m"),
        ] {
            let diags = diags_for(quoted);
            let hit = diags
                .iter()
                .find(|d| code_of(d).as_deref() == Some("NML0001"))
                .unwrap_or_else(|| {
                    panic!("quoted duration is the replaced-syntax migration: {diags:?}")
                });
            assert_eq!(
                hit.suggestions.first().map(|s| s.replacement.as_str()),
                Some(fix),
                "{quoted}"
            );
        }
        // A string that is NOT duration text — and every other wrong kind —
        // is the ordinary type mismatch, never a special case.
        for bad in ["\"30x\"", "\"-3s\"", "\"s\"", "\"12\"", "30", "true"] {
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
mod replaced_literal_tests {
    //! The quoted-literal migration teaching, extended beyond durations:
    //! a quoted number/bool against its typed field is the legacy
    //! spelling and gets NML0001 with the de-quoted machine fix — found
    //! empirically when `port = $ENV.PORT | "3000"` produced only the
    //! generic NML2008 while the duration twin taught the fix. Strings
    //! that do NOT parse as the target type stay NML2008 (never a
    //! special case), and string-typed fields are untouched.

    use super::*;

    fn diags_for(schema: &str, body: &str) -> Vec<Diagnostic> {
        let s = nml_core::cst::extract_schema(schema).0;
        let v = SchemaValidator::new(s.models, s.enums, s.oneofs);
        v.validate(&nml_core::cst::parse_to_ast(body).unwrap())
    }

    fn code_of(d: &Diagnostic) -> Option<String> {
        d.code.map(|c| c.to_string())
    }

    #[test]
    fn quoted_number_and_bool_teach_the_literal() {
        for (schema, body, fix) in [
            (
                "model svc:\n    port number\n",
                "svc A:\n    port = \"3000\"\n",
                "3000",
            ),
            (
                "model svc:\n    port number\n",
                "svc A:\n    port = \"1.5\"\n",
                "1.5",
            ),
            (
                "model svc:\n    admin bool\n",
                "svc A:\n    admin = \"true\"\n",
                "true",
            ),
            (
                "model svc:\n    admin bool\n",
                "svc A:\n    admin = \"false\"\n",
                "false",
            ),
        ] {
            let diags = diags_for(schema, body);
            let hit = diags
                .iter()
                .find(|d| code_of(d).as_deref() == Some("NML0001"))
                .unwrap_or_else(|| panic!("{body:?} must teach the literal: {diags:?}"));
            assert!(
                hit.message.contains("drop the quotes"),
                "teaching text: {}",
                hit.message
            );
            assert_eq!(
                hit.suggestions.first().map(|s| s.replacement.as_str()),
                Some(fix),
                "{body:?}"
            );
        }
    }

    #[test]
    fn non_literal_strings_stay_ordinary_mismatches() {
        for (schema, body) in [
            (
                "model svc:\n    port number\n",
                "svc A:\n    port = \"high\"\n",
            ),
            (
                "model svc:\n    admin bool\n",
                "svc A:\n    admin = \"yes\"\n",
            ),
            (
                "model svc:\n    admin bool\n",
                "svc A:\n    admin = \"1\"\n",
            ),
        ] {
            let diags = diags_for(schema, body);
            assert!(
                diags
                    .iter()
                    .any(|d| code_of(d).as_deref() == Some("NML2008")),
                "{body:?} must stay NML2008 (no invented coercion): {diags:?}"
            );
            assert!(
                diags
                    .iter()
                    .all(|d| code_of(d).as_deref() != Some("NML0001")),
                "{body:?} must not claim a migration fix: {diags:?}"
            );
        }
    }

    /// String-typed fields take strings — the teaching arms must never
    /// reach them, and a plain string stays diagnostics-free.
    #[test]
    fn string_fields_are_untouched() {
        let diags = diags_for(
            "model svc:\n    name string\n",
            "svc A:\n    name = \"3000\"\n",
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    /// `$ENV.KEY` on a typed field is DEFERRED (resolved later), so the
    /// teaching arms must never fire on it and the field must not error:
    /// the migration is for source LITERALS only, never references. The
    /// origin bug was `port = $ENV.PORT | "3000"` — the reference primary
    /// must stay clean while only the quoted fallback teaches.
    #[test]
    fn env_reference_on_typed_field_is_deferred_not_taught() {
        let diags = diags_for(
            "model svc:\n    port number\n    admin bool\n",
            "svc A:\n    port = $ENV.PORT\n    admin = $ENV.ADMIN\n",
        );
        assert!(
            diags.is_empty(),
            "an $ENV reference on a typed field is deferred, never taught: {diags:?}"
        );
    }

    /// The teaching grammar is the de-layer's COERCION grammar, not the
    /// literal grammar: spellings that only the old string coercion
    /// accepted ("1e-6", "1.5h") are still the replaced spelling and get
    /// the migration fix — as the CANONICAL literal (always valid
    /// source), with the "(drop the quotes)" hint withheld because
    /// de-quoting alone would not produce it.
    #[test]
    fn coercion_only_spellings_teach_the_canonical_literal() {
        for (schema, body, fix) in [
            (
                "model svc:\n    port number\n",
                "svc A:\n    port = \"1e-6\"\n",
                "0.000001",
            ),
            (
                "model svc:\n    timeout duration\n",
                "svc A:\n    timeout = \"1.5h\"\n",
                "1h30m",
            ),
        ] {
            let diags = diags_for(schema, body);
            let hit = diags
                .iter()
                .find(|d| code_of(d).as_deref() == Some("NML0001"))
                .unwrap_or_else(|| panic!("{body:?} must teach the literal: {diags:?}"));
            assert!(
                !hit.message.contains("drop the quotes"),
                "de-quoting is NOT the fix here; the hint would lie: {}",
                hit.message
            );
            assert_eq!(
                hit.suggestions.first().map(|s| s.replacement.as_str()),
                Some(fix),
                "{body:?}"
            );
        }
    }

    /// Design #2 regression pin: the validator judges EACH leg of a
    /// fallback chain, so the quoted-literal fallback (`$ENV.PORT |
    /// "3000"`) fails at validation time with the machine fix — at every
    /// boundary that validates, whether or not the variable is set. The
    /// bare typed fallback stays clean. This is the property that makes
    /// the quoted-fallback asymmetry a teaching error instead of a
    /// production sharp edge.
    #[test]
    fn quoted_fallback_leg_teaches_while_bare_fallback_stays_clean() {
        let diags = diags_for(
            "model svc:\n    port number\n",
            "svc A:\n    port = $ENV.PORT | \"3000\"\n",
        );
        let hit = diags
            .iter()
            .find(|d| code_of(d).as_deref() == Some("NML0001"))
            .unwrap_or_else(|| panic!("quoted fallback leg must teach: {diags:?}"));
        assert_eq!(
            hit.suggestions.first().map(|s| s.replacement.as_str()),
            Some("3000")
        );

        let diags = diags_for(
            "model svc:\n    port number\n",
            "svc A:\n    port = $ENV.PORT | 3000\n",
        );
        assert!(
            diags.is_empty(),
            "the typed fallback spelling is the supported form: {diags:?}"
        );
    }
}

#[cfg(test)]
mod resolved_facet_tests {
    //! RFC 0047 resolved lane: a validator that owns a file's resolution
    //! (`with_env_resolution`) enforces Number/Duration facets on what
    //! `$ENV` values resolve to — with diagnostics that name the variable
    //! and the bound, never the resolved content. Everything else about
    //! deferral is unchanged, and resolver-free validators behave exactly
    //! as before (pinned by `env_reference_on_typed_field_is_deferred_not_taught`).

    use nml_core::resolve::ValueResolver;

    use super::*;

    fn fixed(pairs: &'static [(&'static str, &'static str)]) -> ValueResolver {
        ValueResolver::new(move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        })
    }

    fn diags_with_env(
        schema: &str,
        body: &str,
        pairs: &'static [(&'static str, &'static str)],
    ) -> Vec<Diagnostic> {
        let s = nml_core::cst::extract_schema(schema).0;
        let v = SchemaValidator::new(s.models, s.enums, s.oneofs).with_env_resolution(fixed(pairs));
        v.validate(&nml_core::cst::parse_to_ast(body).unwrap())
    }

    fn facet_errors(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
        diags
            .iter()
            .filter(|d| d.code == Some(codes::FACET_VIOLATION))
            .collect()
    }

    /// The headline case — the regression the duration migration
    /// introduced: `pollInterval`'s old post-resolution clamp is gone and
    /// the facet replacing it could not see `$ENV`. Now it can, and the
    /// diagnostic names the knob (`$ENV.P`), the relation, and the bound
    /// — NEVER the resolved text (secret-provenance redaction is the
    /// no-echo posture, pinned here).
    #[test]
    fn resolved_env_facet_violation_names_the_variable_not_the_value() {
        let diags = diags_with_env(
            "model svc:\n    pollInterval duration(min = 60s)\n",
            "svc A:\n    pollInterval = $ENV.P\n",
            &[("P", "1s")],
        );
        let errs = facet_errors(&diags);
        assert_eq!(errs.len(), 1, "{diags:?}");
        let msg = &errs[0].message;
        assert!(
            msg.contains("$ENV.P") && msg.contains("below the schema's min = 60s"),
            "must name the variable and the bound: {msg}"
        );
        assert!(
            !msg.contains("1s,") && !msg.contains("is 1s"),
            "must never echo the resolved value: {msg}"
        );
    }

    /// Number lane, coercion-grammar parity: "1e-6" is exactly what the
    /// de-layer's `parse_coercion` will accept at runtime, so validation
    /// must judge the same value — not reject the spelling (literal
    /// grammar) or skip it.
    #[test]
    fn resolved_env_number_uses_the_coercion_grammar() {
        let diags = diags_with_env(
            "model svc:\n    port number(min = 1)\n",
            "svc A:\n    port = $ENV.PORT\n",
            &[("PORT", "1e-6")],
        );
        let errs = facet_errors(&diags);
        assert_eq!(errs.len(), 1, "{diags:?}");
        assert!(
            errs[0].message.contains("$ENV.PORT")
                && errs[0].message.contains("below the schema's min = 1"),
            "{}",
            errs[0].message
        );
    }

    /// The wire-granularity case the CLI's hand guard used to own:
    /// `multipleOf = 1s` now fires on the `$ENV` lane too.
    #[test]
    fn resolved_env_subsecond_violates_whole_second_facet() {
        let diags = diags_with_env(
            "model svc:\n    drain duration(multipleOf = 1s)?\n",
            "svc A:\n    drain = $ENV.D\n",
            &[("D", "900ms")],
        );
        let errs = facet_errors(&diags);
        assert_eq!(errs.len(), 1, "{diags:?}");
        assert!(
            errs[0].message.contains("$ENV.D")
                && errs[0].message.contains("multipleOf = 1s")
                && !errs[0].message.contains("900ms"),
            "{}",
            errs[0].message
        );
    }

    /// In-range resolutions are silent — the lane adds checks, never noise.
    #[test]
    fn resolved_env_within_facets_is_silent() {
        let diags = diags_with_env(
            "model svc:\n    pollInterval duration(min = 60s)\n    port number(min = 1)\n",
            "svc A:\n    pollInterval = $ENV.P\n    port = $ENV.PORT\n",
            &[("P", "5m"), ("PORT", "8080")],
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    /// Unset variables defer exactly as without a resolver: a missing
    /// variable is deserialization's business (fallbacks, its own error),
    /// never a validation failure — the no-false-reject property.
    #[test]
    fn unset_env_defers_silently() {
        let diags = diags_with_env(
            "model svc:\n    pollInterval duration(min = 60s)\n",
            "svc A:\n    pollInterval = $ENV.UNSET\n",
            &[],
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    /// Text outside the domain's coercion grammar is left to the
    /// de-layer's provenance-aware coercion error (which names the
    /// variable); the facet lane must not double-report or guess.
    #[test]
    fn unparseable_env_text_is_deserializations_business() {
        let diags = diags_with_env(
            "model svc:\n    pollInterval duration(min = 60s)\n",
            "svc A:\n    pollInterval = $ENV.P\n",
            &[("P", "banana")],
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    /// A fallback whose primary resolves is judged on the RESOLVED value;
    /// the literal legs were already judged by the normal facet arm
    /// (leg-splitting), so `$ENV.D | 5s` with a bad `D` errors on the
    /// resolved lane while the same chain with `D` unset is clean.
    #[test]
    fn fallback_primary_is_checked_when_it_resolves() {
        let schema = "model svc:\n    drain duration(multipleOf = 1s)?\n";
        let body = "svc A:\n    drain = $ENV.D | 5s\n";
        let diags = diags_with_env(schema, body, &[("D", "900ms")]);
        assert_eq!(facet_errors(&diags).len(), 1, "{diags:?}");

        let diags = diags_with_env(schema, body, &[]);
        assert!(
            diags.is_empty(),
            "unset primary → literal fallback lane: {diags:?}"
        );
    }

    /// `secret`-typed fields carry no facets (`PrimitiveFacets::None`), so
    /// the resolved lane is structurally unable to touch credentials —
    /// with a resolver configured and the variable SET, a secret field
    /// stays diagnostic-free.
    #[test]
    fn secret_fields_never_enter_the_resolved_lane() {
        let diags = diags_with_env(
            "model svc:\n    apiKey secret\n",
            "svc A:\n    apiKey = $ENV.KEY\n",
            &[("KEY", "hunter2")],
        );
        assert!(diags.is_empty(), "{diags:?}");
        // Belt-and-braces: nothing anywhere in any diagnostic may carry
        // the resolved content.
        assert!(diags.iter().all(|d| !d.message.contains("hunter2")));
    }

    /// Wiring 2 — the per-model shallow walk for split-ownership files
    /// (the deploy CLI's `build:`/`deploy:` blocks): same leaf, same
    /// no-echo posture, driven by an explicit resolver argument.
    #[test]
    fn shallow_walk_checks_resolved_properties_against_the_named_model() {
        let s = nml_core::cst::extract_schema(
            "model deploy:\n    drain_timeout duration(max = 5m, multipleOf = 1s)?\n",
        )
        .0;
        let v = SchemaValidator::new(s.models, s.enums, s.oneofs);
        // Blocks are `keyword Name:` — the CLI matches on the keyword
        // (`block.keyword.name`), never the instance name.
        let file = nml_core::parse("deploy D:\n    drain_timeout = $ENV.D\n").unwrap();
        let body = match &file.declarations[0].kind {
            nml_core::ast::DeclarationKind::Block(b) => &b.body,
            other => panic!("fixture must parse as a block: {other:?}"),
        };

        let diags = v.validate_resolved_facets(body, "deploy", &fixed(&[("D", "900ms")]));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(
            diags[0].message.contains("$ENV.D")
                && diags[0].message.contains("multipleOf = 1s")
                && !diags[0].message.contains("900ms"),
            "{}",
            diags[0].message
        );

        // Unset variable: silent (deserialization's business).
        assert!(
            v.validate_resolved_facets(body, "deploy", &fixed(&[]))
                .is_empty()
        );

        // Literal values are the whole-file pass's job — the shallow walk
        // must not double-report them.
        let file = nml_core::parse("deploy D:\n    drain_timeout = 900ms\n").unwrap();
        let body = match &file.declarations[0].kind {
            nml_core::ast::DeclarationKind::Block(b) => &b.body,
            other => panic!("fixture must parse as a block: {other:?}"),
        };
        assert!(
            v.validate_resolved_facets(body, "deploy", &fixed(&[("D", "900ms")]))
                .is_empty()
        );
    }

    /// Fallback chains resolve ONCE, primary-wins — the same selection
    /// deserialization makes. Judging legs individually would reject a
    /// config the runtime runs happily: `$ENV.OVERRIDE | $ENV.STALE` with
    /// a valid override and a stale-invalid baseline. Both walks must
    /// agree, since they share the leaf.
    #[test]
    fn fallback_resolves_once_primary_wins_in_both_walks() {
        let schema = "model svc:\n    drain duration(multipleOf = 1s)?\n";
        let body = "svc A:\n    drain = $ENV.OVERRIDE | $ENV.STALE\n";
        let env: &'static [(&'static str, &'static str)] =
            &[("OVERRIDE", "5s"), ("STALE", "900ms")];

        // Deep walk: the dead leg must NOT be judged.
        let diags = diags_with_env(schema, body, env);
        assert!(
            diags.is_empty(),
            "a losing fallback leg must never fail validation: {diags:?}"
        );

        // Shallow walk: identical verdict, same leaf.
        let s = nml_core::cst::extract_schema(schema).0;
        let v = SchemaValidator::new(s.models, s.enums, s.oneofs);
        let file = nml_core::parse("deploy D:\n    drain = $ENV.OVERRIDE | $ENV.STALE\n").unwrap();
        let block_body = match &file.declarations[0].kind {
            nml_core::ast::DeclarationKind::Block(b) => &b.body,
            other => panic!("fixture must parse as a block: {other:?}"),
        };
        let s2 =
            nml_core::cst::extract_schema("model deploy:\n    drain duration(multipleOf = 1s)?\n")
                .0;
        let v2 = SchemaValidator::new(s2.models, s2.enums, s2.oneofs);
        assert!(
            v2.validate_resolved_facets(block_body, "deploy", &fixed(env))
                .is_empty(),
            "shallow walk must agree with the deep walk on primary-wins"
        );
        let _ = v;

        // …and when the WINNING leg violates, both still catch it.
        let bad: &'static [(&'static str, &'static str)] =
            &[("OVERRIDE", "900ms"), ("STALE", "5s")];
        assert_eq!(facet_errors(&diags_with_env(schema, body, bad)).len(), 1);
        assert_eq!(
            v2.validate_resolved_facets(block_body, "deploy", &fixed(bad))
                .len(),
            1
        );
    }

    /// A chain of THREE OR MORE legs parses right-associative
    /// (`a | b | c` → `Fallback(a, Fallback(b, c))`), so the type split
    /// must RECURSE. A non-recursive split hands the nested tail to the
    /// type match and reports a bogus "expected number, got fallback" —
    /// a hard false reject on every resolver-free surface (`nml check`,
    /// the LSP, tenant-upload validation), for a chain the runtime
    /// resolves happily.
    #[test]
    fn multi_leg_fallback_chains_are_not_type_mismatches() {
        for (schema, body) in [
            (
                "model svc:\n    port number\n",
                "svc A:\n    port = $ENV.A | $ENV.B | 3000\n",
            ),
            (
                "model svc:\n    port number\n",
                "svc A:\n    port = $ENV.A | $ENV.B | $ENV.C | 3000\n",
            ),
            (
                "model svc:\n    t duration\n",
                "svc A:\n    t = $ENV.A | $ENV.B | 5s\n",
            ),
            (
                "model svc:\n    name string\n",
                "svc A:\n    name = $ENV.A | $ENV.B | \"x\"\n",
            ),
        ] {
            // Resolver-free: the lane is off, so this is pure type checking.
            let s = nml_core::cst::extract_schema(schema).0;
            let v = SchemaValidator::new(s.models, s.enums, s.oneofs);
            let diags = v.validate(&nml_core::cst::parse_to_ast(body).unwrap());
            assert!(
                diags.is_empty(),
                "{body:?} must type-check clean: {diags:?}"
            );

            // And with the lane on, still clean when nothing is set.
            let diags = diags_with_env(schema, body, &[]);
            assert!(diags.is_empty(), "{body:?} with a resolver: {diags:?}");
        }
    }

    /// A fallback whose winning leg is a facet-violating LITERAL must be
    /// reported ONCE. The leg split already judges literals; the resolved
    /// lane resolving the chain to that same literal must not echo it a
    /// second time at the chain span.
    #[test]
    fn fallback_to_violating_literal_reports_once() {
        let schema = "model svc:\n    port number(min = 4000)\n";
        let body = "svc A:\n    port = 3000 | $ENV.P\n";
        for env in [
            &[][..],
            &[("P", "9000")][..], // primary literal wins regardless
        ] {
            let diags = diags_with_env(schema, body, unsafe {
                // The helper takes &'static; these literals are static.
                std::mem::transmute::<&[(&str, &str)], &'static [(&'static str, &'static str)]>(env)
            });
            assert_eq!(
                facet_errors(&diags).len(),
                1,
                "one violation, one report: {diags:?}"
            );
        }
    }

    /// An unfaceted field must not have its `$ENV` READ during
    /// validation. For a `secret` field that means the credential is
    /// never even materialized — outside the lane's resolution, not just
    /// outside its judgment.
    #[test]
    fn unfaceted_fields_are_never_resolved_by_the_lane() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static READS: AtomicUsize = AtomicUsize::new(0);
        READS.store(0, Ordering::SeqCst);

        let s = nml_core::cst::extract_schema(
            "model svc:\n    apiKey secret\n    label string\n    port number(min = 1)\n",
        )
        .0;
        let v = SchemaValidator::new(s.models, s.enums, s.oneofs).with_env_resolution(
            ValueResolver::new(|k| {
                READS.fetch_add(1, Ordering::SeqCst);
                (k == "PORT").then(|| "8080".to_string())
            }),
        );
        let diags = v.validate(
            &nml_core::cst::parse_to_ast(
                "svc A:\n    apiKey = $ENV.KEY\n    label = $ENV.LABEL\n    port = $ENV.PORT\n",
            )
            .unwrap(),
        );
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(
            READS.load(Ordering::SeqCst),
            1,
            "only the faceted field may be resolved — secret/string fields must not be read"
        );
    }

    /// A `const` reference is a deferred value too. Resolved through a
    /// symbol table it yields an ordinary literal — which must still face
    /// its facets, or a faceted field could dodge its bounds by hiding
    /// the value behind a `const`. Authored in-file and secret-free, so
    /// it takes the ordinary value-echoing message.
    #[test]
    fn const_referenced_values_are_facet_checked() {
        let s = nml_core::cst::extract_schema("model svc:\n    port number(min = 10)\n").0;
        let resolver = ValueResolver::without_env()
            .with_symbols(|name| (name == "LOW").then(|| Value::Number("5".parse().unwrap())));
        let v = SchemaValidator::new(s.models, s.enums, s.oneofs).with_env_resolution(resolver);
        let diags = v.validate(&nml_core::cst::parse_to_ast("svc A:\n    port = LOW\n").unwrap());
        let errs = facet_errors(&diags);
        assert_eq!(errs.len(), 1, "a const must not dodge facets: {diags:?}");
        assert!(
            errs[0].message.contains("below the schema's min = 10"),
            "{}",
            errs[0].message
        );
    }

    /// The shallow walk resolves the WHOLE property value, so fallback
    /// chains behave exactly like deserialization: primary set-and-bad →
    /// error; primary unset → the literal leg (already judged by the
    /// whole-file pass) and silence here.
    #[test]
    fn shallow_walk_resolves_fallback_chains_like_deserialization() {
        let s = nml_core::cst::extract_schema(
            "model deploy:\n    drain_timeout duration(multipleOf = 1s)?\n",
        )
        .0;
        let v = SchemaValidator::new(s.models, s.enums, s.oneofs);
        let file = nml_core::parse("deploy D:\n    drain_timeout = $ENV.D | 5s\n").unwrap();
        let body = match &file.declarations[0].kind {
            nml_core::ast::DeclarationKind::Block(b) => &b.body,
            other => panic!("fixture must parse as a block: {other:?}"),
        };
        assert_eq!(
            v.validate_resolved_facets(body, "deploy", &fixed(&[("D", "900ms")]))
                .len(),
            1
        );
        assert!(
            v.validate_resolved_facets(body, "deploy", &fixed(&[]))
                .is_empty()
        );
    }

    /// "Once per declared value" is about the VALUE, not about how many
    /// type layers the schema wraps around it. A faceted primitive behind
    /// a `Modifier` resolves exactly ONCE for the whole fallback chain —
    /// and, because resolution short-circuits on the first leg that
    /// succeeds, that means the winning leg only.
    ///
    /// The wrapper arms re-enter the TYPE-ONLY door for exactly this
    /// reason. Routing them back through the public door instead skips
    /// the whole-chain check (the guard sees `Modifier`, not `Primitive`)
    /// and then runs the lane once per LEG underneath — reading variables
    /// the runtime never reads, and reporting a bound violation in a leg
    /// that never wins. The read count is what makes both halves visible.
    #[test]
    fn a_wrapped_faceted_field_resolves_once_for_the_whole_chain() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static READS: AtomicUsize = AtomicUsize::new(0);
        READS.store(0, Ordering::SeqCst);

        let s = nml_core::cst::extract_schema("model svc:\n    |window duration(min = 60s)?\n").0;
        assert!(
            matches!(&s.models[0].fields[0].field_type, FieldType::Modifier(_)),
            "fixture must exercise the wrapper arm: {:?}",
            s.models[0].fields[0].field_type
        );
        let v = SchemaValidator::new(s.models, s.enums, s.oneofs).with_env_resolution(
            ValueResolver::new(|k| {
                READS.fetch_add(1, Ordering::SeqCst);
                (k == "GOOD").then(|| "90s".to_string())
            }),
        );
        let diags = v.validate(
            &nml_core::cst::parse_to_ast("svc A:\n    window = $ENV.GOOD | $ENV.STALE\n").unwrap(),
        );

        assert!(
            facet_errors(&diags).is_empty(),
            "the winning leg satisfies the bound; a losing leg must not be judged: {diags:?}"
        );
        assert_eq!(
            READS.load(Ordering::SeqCst),
            1,
            "the chain must resolve once, short-circuiting on the winning leg"
        );
    }

    /// Unions are OUTSIDE the lane, deliberately and uniformly: the
    /// shallow walk's `scalar_facets` returns `None` for them, the
    /// embedder's lane census does not count them, and the deep walk now
    /// agrees. `any-variant-admits` has no single facet set to judge a
    /// resolved value against, so judging one would mean picking a
    /// variant for the user. What must NOT happen is the middle ground
    /// the wrapper bug produced: resolving the value once per candidate
    /// variant per fallback leg.
    #[test]
    fn union_variants_do_not_each_resolve_the_value() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static READS: AtomicUsize = AtomicUsize::new(0);
        READS.store(0, Ordering::SeqCst);

        let s = nml_core::cst::extract_schema(
            "model svc:\n    port (number(min = 10) | number(min = 20))\n",
        )
        .0;
        let v = SchemaValidator::new(s.models, s.enums, s.oneofs).with_env_resolution(
            ValueResolver::new(|k| {
                READS.fetch_add(1, Ordering::SeqCst);
                (k == "A").then(|| "50".to_string())
            }),
        );
        let _ = v.validate(
            &nml_core::cst::parse_to_ast("svc A:\n    port = $ENV.A | $ENV.B\n").unwrap(),
        );
        assert_eq!(
            READS.load(Ordering::SeqCst),
            0,
            "a union field is outside the lane; no variant may resolve its value"
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

    const SCHEMA: &str = "trait monitored:\n    timeout duration = 5s\n\n\
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
