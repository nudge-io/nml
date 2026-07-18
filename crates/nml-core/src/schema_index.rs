//! Order-preserving schema lookup and field dispatch.
//!
//! [`SchemaIndex`] owns the model / enum / `oneof` definitions and provides
//! `O(1)` lookup by name while preserving definition order and
//! first-definition-wins semantics (a bare `HashMap` would lose both). It is the
//! single source of schema dispatch shared by validation and defaulting:
//! [`SchemaIndex::resolve_field`] classifies a field into the [`FieldTarget`] it
//! resolves to, so neither consumer re-derives that logic.

use std::collections::HashMap;

use crate::ast::{Body, BodyEntryKind};
use crate::model::{EnumDef, FieldDef, FieldType, ModelDef, OneOfDef};
use crate::types::PrimitiveType;

/// What a model field resolves to, for schema-guided traversal.
///
/// Borrows the referenced definition from the owning [`SchemaIndex`]. Most
/// variants borrow only from the index; [`FieldTarget::Arms`] borrows the
/// key/target types from the field type itself (both live in the index's
/// models in practice).
#[derive(Debug)]
pub enum FieldTarget<'a> {
    /// A nested model instance — recurse into its body with this model.
    Model(&'a ModelDef),
    /// A discriminated union — resolve the discriminator, then the variant.
    OneOf(&'a OneOfDef),
    /// A list whose items resolve to the boxed target.
    ListOf(Box<FieldTarget<'a>>),
    /// A `set<T>` whose items resolve to the boxed target (RFC 0032). Shape
    /// validation is exactly `ListOf`; the validator additionally rejects
    /// duplicate elements (value-level identity) at load.
    SetOf(Box<FieldTarget<'a>>),
    /// Free-form `object` — accepts arbitrary keys; no schema to recurse into.
    Object,
    /// A type union — ambiguous without a discriminator; not recursed.
    Union,
    /// A typed arm set `(K -> V)` (RFC 0007) — the body holds routing arms;
    /// keys validate against `key`, targets are typed by `target` (reference
    /// targets are consumer-resolved; `target` drives editor intelligence).
    Arms {
        key: &'a FieldType,
        target: &'a FieldType,
    },
    /// A primitive scalar, enum, or unknown reference — a leaf value.
    Leaf,
}

/// Owns schema definitions with `O(1)`, order-preserving, first-wins lookup.
#[derive(Debug, Default)]
pub struct SchemaIndex {
    models: Vec<ModelDef>,
    model_pos: HashMap<String, usize>,
    enums: Vec<EnumDef>,
    enum_pos: HashMap<String, usize>,
    oneofs: Vec<OneOfDef>,
    oneof_pos: HashMap<String, usize>,
}

impl SchemaIndex {
    /// Build an index from extracted definitions. On a duplicate name the first
    /// occurrence wins (matching the validator's authoritative-first behavior);
    /// iteration order is preserved.
    pub fn build(models: Vec<ModelDef>, enums: Vec<EnumDef>, oneofs: Vec<OneOfDef>) -> Self {
        let model_pos = first_wins(&models, |m| &m.name);
        let enum_pos = first_wins(&enums, |e| &e.name);
        let oneof_pos = first_wins(&oneofs, |o| &o.name);
        Self {
            models,
            model_pos,
            enums,
            enum_pos,
            oneofs,
            oneof_pos,
        }
    }

    pub fn model(&self, name: &str) -> Option<&ModelDef> {
        self.model_pos.get(name).map(|&i| &self.models[i])
    }

    pub fn enum_def(&self, name: &str) -> Option<&EnumDef> {
        self.enum_pos.get(name).map(|&i| &self.enums[i])
    }

    pub fn oneof(&self, name: &str) -> Option<&OneOfDef> {
        self.oneof_pos.get(name).map(|&i| &self.oneofs[i])
    }

    /// Definitions in source order, for order-sensitive passes (cycle and
    /// duplicate reporting).
    pub fn models(&self) -> &[ModelDef] {
        &self.models
    }

    pub fn enums(&self) -> &[EnumDef] {
        &self.enums
    }

    pub fn oneofs(&self) -> &[OneOfDef] {
        &self.oneofs
    }

    /// Classify a field by the target it resolves to. Pure dispatch shared by
    /// validation and defaulting. An [`FieldTarget::Arms`] result borrows the
    /// key/target types from the field itself; every other variant borrows
    /// only from the index.
    pub fn resolve_field<'a>(&'a self, field: &'a FieldDef) -> FieldTarget<'a> {
        self.resolve_type(&field.field_type)
    }

    /// Resolve a type against a known instance body — the one dispatch that needs
    /// the body. For a **union** it applies the `has_list_items` rule to select the
    /// variant and returns that variant's resolved target (a concrete
    /// Model/OneOf/Leaf/ListOf — never `Union`); for any other type it is exactly
    /// `resolve_field`/`resolve_type`. This is the single definition of the
    /// body-dependent variant selection, shared by the validator's walk and the
    /// defaulter's walk so neither re-derives it.
    pub fn resolve_type_in_body<'a>(&'a self, ty: &'a FieldType, body: &Body) -> FieldTarget<'a> {
        let FieldType::Union(variants) = ty else {
            return self.resolve_type(ty);
        };
        let has_list_items = body
            .entries
            .iter()
            .any(|e| matches!(e.kind, BodyEntryKind::ListItem(_)));
        let has_arms = body
            .entries
            .iter()
            .any(|e| matches!(e.kind, BodyEntryKind::Arm(_)));
        // Body shape selects the variant: arm entries → the arm-set variant
        // (RFC 0007); list items → the list variant; otherwise the scalar /
        // model-ref variant. First matching variant wins (source order).
        variants
            .iter()
            .find(|variant| match variant {
                FieldType::Arms { .. } => has_arms,
                FieldType::List(_) => !has_arms && has_list_items,
                _ => !has_arms && !has_list_items,
            })
            .map(|variant| self.resolve_type(variant))
            .unwrap_or(FieldTarget::Leaf)
    }

    /// Resolve a named type reference (`someModel`) to its target: a model, a
    /// `oneof`, or a leaf (enum or unknown name). The single definition of
    /// name→target dispatch, shared by schema validation and defaulting.
    pub fn resolve_ref(&self, name: &str) -> FieldTarget<'_> {
        if let Some(m) = self.model(name) {
            FieldTarget::Model(m)
        } else if let Some(o) = self.oneof(name) {
            FieldTarget::OneOf(o)
        } else {
            FieldTarget::Leaf
        }
    }

    fn resolve_type<'a>(&'a self, ty: &'a FieldType) -> FieldTarget<'a> {
        match ty {
            FieldType::Primitive(PrimitiveType::Object) => FieldTarget::Object,
            FieldType::Primitive(_) => FieldTarget::Leaf,
            FieldType::List(inner) => FieldTarget::ListOf(Box::new(self.resolve_type(inner))),
            FieldType::Set(inner) => FieldTarget::SetOf(Box::new(self.resolve_type(inner))),
            // A modifier field carries its declared inner type; classify by it.
            FieldType::Modifier(inner) => self.resolve_type(inner),
            FieldType::Union(_) => FieldTarget::Union,
            FieldType::Arms { key, target } => FieldTarget::Arms { key, target },
            FieldType::ModelRef(name) => self.resolve_ref(name),
        }
    }
}

/// Map each item's name to its first occurrence's index (`or_insert` keeps the
/// first, discarding later duplicates).
fn first_wins<T>(items: &[T], name: impl Fn(&T) -> &str) -> HashMap<String, usize> {
    let mut pos = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        pos.entry(name(item).to_string()).or_insert(i);
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::Span;

    fn model(name: &str, fields: Vec<FieldDef>) -> ModelDef {
        ModelDef {
            name: name.to_string(),
            extends: Vec::new(),
            fields,
            span: Span::empty(0),
        }
    }

    fn field(name: &str, ty: FieldType) -> FieldDef {
        FieldDef {
            name: name.to_string(),
            field_type: ty,
            optional: false,
            shorthand: false,
            default_value: None,
            directives: Vec::new(),
            doc: None,
            span: Span::empty(0),
        }
    }

    #[test]
    fn lookup_is_first_wins() {
        let idx = SchemaIndex::build(
            vec![
                model(
                    "dup",
                    vec![field("a", FieldType::Primitive(PrimitiveType::String))],
                ),
                model(
                    "dup",
                    vec![field("b", FieldType::Primitive(PrimitiveType::String))],
                ),
            ],
            vec![],
            vec![],
        );
        // First definition is authoritative.
        assert_eq!(idx.model("dup").unwrap().fields[0].name, "a");
    }

    #[test]
    fn iteration_preserves_order() {
        let idx = SchemaIndex::build(
            vec![model("first", vec![]), model("second", vec![])],
            vec![],
            vec![],
        );
        let names: Vec<&str> = idx.models().iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second"]);
    }

    #[test]
    fn resolve_field_dispatch() {
        let idx = SchemaIndex::build(vec![model("inner", vec![])], vec![], vec![]);

        assert!(matches!(
            idx.resolve_field(&field("x", FieldType::Primitive(PrimitiveType::String))),
            FieldTarget::Leaf
        ));
        assert!(matches!(
            idx.resolve_field(&field("x", FieldType::Primitive(PrimitiveType::Object))),
            FieldTarget::Object
        ));
        assert!(matches!(
            idx.resolve_field(&field("x", FieldType::ModelRef("inner".into()))),
            FieldTarget::Model(m) if m.name == "inner"
        ));
        assert!(matches!(
            idx.resolve_field(&field("x", FieldType::ModelRef("unknown".into()))),
            FieldTarget::Leaf
        ));
        assert!(matches!(
            idx.resolve_field(&field(
                "x",
                FieldType::List(Box::new(FieldType::ModelRef("inner".into())))
            )),
            FieldTarget::ListOf(inner) if matches!(*inner, FieldTarget::Model(_))
        ));
        assert!(matches!(
            idx.resolve_field(&field(
                "x",
                FieldType::Union(vec![FieldType::Primitive(PrimitiveType::String)])
            )),
            FieldTarget::Union
        ));
    }

    #[test]
    fn resolve_field_oneof() {
        let idx = SchemaIndex::build(
            vec![model("varA", vec![])],
            vec![],
            vec![OneOfDef {
                name: "u".into(),
                discriminator: "kind".into(),
                discriminator_type: None,
                default_discriminator: None,
                variants: vec![("a".into(), "varA".into())],
                span: Span::empty(0),
            }],
        );
        assert!(matches!(
            idx.resolve_field(&field("x", FieldType::ModelRef("u".into()))),
            FieldTarget::OneOf(o) if o.name == "u"
        ));
    }

    #[test]
    fn resolve_type_in_body_selects_union_variant_by_body_shape() {
        let idx = SchemaIndex::build(vec![model("step", vec![])], vec![], vec![]);
        // `(step | []step)` — the workflow `parallel` shape.
        let union = FieldType::Union(vec![
            FieldType::ModelRef("step".into()),
            FieldType::List(Box::new(FieldType::ModelRef("step".into()))),
        ]);

        // A scalar body selects the model-ref variant → Model.
        let scalar = body_of("x X:\n    k = \"v\"\n");
        assert!(matches!(
            idx.resolve_type_in_body(&union, &scalar),
            FieldTarget::Model(m) if m.name == "step"
        ));

        // A list-shaped body selects the list variant → ListOf(Model).
        let list = body_of("x X:\n    - A:\n        k = \"v\"\n");
        assert!(matches!(
            idx.resolve_type_in_body(&union, &list),
            FieldTarget::ListOf(inner) if matches!(*inner, FieldTarget::Model(_))
        ));
    }

    fn body_of(src: &str) -> Body {
        let file = crate::cst::parse_to_ast(src).unwrap();
        match &file.declarations[0].kind {
            crate::ast::DeclarationKind::Block(b) => b.body.clone(),
            _ => panic!("expected block"),
        }
    }
}
