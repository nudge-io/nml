use crate::ast::*;
use crate::model::{EnumDef, FieldDef, FieldType, ModelDef, TraitDef};
use crate::types::PrimitiveType;

/// Results of extracting schema definitions from a parsed NML file.
#[derive(Debug, Default)]
pub struct ExtractedSchema {
    pub models: Vec<ModelDef>,
    pub enums: Vec<EnumDef>,
    pub traits: Vec<TraitDef>,
}

/// Extract model, enum, and trait definitions from a parsed AST.
pub fn extract(file: &File) -> ExtractedSchema {
    let mut schema = ExtractedSchema::default();

    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => match block.keyword.name.as_str() {
                "model" => {
                    if let Some(model) = extract_model(block, decl.span) {
                        schema.models.push(model);
                    }
                }
                "trait" => {
                    if let Some(trait_def) = extract_trait(block, decl.span) {
                        schema.traits.push(trait_def);
                    }
                }
                "enum" => {
                    if let Some(enum_def) = extract_enum(block, decl.span) {
                        schema.enums.push(enum_def);
                    }
                }
                _ => {}
            },
            DeclarationKind::Array(_) => {}
            DeclarationKind::Const(_) | DeclarationKind::Template(_) => {}
        }
    }

    schema
}

fn extract_model(block: &BlockDecl, span: crate::span::Span) -> Option<ModelDef> {
    let mut fields = Vec::new();

    for entry in &block.body.entries {
        match &entry.kind {
            BodyEntryKind::FieldDefinition(fd) => {
                fields.push(convert_field_def(fd, entry.span));
            }
            BodyEntryKind::Modifier(m) => {
                if let ModifierValue::TypeAnnotation {
                    field_type,
                    optional,
                } = &m.value
                {
                    fields.push(FieldDef {
                        name: m.name.name.clone(),
                        field_type: FieldType::Modifier(resolve_type_name(field_type)),
                        optional: *optional,
                        default_value: None,
                        constraints: Vec::new(),
                        span: entry.span,
                    });
                }
            }
            _ => {}
        }
    }

    Some(ModelDef {
        name: block.name.name.clone(),
        traits: Vec::new(),
        fields,
        span,
    })
}

fn extract_trait(block: &BlockDecl, span: crate::span::Span) -> Option<TraitDef> {
    let mut fields = Vec::new();

    for entry in &block.body.entries {
        if let BodyEntryKind::FieldDefinition(fd) = &entry.kind {
            fields.push(convert_field_def(fd, entry.span));
        }
    }

    Some(TraitDef {
        name: block.name.name.clone(),
        fields,
        span,
    })
}

fn extract_enum(block: &BlockDecl, span: crate::span::Span) -> Option<EnumDef> {
    let mut variants = Vec::new();

    for entry in &block.body.entries {
        if let BodyEntryKind::ListItem(item) = &entry.kind {
            match &item.kind {
                ListItemKind::Shorthand(val) => {
                    if let crate::types::Value::String(s) = &val.value {
                        variants.push(s.clone());
                    }
                }
                ListItemKind::Reference(ident) => {
                    variants.push(ident.name.clone());
                }
                _ => {}
            }
        }
    }

    Some(EnumDef {
        name: block.name.name.clone(),
        variants,
        span,
    })
}

fn convert_field_def(fd: &FieldDefinition, span: crate::span::Span) -> FieldDef {
    let field_type = resolve_field_type(&fd.field_type);
    let default_value = fd.default_value.as_ref().map(|v| format_default(&v.value));

    FieldDef {
        name: fd.name.name.clone(),
        field_type,
        optional: fd.optional,
        default_value,
        constraints: Vec::new(),
        span,
    }
}

fn resolve_field_type(expr: &FieldTypeExpr) -> FieldType {
    match expr {
        FieldTypeExpr::Named(id) => {
            if let Some(prim) = PrimitiveType::from_str(&id.name) {
                FieldType::Primitive(prim)
            } else {
                FieldType::ModelRef(id.name.clone())
            }
        }
        FieldTypeExpr::Array(id) => {
            let inner = if let Some(prim) = PrimitiveType::from_str(&id.name) {
                FieldType::Primitive(prim)
            } else {
                FieldType::ModelRef(id.name.clone())
            };
            FieldType::List(Box::new(inner))
        }
    }
}

