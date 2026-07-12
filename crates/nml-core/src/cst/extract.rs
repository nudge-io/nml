//! Schema extraction over the CST (RFC 0004 P4).
//!
//! Produces an [`ExtractedSchema`] — model / enum / oneof definitions for
//! `SchemaIndex` — by walking the typed-wrapper layer ([`ast`](crate::cst::ast)).
//! Only the *reading* lives here; the downstream validation/inheritance passes
//! (`find_*_errors`, `resolve_model_inheritance`) operate on `ExtractedSchema` and
//! are reused unchanged.

use crate::cst::ast::{AstNode, BlockDecl, Decl, Entry, OneOfDecl, Root, TypeExpr, TypeExprKind};
use crate::cst::syntax::node_span;
use crate::cst::value::decode_string_token;
use crate::model::{EnumDef, FieldDef, FieldType, ModelDef, OneOfDef};
use crate::schema::ExtractedSchema;
use crate::types::{PrimitiveType, Value};

/// Extract model / enum / oneof definitions from a parsed CST.
pub fn extract(root: &Root) -> ExtractedSchema {
    let mut schema = ExtractedSchema::default();
    for decl in root.decls() {
        match decl {
            Decl::Block(block) => match block.keyword().as_ref().map(|t| t.text()) {
                Some("model") => schema.models.push(extract_model(&block)),
                Some("enum") => schema.enums.push(extract_enum(&block)),
                _ => {}
            },
            Decl::OneOf(oneof) => schema.oneofs.push(extract_oneof(&oneof)),
            Decl::Array(_) | Decl::Const(_) | Decl::Template(_) => {}
        }
    }
    schema
}

