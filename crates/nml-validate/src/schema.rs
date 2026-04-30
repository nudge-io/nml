use std::collections::{HashMap, HashSet};

use nml_core::ast::*;
use nml_core::model::{EnumDef, FieldDef, FieldType, ModelDef};
use nml_core::types::{PrimitiveType, Value};

use crate::diagnostics::Diagnostic;
#[cfg(test)]
use crate::diagnostics::Severity;

const MAX_VALIDATION_DEPTH: u32 = 64;

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
    models: Vec<ModelDef>,
    enums: Vec<EnumDef>,
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

impl SchemaValidator {
    pub fn new(models: Vec<ModelDef>, enums: Vec<EnumDef>) -> Self {
        Self {
            models,
            enums,
            valid_modifiers: Vec::new(),
            strict_unknown_fields: false,
            membership: MembershipSemantics::default(),
        }
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
        self.models.iter().find(|m| m.name == name)
    }

    pub fn find_enum(&self, name: &str) -> Option<&EnumDef> {
        self.enums.iter().find(|e| e.name == name)
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
                DeclarationKind::Const(_) | DeclarationKind::Template(_) => {}
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
        }

        self.validate_body(&block.body, is_schema_def, keyword, diags);
        self.validate_members_builtin_refs(&block.body, keyword, diags);

