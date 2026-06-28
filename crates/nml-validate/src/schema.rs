use std::collections::HashMap;

use nml_core::ast::*;
use nml_core::model::{EnumDef, FieldDef, FieldType, ModelDef, OneOfDef};
use nml_core::schema::{report_graph_cycles, ExtractedSchema};
use nml_core::schema_index::{FieldTarget, SchemaIndex};
use nml_core::span::Span;
use nml_core::types::{PrimitiveType, Value};

use crate::diagnostics::{Diagnostic, Severity};

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
pub struct SchemaValidator {
    index: SchemaIndex,
    valid_modifiers: Vec<String>,
    strict_unknown_fields: bool,
    membership: MembershipSemantics,
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

impl SchemaValidator {
    pub fn new(models: Vec<ModelDef>, enums: Vec<EnumDef>, oneofs: Vec<OneOfDef>) -> Self {
        Self {
            index: SchemaIndex::build(models, enums, oneofs),
            valid_modifiers: Vec::new(),
            strict_unknown_fields: false,
            membership: MembershipSemantics::default(),
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

    fn unknown_property_diagnostic(&self, message: String) -> Diagnostic {
        if self.strict_unknown_fields {
            Diagnostic::error(message)
        } else {
            Diagnostic::warning(message)
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

    /// Validate a parsed NML file against the loaded models.
    pub fn validate(&self, file: &File) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for decl in &file.declarations {
            match &decl.kind {
                DeclarationKind::Block(block) => {
                    self.validate_block(block, &mut diagnostics);
                }
                DeclarationKind::Array(arr) => {
                    self.validate_array(arr, &mut diagnostics);
                }
                // `oneof` declarations are schema definitions, validated when
                // the schema is loaded; they carry no instance data here.
                DeclarationKind::Const(_)
                | DeclarationKind::Template(_)
                | DeclarationKind::OneOf(_) => {}
            }
        }

        self.validate_member_cycles(file, &mut diagnostics);

        diagnostics
    }

    fn validate_block(&self, block: &BlockDecl, diags: &mut Vec<Diagnostic>) {
        let keyword = &block.keyword.name;
        let is_schema_def = matches!(keyword.as_str(), "model" | "enum");

        if keyword == "model" {
            for parent in &block.extends {
                if self.find_model(&parent.name).is_none() {
                    diags.push(
                        Diagnostic::error(format!(
                            "unknown parent model '{}' in extends clause",
                            parent.name
                        ))
                        .with_span(parent.span),
                    );
                }
            }
            self.validate_field_defaults(block, diags);
        }

        self.validate_body(&block.body, is_schema_def, keyword, diags);
        self.validate_members_builtin_refs(&block.body, keyword, diags);

        if !is_schema_def {
            // A block declaration (`role editor:`) fills its model's `name` field from
            // the block name — lenient: an explicit `name` in the body wins (RFC 0005
            // §5). `oneof`/other targets keep the prior path.
            let resolved = match self.index.resolve_ref(keyword) {
                FieldTarget::Model(m) => {
                    let result =
                        nml_core::identity::materialize_named(&block.name, &block.body, m);
                    self.validate_materialized(result, m, 0, Some(block.name.span), diags);
                    true
                }
                other => self.validate_target_instance(
                    other,
                    &block.body,
                    0,
                    Some(block.name.span),
                    diags,
                ),
            };
            if !resolved && self.strict_unknown_fields {
                diags.push(
                    Diagnostic::error(format!("block keyword '{keyword}' has no model definition"))
                        .with_span(block.keyword.span),
                );
            }
        }
    }

    fn validate_array(&self, arr: &ArrayDecl, diags: &mut Vec<Diagnostic>) {
        for modifier in &arr.body.modifiers {
            self.validate_modifier_name(modifier, diags);
            self.validate_modifier_content(modifier, diags);
        }

        let keyword = &arr.item_keyword.name;
        let is_schema_def = matches!(keyword.as_str(), "model" | "enum");
        // An array item keyword may name a model or a `oneof`, mirroring the
        // block-keyword dispatch in `validate_block` — same single `resolve_ref`.
        let resolves = !is_schema_def
            && matches!(
                self.index.resolve_ref(keyword),
                FieldTarget::Model(_) | FieldTarget::OneOf(_)
            );

        // Inline definitions (named or scalar) need a model/oneof to validate against;
        // references/links don't. A scalar `- "/api"` under an undefined keyword is as
        // malformed as a named one, so both count toward the strict "no definition" check.
        let has_inline_items = arr.body.items.iter().any(|i| {
            matches!(
                &i.kind,
                ListItemKind::Named { .. } | ListItemKind::Shorthand { .. }
            )
        });

        if !is_schema_def && !resolves && has_inline_items && self.strict_unknown_fields {
            diags.push(
                Diagnostic::error(format!(
                    "array item keyword '{keyword}' has no model or oneof definition"
                ))
                .with_span(arr.item_keyword.span),
            );
        }

        for item in &arr.body.items {
            match &item.kind {
                ListItemKind::Named { name, body } => {
                    self.validate_body(body, is_schema_def, keyword, diags);
                    self.validate_members_builtin_refs(body, keyword, diags);
                    if !is_schema_def {
                        self.validate_list_item(item, keyword, Some(name.span), diags);
                    }
                }
                // A bare scalar (`- "/api"`) fills the element model's shorthand
                // field; validate its materialized body (RFC 0005 §10).
                ListItemKind::Shorthand { value, .. } if !is_schema_def => {
                    self.validate_list_item(item, keyword, Some(value.span), diags);
                }
                _ => {}
            }
        }
    }

    /// Validate a list item under `keyword` against its element model, after
    /// **materializing the item's identity** into the body (RFC 0005 §10): a named
    /// item's `name`, or a bare scalar's shorthand field, becomes a present property
    /// so the required-field scan sees it. A scalar on a union list is out of scope
    /// and flagged explicitly; other non-model targets keep the prior path.
    fn validate_list_item(
        &self,
        item: &ListItem,
        keyword: &str,
        header_span: Option<Span>,
        diags: &mut Vec<Diagnostic>,
    ) {
        let target = self.index.resolve_ref(keyword);
        if let FieldTarget::Model(m) = target {
            let result = nml_core::identity::materialize_item(item, m);
            self.validate_materialized(result, m, 0, header_span, diags);
            return;
        }
        match &item.kind {
            // A union element's variant isn't known until its discriminator is
            // resolved (after materialization), so scalar shorthand on a union is out
            // of scope — flagged explicitly rather than silently dropped (§10).
            ListItemKind::Shorthand { value, .. } if matches!(target, FieldTarget::OneOf(_)) => {
                diags.push(Diagnostic::error(UNION_SHORTHAND_MSG.to_string()).with_span(value.span))
            }
            ListItemKind::Named { body, .. } => {
                self.validate_target_instance(target, body, 0, header_span, diags);
            }
            _ => {}
        }
    }

    /// Surface a materialization's diagnostics (the only one is dropped-key) and
    /// validate the enriched body against `model` — unless the item is unplaceable (a
    /// scalar with no shorthand field), in which case the required-field scan is
    /// skipped so it doesn't pile noise on the dropped-key diagnostic. The single
    /// "materialize → validate" path shared by list items (`materialize_item`) and
    /// block declarations (`materialize_named`).
    fn validate_materialized(
        &self,
        result: nml_core::identity::Materialized,
        model: &ModelDef,
        depth: u32,
        header: Option<Span>,
        diags: &mut Vec<Diagnostic>,
    ) {
        for err in result.diagnostics {
            diags.push(Diagnostic {
                message: err.message().to_string(),
                severity: Severity::Error,
                span: Some(err.span()),
            });
        }
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
        for entry in &body.entries {
            match &entry.kind {
                BodyEntryKind::Modifier(m) => {
                    self.validate_modifier_name(m, diags);
                    self.validate_modifier_content(m, diags);
                }
                BodyEntryKind::FieldDefinition(_) if !is_schema_def => {
                    diags.push(
                        Diagnostic::error(format!(
                            "field definitions are only allowed in model declarations, not '{keyword}'"
                        ))
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

    fn validate_modifier_name(&self, m: &Modifier, diags: &mut Vec<Diagnostic>) {
        if self.valid_modifiers.is_empty() {
            return;
        }
        if !self.valid_modifiers.iter().any(|v| v == &m.name.name) {
            diags.push(
                Diagnostic::warning(format!(
                    "unknown modifier '|{}'; expected one of: {}",
                    m.name.name,
                    self.valid_modifiers
                        .iter()
                        .map(|s| format!("|{s}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .with_span(m.name.span),
            );
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
        self.validate_target_instance(self.index.resolve_ref(ref_name), body, depth, header_span, diags)
    }

    /// Validate `body` against an already-resolved [`FieldTarget`]. The single
    /// dispatch on a resolved target, shared by keyword/ref dispatch
    /// ([`Self::validate_ref_instance`]) and union variant selection (via
    /// [`SchemaIndex::resolve_type_in_body`]). A `ListOf` target validates each
    /// named item against the item target. Returns whether the target carried
    /// instance structure (model / oneof / list of those).
    fn validate_target_instance(
        &self,
        target: FieldTarget,
        body: &Body,
        depth: u32,
        header_span: Option<Span>,
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
            FieldTarget::ListOf(inner) => {
                for entry in &body.entries {
                    let BodyEntryKind::ListItem(item) = &entry.kind else {
                        continue;
                    };
                    let header = match &item.kind {
                        ListItemKind::Named { name, .. } => Some(name.span),
                        ListItemKind::Shorthand { value, .. } => Some(value.span),
                        // References/links carry no inline instance to validate.
                        ListItemKind::Reference(_) | ListItemKind::Role(_) => continue,
                    };
                    match inner.as_ref() {
                        // Inline items (named or scalar) validate against the model
                        // *after* identity materialization (RFC 0005 §10).
                        FieldTarget::Model(m) => {
                            let result = nml_core::identity::materialize_item(item, m);
                            self.validate_materialized(result, m, depth, header, diags);
                        }
                        FieldTarget::OneOf(o) => match &item.kind {
                            ListItemKind::Named { body: item_body, .. } => {
                                self.validate_instance_against_oneof(
                                    item_body, o, depth, header, diags,
                                );
                            }
                            // Scalar shorthand on a union list is out of scope (§10).
                            ListItemKind::Shorthand { value, .. } => diags.push(
                                Diagnostic::error(UNION_SHORTHAND_MSG.to_string())
                                    .with_span(value.span),
                            ),
                            _ => {}
                        },
                        _ => {}
                    }
                }
                true
            }
            FieldTarget::Object | FieldTarget::Union | FieldTarget::Leaf => false,
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
            let mut diag = Diagnostic::warning(format!(
                "validation truncated: nesting exceeds maximum depth of {MAX_VALIDATION_DEPTH}; deeper entries were not checked"
            ));
            if let Some(span) = header_span.or_else(|| body.entries.first().map(|e| e.span)) {
                diag = diag.with_span(span);
            }
            diags.push(diag);
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
                        diags.push(
                            self.unknown_property_diagnostic(format!(
                                "unknown property '{name}' (not defined in model '{}')",
                                model.name
                            ))
                            .with_span(prop.name.span),
                        );
                    }
                }
                BodyEntryKind::NestedBlock(nb) => {
                    seen_fields.push(&nb.name.name);

                    if let Some(field_def) = model.fields.iter().find(|f| f.name == nb.name.name) {
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
                                // Each named item resolves its inner type against
                                // its own body (so a `(a | b)` union variant is
                                // picked per item) via the shared dispatch; a
                                // `ModelRef` inner resolves body-independently.
                                for entry in &nb.body.entries {
                                    if let BodyEntryKind::ListItem(item) = &entry.kind {
                                        if let ListItemKind::Named { name, body } = &item.kind {
                                            let target =
                                                self.index.resolve_type_in_body(inner, body);
                                            self.validate_target_instance(
                                                target,
                                                body,
                                                depth + 1,
                                                Some(name.span),
                                                diags,
                                            );
                                        }
                                    }
                                }
                            }
                            FieldType::Primitive(PrimitiveType::Object) => {}
                            _ => {}
                        }
                    } else {
                        diags.push(
                            self.unknown_property_diagnostic(format!(
                                "unknown property '{name}' (not defined in model '{model_name}')",
                                name = nb.name.name,
                                model_name = model.name
                            ))
                            .with_span(nb.name.span),
                        );
                    }
                }
                BodyEntryKind::Modifier(m) => {
                    seen_fields.push(&m.name.name);

                    if let Some(field_def) = model.fields.iter().find(|f| f.name == m.name.name) {
                        self.validate_modifier_value(m, field_def, diags);
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
            let mut diag = Diagnostic::warning(format!(
                "validation truncated: nesting exceeds maximum depth of {MAX_VALIDATION_DEPTH}; deeper entries were not checked"
            ));
            if let Some(span) = header_span.or_else(|| body.entries.first().map(|e| e.span)) {
                diag = diag.with_span(span);
            }
            diags.push(diag);
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
                .with_span(discriminator.value.span),
            );
            return;
        };

        let Some((_, model_name)) = oneof.variants.iter().find(|(v, _)| v == value) else {
            diags.push(
                Diagnostic::error(format!(
                    "unknown {} \"{}\" for oneof '{}'; expected one of: {}",
                    oneof.discriminator,
                    value,
                    oneof.name,
                    valid_values(),
                ))
                .with_span(discriminator.value.span),
            );
            return;
        };

        // The variant model is guaranteed to exist (checked at schema-load
        // time by `find_oneof_errors`); skip silently if the schema was built
        // without that check.
        let Some(variant_model) = self.find_model(model_name) else {
            return;
        };

        // Validate everything except the discriminator against the variant.
        let variant_body = Body {
            entries: body
                .entries
                .iter()
                .filter(|entry| {
                    !matches!(
                        &entry.kind,
                        BodyEntryKind::Property(prop) if prop.name.name == oneof.discriminator
                    )
                })
                .cloned()
                .collect(),
        };
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
                let FieldType::List(inner) = declared.as_ref() else {
                    diags.push(
                        Diagnostic::error(format!(
                            "type mismatch for '{}': expected {}, got array",
                            field.name, declared
                        ))
                        .with_span(m.name.span),
                    );
                    return;
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
                        ListItemKind::Reference(_) | ListItemKind::Named { .. } => {}
                    }
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
                        .with_span(span),
                    );
                }
            }
            FieldType::Modifier(declared) => {
                self.validate_value_against_type(value, declared, field_name, context, span, diags);
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
            FieldType::Union(variants) => variants
                .iter()
                .any(|variant| self.value_matches_type(value, variant)),
            FieldType::Modifier(declared) => self.value_matches_type(value, declared),
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
        if *prim == PrimitiveType::Role {
            if let Value::String(s) = value {
                let msg = if s.starts_with('@') {
                    format!("role field '{field_name}': use {s} instead of \"{s}\"")
                } else {
                    format!("role field '{field_name}': use @{s} instead of \"{s}\"")
                };
                diags.push(Diagnostic::warning(msg).with_span(span));
                return;
            }
        }
        let expected = if *prim == PrimitiveType::Secret {
            "environment variable ($ENV.VARIABLE_NAME)".to_string()
        } else {
            prim.as_str().to_string()
        };
        diags.push(
            Diagnostic::error(format!(
                "type mismatch {context} '{field_name}': expected {expected}, got {}",
                value_type_name(value)
            ))
            .with_span(span),
        );
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
                    diags.push(
                        Diagnostic::error(format!(
                            "invalid value '{s}' for '{field_name}': expected one of {}",
                            variants()
                        ))
                        .with_span(span),
                    );
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
            diags.push(Diagnostic::warning(format!(
                "circular membership detected: {desc}"
            )));
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

fn value_matches_primitive(value: &Value, prim: &PrimitiveType) -> bool {
    if matches!(value, Value::Reference(_) | Value::Secret(_)) {
        return true;
    }
    match prim {
        PrimitiveType::String => matches!(value, Value::String(_) | Value::TemplateString(_)),
        PrimitiveType::Number => matches!(value, Value::Number(_)),
        PrimitiveType::Bool => matches!(value, Value::Bool(_)),
        PrimitiveType::Money => matches!(value, Value::Money(_)),
        PrimitiveType::Duration => matches!(
            value,
            Value::String(_) | Value::TemplateString(_) | Value::Duration(_)
        ),
        PrimitiveType::Path => matches!(value, Value::String(_) | Value::TemplateString(_)),
        PrimitiveType::Secret => false,
        PrimitiveType::Object => false,
        PrimitiveType::Role => matches!(value, Value::Role(_)),
    }
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::TemplateString(_) => "string",
        Value::Number(_) => "number",
        Value::Money(_) => "money",
        Value::Bool(_) => "bool",
        Value::Duration(_) => "duration",
        Value::Path(_) => "path",
        Value::Secret(_) => "secret",
        Value::Role(_) => "role reference",
        Value::Reference(_) => "reference",
        Value::Array(_) => "array",
        Value::Fallback(_, _) => "fallback",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "model resource:\n    name string?\n    path path!\n",
            "[]resource resources:\n    - \"/api\"\n",
        );
        assert!(d.is_empty(), "unexpected diagnostics: {d:?}");
    }

    #[test]
    fn name_is_shorthand_named_and_scalar_both_fill_it() {
        // `name string!` — identity *is* the name, so both forms fill `name`:
        // the named key (`- editor:`) and the scalar (`- "viewer"`).
        let d = diags(
            "model role:\n    name string!\n    description string?\n",
            "[]role roles:\n    - editor:\n        description = \"x\"\n    - \"viewer\"\n",
        );
        assert!(d.is_empty(), "unexpected diagnostics: {d:?}");
    }

    #[test]
    fn validator_and_de_agree_on_scalar_shorthand() {
        // Agreement guardrail (RFC §11.10): the *same* instance validates clean AND
        // deserializes clean with matching fields — the de-path closed the transitional
        // "validates but de errors" gap.
        let schema = "model resource:\n    path string!\n    method string?\n\nmodel svc:\n    resources []resource\n";
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
        let svc: Svc =
            nml_core::from_body_defaulted(&index, "svc", &block.body, &nml_core::ValueResolver::env())
                .expect("should deserialize");
        assert_eq!(svc.resources[0].path, "/api");
        assert_eq!(svc.resources[0].method, None);
        assert_eq!(svc.resources[1].path, "/health");
        assert_eq!(svc.resources[1].method.as_deref(), Some("GET"));
    }

    #[test]
    fn scalar_shorthand_with_body_fills_field_and_validates() {
        // `- "/admin":` + body: the scalar fills `path!`, the body sets `method`.
        let d = diags(
            "enum httpMethod:\n    - \"GET\"\n    - \"POST\"\nmodel resource:\n    path path!\n    method httpMethod = \"GET\"\n",
            "[]resource resources:\n    - \"/admin\":\n        method = \"POST\"\n",
        );
        assert!(d.is_empty(), "scalar-with-body should validate: {d:?}");
    }

    #[test]
    fn scalar_shorthand_with_body_type_checks_the_body() {
        // The body is validated too: an unknown enum value is caught.
        let d = diags(
            "enum httpMethod:\n    - \"GET\"\n    - \"POST\"\nmodel resource:\n    path path!\n    method httpMethod = \"GET\"\n",
            "[]resource resources:\n    - \"/admin\":\n        method = \"BOGUS\"\n",
        );
        assert!(!d.is_empty(), "invalid method in the body should be flagged");
    }

    #[test]
    fn scalar_without_shorthand_field_is_dropped_key_without_noise() {
        // The dropped-key diagnostic is the *only* one — no spurious "missing
        // required field" piled on from validating an empty body.
        let d = diags(
            "model role:\n    name string\n    label string\n",
            "[]role roles:\n    - \"/api\"\n",
        );
        assert_eq!(d.len(), 1, "expected only the dropped-key diagnostic: {d:?}");
        assert!(d[0].message.contains("no shorthand field"), "{d:?}");
    }

    #[test]
    fn scalar_shorthand_on_union_list_is_flagged() {
        let schema = "model a:\n    x string?\nmodel b:\n    y string?\noneof u by kind:\n    \"a\" => a\n    \"b\" => b\n";
        let d = diags(schema, "[]u items:\n    - \"foo\"\n");
        assert!(d.iter().any(|x| x.message.contains("union-typed lists")), "{d:?}");
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
        assert!(d.is_empty(), "explicit name should win over block name: {d:?}");
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
        assert!(diags
            .iter()
            .any(|d| d.message.contains("unknown modifier '|forbid'")));
    }

    #[test]
    fn test_field_definition_outside_model() {
        let validator = make_validator("");
        let source = "service Svc:\n    name string\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags.iter().any(|d| d
            .message
            .contains("field definitions are only allowed in model declarations")));
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
        assert!(diags
            .iter()
            .any(|d| d.message.contains("unknown property 'unknown'")));
    }

    #[test]
    fn test_required_field_missing() {
        let schema = "model mount:\n    path string\n    wasm string?\n";
        let validator = make_validator(schema);

        let source = "mount Test:\n    wasm = \"handler.wasm\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags
            .iter()
            .any(|d| d.message.contains("missing required field 'path'")));
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
        assert!(diags
            .iter()
            .any(|d| d.message.contains("type mismatch") && d.message.contains("expected string")));
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
        assert!(diags
            .iter()
            .any(|d| d.message.contains("invalid value 'gemini'")));
    }

    #[test]
    fn test_array_declaration_modifier_validation() {
        let validator = make_validator_with_modifiers("", &["allow", "deny"]);
        let source =
            "[]mount mounts:\n    |restrict = [@admin]\n    - Test:\n        path = \"/\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags
            .iter()
            .any(|d| d.message.contains("unknown modifier '|restrict'")));
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
                .any(|d| d.message.contains("use @public instead of \"@public\"")),
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
                .any(|d| d.message.contains("use @admin instead of \"admin\"")),
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
                .any(|d| d.message.contains("use @admin instead of \"@admin\"")),
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
                .any(|d| d.message.contains("unknown parent model 'nonexistent'")),
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
            .filter(|d| d.message.contains("unknown parent"))
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
                .any(|d| d.message.contains("unknown parent model 'foo'")),
            "should detect 'foo' as unknown; diags: {:?}",
            diags
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown parent model 'bar'")),
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
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("circular membership")),
            "should detect circular membership; diags: {:?}",
            diags
        );
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
    fn test_strict_unmodeled_array_keyword_with_scalar_items() {
        // A scalar inline item is as malformed as a named one under an undefined
        // keyword — the strict "no definition" check covers both.
        let validator = make_strict_validator("model server:\n    port number?\n");
        let file = nml_core::cst::parse_to_ast("[]bogus items:\n    - \"/api\"\n").unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("array item keyword 'bogus' has no model or oneof definition")),
            "strict mode should reject unmodeled array with scalar items; diags: {diags:?}"
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
        let mut body = Body { entries: vec![] };
        for _ in 0..(MAX_VALIDATION_DEPTH + 4) {
            body = Body {
                entries: vec![BodyEntry {
                    kind: BodyEntryKind::NestedBlock(NestedBlock {
                        name: Identifier::new("child", span),
                        body,
                    }),
                    span,
                }],
            };
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
        "oneof email by provider:\n    \"log\" => emailLog\n    \"postmark\" => emailPostmark\n\n",
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
        assert!(errs.is_empty(), "valid postmark variant should pass: {errs:?}");
    }

    #[test]
    fn test_oneof_rejects_cross_variant_field() {
        // serverToken belongs to the postmark variant, not log.
        let errs = oneof_errors("email Cfg:\n    provider = \"log\"\n    serverToken = $ENV.TOK\n");
        assert!(
            errs.iter().any(|m| m.contains("unknown property 'serverToken'")),
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
            bad.iter().any(|m| m.contains("unknown property 'serverToken'")),
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
            bad.iter().any(|m| m.contains("unknown property 'serverToken'")),
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
        let schema = "model emailLog:\n    level string?\n\nmodel emailPostmark:\n    serverToken string\n\noneof email by provider = \"log\":\n    \"log\" => emailLog\n    \"postmark\" => emailPostmark\n";
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
        let schema = "model emailLog:\n    level string?\n\noneof email by provider:\n    \"log\" => emailLog\n";
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
        // duration accepts a string literal; an `$ENV` secret default is lenient;
        // a numeric default matches a number field — all reuse the value check.
        let src = "model cfg:\n    sessionDuration duration = \"24h\"\n    apiKey secret = $ENV.KEY\n    retries number = 3\n";
        let file = nml_core::cst::parse_to_ast(src).unwrap();
        let schema = nml_core::cst::extract_schema(src).0;
        let validator = SchemaValidator::new(schema.models, schema.enums, schema.oneofs);
        let errors: Vec<_> = validator
            .validate(&file)
            .into_iter()
            .filter(|d| matches!(d.severity, Severity::Error))
            .collect();
        assert!(errors.is_empty(), "valid typed defaults should pass: {errors:?}");
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
        assert_eq!(count, 1, "inherited bad default must be reported exactly once");
    }
}