fn extract_model(block: &BlockDecl) -> ModelDef {
    let mut fields = Vec::new();
    if let Some(body) = block.body() {
        for entry in body.entries() {
            match entry {
                Entry::FieldDef(fd) => fields.push(FieldDef {
                    name: token_text(fd.name()),
                    field_type: fd
                        .type_expr()
                        .map(|t| resolve_field_type(&t))
                        .unwrap_or_else(unknown_type),
                    optional: fd.optional(),
                    shorthand: fd.shorthand(),
                    default_value: fd.default().and_then(|v| v.decode().ok()),
                    span: node_span(fd.syntax()),
                }),
                // A typed modifier (`|name type?`) declares a field too. Modifiers
                // are never the scalar-shorthand field.
                Entry::Modifier(m) => {
                    if let Some(te) = m.type_expr() {
                        fields.push(FieldDef {
                            name: token_text(m.name()),
                            field_type: FieldType::Modifier(Box::new(resolve_field_type(&te))),
                            optional: m.optional(),
                            shorthand: false,
                            default_value: None,
                            span: node_span(m.syntax()),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    ModelDef {
        name: name_text(block),
        extends: block
            .extends()
            .map(|e| e.parents().map(|t| t.text().to_string()).collect())
            .unwrap_or_default(),
        fields,
        span: node_span(block.syntax()),
    }
}

fn extract_enum(block: &BlockDecl) -> EnumDef {
    let mut variants = Vec::new();
    if let Some(body) = block.body() {
        for entry in body.entries() {
            let Entry::ListItem(item) = entry else { continue };
            if let Some(value) = item.value() {
                // Shorthand: `- "variant"`.
                if let Ok(sv) = value.decode() {
                    if let Value::String(s) = sv.value {
                        variants.push(s);
                    }
                }
            } else if let Some(name) = item.name() {
                // Reference: `- variant`.
                variants.push(name.text().to_string());
            }
        }
    }
    EnumDef {
        name: name_text(block),
        variants,
        span: node_span(block.syntax()),
    }
}

fn extract_oneof(decl: &OneOfDecl) -> OneOfDef {
    OneOfDef {
        name: decl.name().and_then(|n| n.text()).unwrap_or_default(),
        discriminator: token_text(decl.discriminator()),
        discriminator_type: decl.enum_type().map(|t| t.text().to_string()),
        default_discriminator: decl
            .default_value()
            .and_then(|t| decode_string_token(&t).ok()),
        variants: decl
            .arms()
            .filter_map(|arm| {
                let value = decode_string_token(&arm.value()?).ok()?;
                let model = arm.model()?.text().to_string();
                Some((value, model))
            })
            .collect(),
        span: node_span(decl.syntax()),
    }
}

fn resolve_field_type(te: &TypeExpr) -> FieldType {
    match te.kind() {
        TypeExprKind::Named => {
            let name = token_text(te.name());
            match name.parse::<PrimitiveType>() {
                Ok(prim) => FieldType::Primitive(prim),
                Err(_) => FieldType::ModelRef(name),
            }
        }
        TypeExprKind::Array => FieldType::List(Box::new(
            te.children()
                .next()
                .map(|inner| resolve_field_type(&inner))
                .unwrap_or_else(unknown_type),
        )),
        TypeExprKind::Union => {
            FieldType::Union(te.children().map(|v| resolve_field_type(&v)).collect())
        }
        TypeExprKind::Arms => {
            // `(K -> V)`: exactly two child type exprs, key then target.
            let mut children = te.children();
            let mut next = || {
                children
                    .next()
                    .map(|t| resolve_field_type(&t))
                    .unwrap_or_else(unknown_type)
            };
            FieldType::Arms {
                key: Box::new(next()),
                target: Box::new(next()),
            }
        }
    }
}

// ── small helpers ──────────────────────────────────────────────────────────

fn token_text(tok: Option<crate::cst::syntax::SyntaxToken>) -> String {
    tok.map(|t| t.text().to_string()).unwrap_or_default()
}

fn name_text(block: &BlockDecl) -> String {
    block.name().and_then(|n| n.text()).unwrap_or_default()
}

/// A placeholder type for the (impossible-on-valid-input) case of a missing type
/// — keeps extraction total; valid files always have a type here.
fn unknown_type() -> FieldType {
    FieldType::ModelRef(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cst::parse;

    /// Extraction over the whole grammar: inheritance, optional/typed/defaulted
    /// fields, union and modifier field types, enums, and a `oneof` — all read off
    /// the CST. `trait`/non-model/enum blocks are not extracted.
    #[test]
    fn extract_produces_expected_schema() {
        let src = "\
enum Status:
    - active
    - \"on-hold\"

model Base:
    id string
    createdAt string

trait Audited:
    auditedBy string

model Plan is Base:
    name string
    tier string?
    path path+
    slug string?+
    region string = \"us\"
    tags []string
    mixed (string | []int)
    mode (active | inactive)
    price money = 9.99 USD
    |visibility role?

oneof email by provider as providerKind = \"log\":
    \"log\" -> emailLog
    \"postmark\" -> emailPostmark
";
        let schema = extract(&Root::cast(parse(src).syntax()).unwrap());
        // `trait` is not a schema definition, so only `model`/`enum` extract.
        assert_eq!(schema.models.len(), 2);
        assert_eq!(schema.enums.len(), 1);
        assert_eq!(schema.oneofs.len(), 1);

        let plan = schema.models.iter().find(|m| m.name == "Plan").unwrap();
        assert_eq!(plan.extends, vec!["Base".to_string()]);
        let field = |n: &str| plan.fields.iter().find(|f| f.name == n).unwrap();
        assert!(field("tier").optional && !field("tier").shorthand);
        // `path path+` → shorthand, required.
        assert!(field("path").shorthand && !field("path").optional);
        // `slug string?+` → shorthand and optional (order-free flags).
        assert!(field("slug").shorthand && field("slug").optional);
        assert!(!field("name").shorthand && !field("name").optional);
        assert!(matches!(
            field("region").default_value.as_ref().map(|sv| &sv.value),
            Some(Value::String(s)) if s == "us"
        ));
        assert!(matches!(field("tags").field_type, FieldType::List(_)));
        assert!(matches!(field("mode").field_type, FieldType::Union(_)));
        assert!(matches!(
            field("price").default_value.as_ref().map(|sv| &sv.value),
            Some(Value::Money(_))
        ));
        // A modifier field (`|visibility`) is extracted with a Modifier type.
        assert!(plan
            .fields
            .iter()
            .any(|f| matches!(f.field_type, FieldType::Modifier(_)) && f.optional));

        let email = &schema.oneofs[0];
        assert_eq!(email.discriminator, "provider");
        assert_eq!(email.discriminator_type.as_deref(), Some("providerKind"));
        assert_eq!(email.default_discriminator.as_deref(), Some("log"));
    }

    #[test]
    fn extract_handles_nested_array_types() {
        // The CST is context-free (`[`/`]` are atoms the parser composes), so it
        // handles `[][]string` — a nested list type.
        let src = "model M:\n    grid [][]string\n";
        let schema = extract(&Root::cast(parse(src).syntax()).unwrap());
        assert_eq!(schema.models.len(), 1);
        let field = &schema.models[0].fields[0];
        assert_eq!(field.name, "grid");
        // List(List(Primitive(String)))
        assert!(
            matches!(
                &field.field_type,
                FieldType::List(inner)
                    if matches!(&**inner, FieldType::List(_))
            ),
            "expected nested list, got {:?}",
            field.field_type
        );
    }

    #[test]
    fn extracted_schema_feeds_validation_passes() {
        // The reused `ExtractedSchema` passes (cycles, inheritance) work on CST
        // output exactly as on legacy output.
        let src = "model A is B:\n    x string\n\nmodel B is A:\n    y string\n";
        let schema = extract(&Root::cast(parse(src).syntax()).unwrap());
        let cycles = crate::schema::find_extends_cycles(&schema);
        assert!(!cycles.is_empty(), "extends cycle should be detected on CST-extracted schema");
    }
}
