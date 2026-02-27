use nml_core::ast::*;
use nml_core::model::{EnumDef, FieldDef, FieldType, ModelDef};
use nml_core::types::{PrimitiveType, Value};

use crate::diagnostics::Diagnostic;

const VALID_MODIFIERS: &[&str] = &["allow", "deny"];

/// Validates instance declarations against model definitions.
pub struct SchemaValidator {
    models: Vec<ModelDef>,
    enums: Vec<EnumDef>,
}

impl SchemaValidator {
    pub fn new(models: Vec<ModelDef>, enums: Vec<EnumDef>) -> Self {
        Self { models, enums }
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

        diagnostics
    }

    fn validate_block(&self, block: &BlockDecl, diags: &mut Vec<Diagnostic>) {
        let keyword = &block.keyword.name;
        let is_schema_def = matches!(keyword.as_str(), "model" | "trait" | "enum");

        self.validate_body(&block.body, is_schema_def, keyword, diags);

        if !is_schema_def {
            if let Some(model) = self.find_model(keyword) {
                self.validate_instance_against_model(&block.body, model, diags);
            }
        }
    }

    fn validate_array(&self, arr: &ArrayDecl, diags: &mut Vec<Diagnostic>) {
        for modifier in &arr.body.modifiers {
            self.validate_modifier_name(modifier, diags);
        }

        for item in &arr.body.items {
            if let ListItemKind::Named { body, .. } = &item.kind {
                let keyword = &arr.item_keyword.name;
                let is_schema_def = matches!(keyword.as_str(), "model" | "trait" | "enum");

                self.validate_body(body, is_schema_def, keyword, diags);

                if !is_schema_def {
                    if let Some(model) = self.find_model(keyword) {
                        self.validate_instance_against_model(body, model, diags);
                    }
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
                }
                BodyEntryKind::FieldDefinition(_) if !is_schema_def => {
                    diags.push(
                        Diagnostic::error(format!(
                            "field definitions are only allowed in model or trait declarations, not '{keyword}'"
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
        if !VALID_MODIFIERS.contains(&m.name.name.as_str()) {
            diags.push(
                Diagnostic::warning(format!(
                    "unknown modifier '|{}'; expected one of: {}",
                    m.name.name,
                    VALID_MODIFIERS
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
        diags: &mut Vec<Diagnostic>,
    ) {
        let mut seen_fields: Vec<&str> = Vec::new();

        for entry in &body.entries {
            match &entry.kind {
                BodyEntryKind::Property(prop) => {
                    let name = &prop.name.name;
                    seen_fields.push(name);

                    if let Some(field_def) = model.fields.iter().find(|f| f.name == *name) {
                        self.validate_value_type(&prop.value.value, field_def, prop.value.span, diags);
                    } else {
                        diags.push(
                            Diagnostic::warning(format!(
                                "unknown property '{name}' (not defined in model '{}')",
                                model.name
                            ))
                            .with_span(prop.name.span),
                        );
                    }
                }
                BodyEntryKind::NestedBlock(nb) => {
                    seen_fields.push(&nb.name.name);

                    if model.fields.iter().all(|f| f.name != nb.name.name) {
                        diags.push(
                            Diagnostic::warning(format!(
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
        match &field.field_type {
            FieldType::Primitive(prim) => {
                if !value_matches_primitive(value, prim) {
                    diags.push(
                        Diagnostic::error(format!(
                            "type mismatch for '{}': expected {}, got {}",
                            field.name,
                            prim.as_str(),
                            value_type_name(value)
                        ))
                        .with_span(span),
                    );
                }
            }
            FieldType::ModelRef(ref_name) => {
                if let Some(enum_def) = self.find_enum(ref_name) {
                    self.validate_enum_value(value, enum_def, &field.name, span, diags);
                }
            }
            FieldType::List(inner) => {
                if let Value::Array(items) = value {
                    for item in items {
                        self.validate_value_against_type(&item.value, inner, &field.name, item.span, diags);
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
                if !value_matches_primitive(value, prim) {
                    diags.push(
                        Diagnostic::error(format!(
                            "type mismatch in array '{}': expected {}, got {}",
                            field_name,
                            prim.as_str(),
                            value_type_name(value)
                        ))
                        .with_span(span),
                    );
                }
            }
            FieldType::ModelRef(ref_name) => {
                if let Some(enum_def) = self.find_enum(ref_name) {
                    self.validate_enum_value(value, enum_def, field_name, span, diags);
                }
            }
            _ => {}
        }
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
}

fn value_matches_primitive(value: &Value, prim: &PrimitiveType) -> bool {
    if matches!(value, Value::Reference(_)) {
        return true;
    }
    match prim {
        PrimitiveType::String => matches!(value, Value::String(_)),
        PrimitiveType::Number => matches!(value, Value::Number(_)),
        PrimitiveType::Bool => matches!(value, Value::Bool(_)),
        PrimitiveType::Money => matches!(value, Value::Money(_)),
        PrimitiveType::Duration => matches!(value, Value::String(_) | Value::Duration(_)),
        PrimitiveType::Path => matches!(value, Value::String(_)),
        PrimitiveType::Secret => matches!(value, Value::Secret(_)),
    }
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Money(_) => "money",
        Value::Bool(_) => "bool",
        Value::Duration(_) => "duration",
        Value::Path(_) => "path",
        Value::Secret(_) => "secret",
        Value::RoleRef(_) => "role reference",
        Value::Reference(_) => "reference",
        Value::Array(_) => "array",
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

    #[test]
    fn test_valid_modifiers() {
        let validator = make_validator("");
        let source = "service Svc:\n    |allow = [@public]\n    |deny = []\n    localMount = \"/\"\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        let modifier_diags: Vec<_> = diags.iter().filter(|d| d.message.contains("modifier")).collect();
        assert!(modifier_diags.is_empty());
    }

    #[test]
    fn test_invalid_modifier_name() {
        let validator = make_validator("");
        let source = "service Svc:\n    |forbid = [@public]\n    localMount = \"/\"\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags.iter().any(|d| d.message.contains("unknown modifier '|forbid'")));
    }

    #[test]
    fn test_field_definition_outside_model() {
        let validator = make_validator("");
        let source = "service Svc:\n    name string\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags.iter().any(|d| d.message.contains("field definitions are only allowed in model")));
    }

    #[test]
    fn test_field_definition_in_model_ok() {
        let validator = make_validator("");
        let source = "model provider:\n    name string\n    url string?\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        let field_diags: Vec<_> = diags.iter().filter(|d| d.message.contains("field definitions")).collect();
        assert!(field_diags.is_empty());
    }

    #[test]
    fn test_unknown_property() {
        let schema = "model mount:\n    path string\n    wasm string?\n";
        let validator = make_validator(schema);

        let source = "mount Test:\n    path = \"/\"\n    unknown = \"value\"\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags.iter().any(|d| d.message.contains("unknown property 'unknown'")));
    }

    #[test]
    fn test_required_field_missing() {
        let schema = "model mount:\n    path string\n    wasm string?\n";
        let validator = make_validator(schema);

        let source = "mount Test:\n    wasm = \"handler.wasm\"\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags.iter().any(|d| d.message.contains("missing required field 'path'")));
    }

    #[test]
    fn test_required_field_with_default_ok() {
        let schema = "model prompt:\n    outputFormat string = \"text\"\n";
        let validator = make_validator(schema);

        let source = "prompt MyPrompt:\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        let required_diags: Vec<_> = diags.iter().filter(|d| d.message.contains("missing required")).collect();
        assert!(required_diags.is_empty());
    }

    #[test]
    fn test_type_mismatch() {
        let schema = "model mount:\n    path string\n";
        let validator = make_validator(schema);

        let source = "mount Test:\n    path = 42\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags.iter().any(|d| d.message.contains("type mismatch") && d.message.contains("expected string")));
    }

    #[test]
    fn test_type_match_ok() {
        let schema = "model mount:\n    path string\n    port number?\n";
        let validator = make_validator(schema);

        let source = "mount Test:\n    path = \"/api\"\n    port = 8080\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        let type_diags: Vec<_> = diags.iter().filter(|d| d.message.contains("type mismatch")).collect();
        assert!(type_diags.is_empty());
    }

    #[test]
    fn test_enum_validation_valid() {
        let schema = "enum providerType:\n    - \"openai\"\n    - \"groq\"\n\nmodel provider:\n    type providerType\n";
        let validator = make_validator(schema);

        let source = "provider Groq:\n    type = \"groq\"\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        let enum_diags: Vec<_> = diags.iter().filter(|d| d.message.contains("invalid value")).collect();
        assert!(enum_diags.is_empty());
    }

    #[test]
    fn test_enum_validation_invalid() {
        let schema = "enum providerType:\n    - \"openai\"\n    - \"groq\"\n\nmodel provider:\n    type providerType\n";
        let validator = make_validator(schema);

        let source = "provider Groq:\n    type = \"gemini\"\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags.iter().any(|d| d.message.contains("invalid value 'gemini'")));
    }

    #[test]
    fn test_array_declaration_modifier_validation() {
        let validator = make_validator("");
        let source = "[]mount mounts:\n    |restrict = [@admin]\n    - Test:\n        path = \"/\"\n";
        let file = parser::parse(source).unwrap();
        let diags = validator.validate(&file);
        assert!(diags.iter().any(|d| d.message.contains("unknown modifier '|restrict'")));
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
        let type_diags: Vec<_> = diags.iter().filter(|d| d.message.contains("type mismatch")).collect();
        assert!(type_diags.is_empty());
    }
}