fn resolve_type_name(expr: &FieldTypeExpr) -> String {
    match expr {
        FieldTypeExpr::Named(id) => id.name.clone(),
        FieldTypeExpr::Array(id) => format!("[]{}", id.name),
    }
}

fn format_default(value: &crate::types::Value) -> String {
    match value {
        crate::types::Value::String(s) => s.clone(),
        crate::types::Value::Number(n) => format!("{n}"),
        crate::types::Value::Bool(b) => format!("{b}"),
        crate::types::Value::Reference(r) => r.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn test_extract_model() {
        let source = "model provider:\n    type providerType\n    model string\n    temperature number?\n    baseUrl string?\n";
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

        assert_eq!(schema.models.len(), 1);
        let model = &schema.models[0];
        assert_eq!(model.name, "provider");
        assert_eq!(model.fields.len(), 4);

        assert_eq!(model.fields[0].name, "type");
        assert!(matches!(model.fields[0].field_type, FieldType::ModelRef(ref s) if s == "providerType"));
        assert!(!model.fields[0].optional);

        assert_eq!(model.fields[1].name, "model");
        assert!(matches!(model.fields[1].field_type, FieldType::Primitive(PrimitiveType::String)));

        assert_eq!(model.fields[2].name, "temperature");
        assert!(model.fields[2].optional);

        assert_eq!(model.fields[3].name, "baseUrl");
        assert!(model.fields[3].optional);
    }

    #[test]
    fn test_extract_model_with_default() {
        let source = "model prompt:\n    outputFormat string = \"text\"\n";
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

        assert_eq!(schema.models.len(), 1);
        let field = &schema.models[0].fields[0];
        assert_eq!(field.name, "outputFormat");
        assert_eq!(field.default_value, Some("text".to_string()));
    }

    #[test]
    fn test_extract_model_with_array_field() {
        let source = "model workflow:\n    steps []step\n    extensions []extensionPoint?\n";
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

        let model = &schema.models[0];
        assert_eq!(model.fields.len(), 2);

        assert!(matches!(model.fields[0].field_type, FieldType::List(_)));
        assert!(!model.fields[0].optional);

        assert!(matches!(model.fields[1].field_type, FieldType::List(_)));
        assert!(model.fields[1].optional);
    }

    #[test]
    fn test_extract_model_with_modifier_fields() {
        let source = "model plugin:\n    wasm string\n    |allow []string?\n    |deny []string?\n";
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

        let model = &schema.models[0];
        assert_eq!(model.fields.len(), 3);
        assert_eq!(model.fields[0].name, "wasm");
        assert!(matches!(model.fields[1].field_type, FieldType::Modifier(_)));
        assert!(model.fields[1].optional);
    }

    #[test]
    fn test_extract_model_with_object_field() {
        use crate::model::FieldType;
        use crate::types::PrimitiveType;

        let source = "model plugin:\n    wasm string\n    config object?\n";
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

        let model = &schema.models[0];
        assert_eq!(model.fields.len(), 2);
        assert_eq!(model.fields[1].name, "config");
        assert!(matches!(
            &model.fields[1].field_type,
            FieldType::Primitive(PrimitiveType::Object)
        ));
        assert!(model.fields[1].optional);
    }

    #[test]
    fn test_extract_enum() {
        let source = "enum providerType:\n    - \"anthropic\"\n    - \"openai\"\n    - \"groq\"\n    - \"ollama\"\n";
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

        assert_eq!(schema.enums.len(), 1);
        let e = &schema.enums[0];
        assert_eq!(e.name, "providerType");
        assert_eq!(e.variants, vec!["anthropic", "openai", "groq", "ollama"]);
    }

    #[test]
    fn test_extract_trait() {
        let source = "trait auditable:\n    createdAt string\n    updatedAt string?\n";
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

        assert_eq!(schema.traits.len(), 1);
        let t = &schema.traits[0];
        assert_eq!(t.name, "auditable");
        assert_eq!(t.fields.len(), 2);
    }

    #[test]
    fn test_extract_mixed() {
        let source = "\
enum status:\n    - \"active\"\n    - \"inactive\"\n\n\
model user:\n    name string\n    status status\n\n\
trait timestamped:\n    createdAt string\n";
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

        assert_eq!(schema.enums.len(), 1);
        assert_eq!(schema.models.len(), 1);
        assert_eq!(schema.traits.len(), 1);
    }
}