        if !is_schema_def {
            if let Some(model) = self.find_model(keyword) {
                self.validate_instance_against_model(&block.body, model, 0, diags);
            } else if self.strict_unknown_fields {
                diags.push(
                    Diagnostic::error(format!(
                        "block keyword '{keyword}' has no model definition"
                    ))
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
        let model = if !is_schema_def {
            self.find_model(keyword)
        } else {
            None
        };

        let has_named_items = arr
            .body
            .items
            .iter()
            .any(|i| matches!(&i.kind, ListItemKind::Named { .. }));

        if !is_schema_def && model.is_none() && has_named_items && self.strict_unknown_fields {
            diags.push(
                Diagnostic::error(format!(
                    "array item keyword '{keyword}' has no model definition"
                ))
                .with_span(arr.item_keyword.span),
            );
        }

        for item in &arr.body.items {
            if let ListItemKind::Named { body, .. } = &item.kind {
                self.validate_body(body, is_schema_def, keyword, diags);
                self.validate_members_builtin_refs(body, keyword, diags);

                if let Some(model) = model {
                    self.validate_instance_against_model(body, model, 0, diags);
                }
            }
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

    fn validate_instance_against_model(
        &self,
        body: &Body,
        model: &ModelDef,
        depth: u32,
        diags: &mut Vec<Diagnostic>,
    ) {
        if depth >= MAX_VALIDATION_DEPTH {
            return;
        }

        let mut seen_fields: Vec<&str> = Vec::new();

        for entry in &body.entries {
            match &entry.kind {
                BodyEntryKind::Property(prop) => {
                    let name = &prop.name.name;
                    seen_fields.push(name);

                    if let Some(field_def) = model.fields.iter().find(|f| f.name == *name) {
                        self.validate_value_type(
                            &prop.value.value,
                            field_def,
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
                                if let Some(nested_model) = self.find_model(ref_name) {
                                    self.validate_instance_against_model(
                                        &nb.body,
                                        nested_model,
                                        depth + 1,
                                        diags,
                                    );
                                }
                            }
                            FieldType::List(inner) => {
                                for entry in &nb.body.entries {
                                    if let BodyEntryKind::ListItem(item) = &entry.kind {
                                        if let ListItemKind::Named { body, .. } = &item.kind {
                                            match inner.as_ref() {
                                                FieldType::ModelRef(ref_name) => {
                                                    if let Some(inner_model) =
                                                        self.find_model(ref_name)
                                                    {
                                                        self.validate_instance_against_model(
                                                            body,
                                                            inner_model,
                                                            depth + 1,
                                                            diags,
                                                        );
                                                    }
                                                }
                                                FieldType::Union(variants) => {
                                                    let has_list_items =
                                                        body.entries.iter().any(|e| {
                                                            matches!(
                                                                &e.kind,
                                                                BodyEntryKind::ListItem(_)
                                                            )
                                                        });
                                                    for variant in variants {
                                                        match variant {
                                                            FieldType::ModelRef(ref_name)
                                                                if !has_list_items =>
                                                            {
                                                                if let Some(m) =
                                                                    self.find_model(ref_name)
                                                                {
                                                                    self.validate_instance_against_model(body, m, depth + 1, diags);
                                                                }
                                                                break;
                                                            }
                                                            FieldType::List(list_inner)
                                                                if has_list_items =>
                                                            {
                                                                if let FieldType::ModelRef(
                                                                    ref_name,
                                                                ) = list_inner.as_ref()
                                                                {
                                                                    if let Some(m) =
                                                                        self.find_model(ref_name)
                                                                    {
                                                                        for sub_entry in
                                                                            &body.entries
                                                                        {
                                                                            if let BodyEntryKind::ListItem(sub_item) = &sub_entry.kind {
                                                                                if let ListItemKind::Named { body: sub_body, .. } = &sub_item.kind {
                                                                                    self.validate_instance_against_model(sub_body, m, depth + 1, diags);
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                break;
                                                            }
                                                            _ => {}
                                                        }
                                                    }
                                                }
                                                _ => {}
                                            }
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
                }
                _ => {}
            }
        }

        for field in &model.fields {
            if !field.optional && field.default_value.is_none() {
                if !seen_fields.contains(&field.name.as_str()) {
                    diags.push(
                        Diagnostic::error(format!(
                            "missing required field '{}' (defined in model '{}')",
                            field.name, model.name
                        ))
                        .with_span(body.entries.first().map(|e| e.span).unwrap_or(field.span)),
                    );
                }
            }
        }
    }

    fn validate_value_type(
        &self,
        value: &Value,
        field: &FieldDef,
        span: nml_core::span::Span,
        diags: &mut Vec<Diagnostic>,
    ) {
        if let Value::Fallback(primary, fallback) = value {
            self.validate_value_type(&primary.value, field, primary.span, diags);
            self.validate_value_type(&fallback.value, field, fallback.span, diags);
            return;
        }

        match &field.field_type {
            FieldType::Primitive(prim) => {
                self.validate_primitive_value(value, prim, &field.name, "for", span, diags);
            }
            FieldType::ModelRef(ref_name) => {
                if let Some(enum_def) = self.find_enum(ref_name) {
                    self.validate_enum_value(value, enum_def, &field.name, span, diags);
                } else {
                    self.validate_model_ref_value(value, ref_name, &field.name, span, diags);
                }
            }
            FieldType::List(inner) => {
                if let Value::Array(items) = value {
                    for item in items {
                        self.validate_value_against_type(
                            &item.value,
                            inner,
                            &field.name,
                            item.span,
                            diags,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn validate_value_against_type(
        &self,
        value: &Value,
        field_type: &FieldType,
        field_name: &str,
        span: nml_core::span::Span,
        diags: &mut Vec<Diagnostic>,
    ) {
        match field_type {
            FieldType::Primitive(prim) => {
                self.validate_primitive_value(value, prim, field_name, "in array", span, diags);
            }
            FieldType::ModelRef(ref_name) => {
                if let Some(enum_def) = self.find_enum(ref_name) {
                    self.validate_enum_value(value, enum_def, field_name, span, diags);
                } else {
                    self.validate_model_ref_value(value, ref_name, field_name, span, diags);
                }
            }
            _ => {}
        }
    }

    fn validate_primitive_value(
        &self,
        value: &Value,
        prim: &PrimitiveType,
        field_name: &str,
        context: &str,
        span: nml_core::span::Span,
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
        span: nml_core::span::Span,
        diags: &mut Vec<Diagnostic>,
    ) {
        let val_str = match value {
            Value::String(s) => Some(s.as_str()),
            Value::Reference(r) => Some(r.as_str()),
            _ => None,
        };

        if let Some(val) = val_str {
            if !enum_def.variants.iter().any(|v| v == val) {
                diags.push(
                    Diagnostic::error(format!(
                        "invalid value '{val}' for '{field_name}': expected one of {}",
                        enum_def
                            .variants
                            .iter()
                            .map(|v| format!("\"{v}\""))
                            .collect::<Vec<_>>()
                            .join(", ")
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
        span: nml_core::span::Span,
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
        span: nml_core::span::Span,
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
                    if self.membership.member_keywords.iter().any(|k| k == &block.keyword.name) {
                        let refs = collect_member_refs(&block.body, &prefixes);
                        membership.insert(block.name.name.clone(), refs);
                    }
                }
                DeclarationKind::Array(arr) => {
                    if self.membership.member_keywords.iter().any(|k| k == &arr.item_keyword.name) {
                        for item in &arr.body.items {
                            if let ListItemKind::Named { name, body } = &item.kind {
                                let refs = collect_member_refs(body, &prefixes);
                                membership.insert(name.name.clone(), refs);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        let mut globally_visited = HashSet::new();
        let keys: Vec<String> = membership.keys().cloned().collect();
        for name in &keys {
            if globally_visited.contains(name.as_str()) {
                continue;
            }
            let mut path = Vec::new();
            detect_member_cycle(name, &membership, &mut path, &mut globally_visited, diags);
        }
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

fn detect_member_cycle(
    name: &str,
    membership: &HashMap<String, Vec<String>>,
    path: &mut Vec<String>,
    globally_visited: &mut HashSet<String>,
    diags: &mut Vec<Diagnostic>,
) {
    if let Some(pos) = path.iter().position(|n| n == name) {
        let cycle: Vec<&str> = path[pos..].iter().map(|s| s.as_str()).collect();
        let mut cycle_desc: Vec<&str> = cycle.to_vec();
        cycle_desc.push(cycle[0]);
        diags.push(Diagnostic::warning(format!(
            "circular membership detected: {}",
            cycle_desc.join(" -> ")
        )));
        return;
    }

    if globally_visited.contains(name) {
        return;
    }

    path.push(name.to_string());
    if let Some(members) = membership.get(name) {
        for member in members {
            detect_member_cycle(member, membership, path, globally_visited, diags);
        }
    }
    path.pop();
    globally_visited.insert(name.to_string());
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
    use nml_core::model_extract;
    use nml_core::parser;

    fn make_validator(schema_source: &str) -> SchemaValidator {
        let file = parser::parse(schema_source).unwrap();
        let schema = model_extract::extract(&file);
        SchemaValidator::new(schema.models, schema.enums)
    }

    fn make_validator_with_modifiers(schema_source: &str, modifiers: &[&str]) -> SchemaValidator {
        let file = parser::parse(schema_source).unwrap();
        let schema = model_extract::extract(&file);
        SchemaValidator::new(schema.models, schema.enums)
            .with_modifiers(modifiers.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn test_empty_modifiers_accepts_all() {
        let validator = make_validator("");
        let source = "service Svc:\n    |anything = [@public]\n    localMount = \"/\"\n";
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags
            .iter()
            .any(|d| d.message.contains("unknown modifier '|forbid'")));
    }

    #[test]
    fn test_field_definition_outside_model() {
        let validator = make_validator("");
        let source = "service Svc:\n    name string\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags.iter().any(|d| d
            .message
            .contains("field definitions are only allowed in model declarations")));
    }

    #[test]
    fn test_field_definition_in_model_ok() {
        let validator = make_validator("");
        let source = "model provider:\n    name string\n    url string?\n";
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_secret_type() {
        let schema = "model provider:\n    apiKey secret?\n";
        let validator = make_validator(schema);

        let source = "provider P:\n    apiKey = $ENV.MY_KEY\n";
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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

        let parse_result = parser::parse(schema);
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(schema_source).unwrap();
        let schema = model_extract::extract(&file);
        SchemaValidator::new(schema.models, schema.enums)
            .with_membership_semantics(nudge_membership())
    }

    #[test]
    fn test_no_membership_semantics_accepts_all() {
        let validator = make_validator("");

        let source = "service Svc:\n    |allow = [@user/john]\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("@user/")),
            "without membership semantics, @user/ should not be warned: {:?}",
            diags
        );
    }

    #[test]
    fn test_user_ref_in_allow_inline_warning() {
        let validator = make_validator_with_membership("");

        let source = "service Svc:\n    |allow = [@user/john]\n";
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(schema_source).unwrap();
        let schema = model_extract::extract(&file);
        SchemaValidator::new(schema.models, schema.enums).strict()
    }

    #[test]
    fn test_strict_unknown_property_is_error() {
        let schema = "model server:\n    port number?\n";
        let validator = make_strict_validator(schema);

        let source = "server S:\n    port = 3000\n    bogus = true\n";
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("block keyword 'bogusBlock' has no model definition")),
            "strict mode should reject unmodeled block keyword; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_default_unmodeled_block_keyword_silent() {
        let schema = "model server:\n    port number?\n";
        let validator = make_validator(schema);

        let source = "bogusBlock Thing:\n    key = \"value\"\n";
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.iter().any(|d| d
                .message
                .contains("array item keyword 'bogus' has no model definition")),
            "strict mode should reject unmodeled array with named items; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_strict_shorthand_array_no_false_positive() {
        let schema = "model server:\n    port number?\n";
        let validator = make_strict_validator(schema);

        let source = "[]plugin plugins:\n    - \"echo\"\n    - \"telnyx\"\n";
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(
            diags.is_empty(),
            "object? fields should accept arbitrary keys even in strict mode; diags: {:?}",
            diags
        );
    }

    #[test]
    fn test_strict_nested_unknown_property_is_error() {
        let schema =
            "model prompt:\n    system string?\n\nmodel step:\n    prompt prompt?\n";
        let validator = make_strict_validator(schema);

        let source = "step S:\n    prompt:\n        systm = \"typo\"\n";
        let file = parser::parse(source).unwrap();
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
}
