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
        // A modifier field carries its declared inner type; classify by it —
        // the same unwrap `resolve_type` performs, so a modifier-wrapped union
        // (`|slot (a | b)`) gets full body-aware + RFC 0015 annotation-aware
        // variant selection instead of degrading to the bare `Union` target.
        if let FieldType::Modifier(inner) = ty {
            return self.resolve_type_in_body(inner, body);
        }
        let FieldType::Union(variants) = ty else {
            return self.resolve_type(ty);
        };
        // RFC 0015: an explicit nominal annotation (`field as <Variant>:`) rides
        // on the body and selects the variant by *declared type name* — exact,
        // never shape-inferred. A valid annotation wins; an unknown one falls
        // through to structural (the validator reports it, so no consumer here
        // needs an error channel and none panics). This is the single point that
        // makes every union-variant consumer honor `as` — no per-consumer wiring.
        if let Some(name) = body.type_annotation.as_ref().map(|i| i.name.as_str()) {
            if let Some(variant) = self.select_variant_by_type_name(variants, name) {
                return self.resolve_type(variant);
            }
        }
        let has_list_items = body
            .entries
            .iter()
            .any(|e| matches!(e.kind, BodyEntryKind::ListItem(_)));
        let has_arms = body
            .entries
            .iter()
            .any(|e| matches!(e.kind, BodyEntryKind::Arm(_)));
        // A KEYED body (properties / nested blocks / modifiers) is a block
        // instance — a scalar variant cannot represent it, so it must select a
        // model/oneof-ref variant.
        let has_keyed = body.entries.iter().any(|e| {
            matches!(
                e.kind,
                BodyEntryKind::Property(_)
                    | BodyEntryKind::NestedBlock(_)
                    | BodyEntryKind::Modifier(_)
            )
        });
        // Body shape selects the variant (first matching wins, source order):
        // arms → the arm-set variant (RFC 0007); list items → the list variant;
        // a keyed block → the first model/oneof-ref variant (never a scalar —
        // a scalar can't hold keyed entries); otherwise (bare / empty) the
        // first scalar / model-ref variant.
        variants
            .iter()
            .find(|variant| match variant {
                FieldType::Arms { .. } => has_arms,
                FieldType::List(_) => !has_arms && has_list_items,
                FieldType::ModelRef(name) if has_keyed => {
                    !has_arms
                        && !has_list_items
                        && matches!(
                            self.resolve_ref(name),
                            FieldTarget::Model(_) | FieldTarget::OneOf(_)
                        )
                }
                _ => !has_arms && !has_list_items && !has_keyed,
            })
            .map(|variant| self.resolve_type(variant))
            .unwrap_or(FieldTarget::Leaf)
    }

    /// RFC 0015 nominal variant selection: the union variant whose declared
    /// **type name** equals `name` — the exact counterpart to the shape rule in
    /// [`resolve_type_in_body`](Self::resolve_type_in_body). Only *nameable*
    /// variants qualify: a `ModelRef` that resolves to a model or `oneof` (a
    /// block type an `as <Variant>` can name). Disjoint list/scalar variants are
    /// structurally unambiguous and intentionally not nameable. `None` when no
    /// variant matches — an unknown annotation, which the validator reports.
    /// The single nominal-selection primitive: the resolver selects with it, the
    /// validator checks membership with it, and completion/did-you-mean draw
    /// their candidate set from [`nameable_variant_names`](Self::nameable_variant_names).
    pub fn select_variant_by_type_name<'a>(
        &self,
        variants: &'a [FieldType],
        name: &str,
    ) -> Option<&'a FieldType> {
        variants.iter().find(|v| match v {
            FieldType::ModelRef(n) => {
                n == name
                    && matches!(
                        self.resolve_ref(n),
                        FieldTarget::Model(_) | FieldTarget::OneOf(_)
                    )
            }
            _ => false,
        })
    }

    /// The declared type names of a union's **nameable** variants (model/`oneof`
    /// refs) — the one candidate set powering `as`-position completion and the
    /// did-you-mean on an unknown annotation. Source order, so completion and
    /// diagnostics list variants as authored.
    pub fn nameable_variant_names<'a>(&self, variants: &'a [FieldType]) -> Vec<&'a str> {
        variants
            .iter()
            .filter_map(|v| match v {
                FieldType::ModelRef(n)
                    if matches!(
                        self.resolve_ref(n),
                        FieldTarget::Model(_) | FieldTarget::OneOf(_)
                    ) =>
                {
                    Some(n.as_str())
                }
                _ => None,
            })
            .collect()
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
    use crate::model::ModelKind;
    use crate::span::Span;

    fn model(name: &str, fields: Vec<FieldDef>) -> ModelDef {
        ModelDef {
            kind: ModelKind::Model,
            source: None,
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
                source: None,
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

        // Rule-completion: a KEYED block body under `(string | model)` selects
        // the MODEL variant, never the scalar (a scalar can't hold properties).
        let sm = FieldType::Union(vec![
            FieldType::Primitive(PrimitiveType::String),
            FieldType::ModelRef("step".into()),
        ]);
        let keyed = body_of("x X:\n    k = \"v\"\n");
        assert!(
            matches!(
                idx.resolve_type_in_body(&sm, &keyed),
                FieldTarget::Model(m) if m.name == "step"
            ),
            "a keyed body must resolve to the model variant, not the leading scalar"
        );
    }

    fn body_of(src: &str) -> Body {
        let file = crate::cst::parse_to_ast(src).unwrap();
        match &file.declarations[0].kind {
            crate::ast::DeclarationKind::Block(b) => b.body.clone(),
            _ => panic!("expected block"),
        }
    }

    /// The body of the nested block named `name` within `body`.
    fn nested_body<'a>(body: &'a Body, name: &str) -> &'a Body {
        body.entries
            .iter()
            .find_map(|e| match &e.kind {
                crate::ast::BodyEntryKind::NestedBlock(nb) if nb.name.name == name => {
                    Some(&nb.body)
                }
                _ => None,
            })
            .expect("nested block not found")
    }

    #[test]
    fn resolve_type_in_body_honors_nominal_annotation() {
        // Two same-class models — shape inference cannot disambiguate them.
        let idx = SchemaIndex::build(
            vec![model("modelA", vec![]), model("modelB", vec![])],
            vec![],
            vec![],
        );
        let union = FieldType::Union(vec![
            FieldType::ModelRef("modelA".into()),
            FieldType::ModelRef("modelB".into()),
        ]);
        let FieldType::Union(variants) = &union else {
            unreachable!()
        };

        // `slot as modelB:` — the annotation lowers onto slot's body and selects
        // the *named* variant, overriding structural first-wins.
        let outer = body_of("x X:\n    slot as modelB:\n        k = \"v\"\n");
        let slot = nested_body(&outer, "slot");
        assert_eq!(
            slot.type_annotation.as_ref().map(|i| i.name.as_str()),
            Some("modelB"),
            "the `as` annotation must lower onto the body"
        );
        assert!(
            matches!(idx.resolve_type_in_body(&union, slot), FieldTarget::Model(m) if m.name == "modelB"),
            "a valid annotation selects the named variant"
        );

        // Same shape, no annotation → structural first-wins (modelA).
        let plain = body_of("x X:\n    slot:\n        k = \"v\"\n");
        assert!(
            matches!(idx.resolve_type_in_body(&union, nested_body(&plain, "slot")), FieldTarget::Model(m) if m.name == "modelA"),
            "no annotation → structural first-declared"
        );

        // Unknown annotation → falls through to structural (never panics; the
        // validator reports the unknown variant separately).
        let bad = body_of("x X:\n    slot as nope:\n        k = \"v\"\n");
        assert!(
            matches!(idx.resolve_type_in_body(&union, nested_body(&bad, "slot")), FieldTarget::Model(m) if m.name == "modelA"),
            "an unknown annotation degrades to structural, does not panic"
        );

        // `select_variant_by_type_name` is exact and membership-checked.
        assert!(idx
            .select_variant_by_type_name(variants, "modelB")
            .is_some());
        assert!(idx.select_variant_by_type_name(variants, "nope").is_none());
        assert_eq!(
            idx.nameable_variant_names(variants),
            vec!["modelA", "modelB"]
        );
    }
}
