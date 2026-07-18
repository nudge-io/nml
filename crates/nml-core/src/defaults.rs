//! Schema-guided default injection.
//!
//! [`apply_defaults`] walks a body against the model or `oneof` its root names
//! and injects schema-declared field defaults the instance omits, producing an
//! enriched body. It is a pure AST→AST transform built on [`SchemaIndex`]: it
//! performs no validation and never fails — an unmodeled root, an unknown field,
//! or an absent `oneof` discriminator is left untouched.
//!
//! [`from_body_defaulted`] / [`from_block_defaulted`] compose it into the
//! canonical deserialize pipeline `apply_shared_properties → apply_defaults →
//! resolve_body → from_block`. Resolution runs **last** so an injected
//! `$ENV`/fallback default is resolved on equal footing with author-written
//! values, and the schema-aware passes never handle resolved secret material.

use serde::Deserialize;

use crate::ast::*;
use crate::de::{self, from_block};
use crate::model::{FieldType, ModelDef, OneOfDef};
use crate::resolve::{apply_shared_properties, ValueResolver};
use crate::schema_index::{FieldTarget, SchemaIndex};
use crate::span::Span;
use crate::types::{SpannedValue, Value};

/// Recursion bound mirroring the validator's `MAX_VALIDATION_DEPTH`. Bounds the
/// *depth* of recursion into authored and materialized structure.
const MAX_DEFAULT_DEPTH: u32 = 64;

/// Upper bound on the number of nested models materialized from defaults in a
/// single pass. The depth guard caps recursion *depth* but not *width*: a
/// diamond-shaped fully-defaultable schema (each model with several required
/// refs to the next) would otherwise synthesize exponentially many blocks within
/// the depth limit. This caps total synthesis, so a hostile schema degrades to a
/// missing-required-field validation error rather than memory exhaustion. The
/// limit is far above any real config (materialization is rare and shallow in
/// practice).
const MAX_MATERIALIZED_MODELS: u32 = 1024;

/// Inject schema defaults into `body`, dispatching on whether `root` names a
/// model or a `oneof` (a top-level union such as `email`). An unmodeled `root`
/// yields the body unchanged — defaulting only ever adds values, never blocks.
pub fn apply_defaults(index: &SchemaIndex, root: &str, body: &Body) -> Body {
    let defaulter = Defaulter::new(index);
    if let Some(model) = index.model(root) {
        defaulter.model_body(model, body, 0)
    } else if let Some(oneof) = index.oneof(root) {
        defaulter.oneof_body(oneof, body, 0)
    } else {
        body.clone()
    }
}

/// Canonical defaulted deserialize against an explicit root model/oneof name.
pub fn from_body_defaulted<T>(
    index: &SchemaIndex,
    root: &str,
    body: &Body,
    resolver: &ValueResolver,
) -> Result<T, de::Error>
where
    T: for<'de> Deserialize<'de>,
{
    // Materialize scalar shorthand items into bodies *first*, so an item's own token
    // beats a shared property and `de` can read it as a struct (RFC 0005 §10).
    let positional = crate::identity::apply_positional(index, root, body);
    let shared = apply_shared_properties(&positional);
    let defaulted = apply_defaults(index, root, &shared);
    let resolved = resolver.resolve_body(&defaulted)?;
    from_block(&resolved)
}

/// [`from_body_defaulted`] keyed by a block's keyword (its root model/oneof), so
/// keyword and body cannot be mismatched.
pub fn from_block_defaulted<T>(
    index: &SchemaIndex,
    block: &BlockDecl,
    resolver: &ValueResolver,
) -> Result<T, de::Error>
where
    T: for<'de> Deserialize<'de>,
{
    from_body_defaulted(index, &block.keyword.name, &block.body, resolver)
}

// ---------------------------------------------------------------------------
// Core transform
// ---------------------------------------------------------------------------

/// One defaulting pass over a schema. Owns the index and a once-computed set of
/// fully-defaultable model names, so materialization decisions are `O(1)`
/// lookups rather than per-field recursion (see [`Defaulter::new`]).
struct Defaulter<'a> {
    index: &'a SchemaIndex,
    /// Names of models every required field of which can be satisfied without an
    /// authored value, transitively. Membership ⇒ the model can be materialized
    /// from defaults alone.
    defaultable: std::collections::HashSet<&'a str>,
    /// Remaining materialization budget (see [`MAX_MATERIALIZED_MODELS`]).
    budget: std::cell::Cell<u32>,
}

impl<'a> Defaulter<'a> {
    fn new(index: &'a SchemaIndex) -> Self {
        Self {
            index,
            defaultable: compute_defaultable(index),
            budget: std::cell::Cell::new(MAX_MATERIALIZED_MODELS),
        }
    }

    /// Consume one unit of materialization budget, returning `false` once it is
    /// exhausted (whereupon further required defaultable models are left absent
    /// for validation to report, instead of being synthesized).
    fn take_budget(&self) -> bool {
        match self.budget.get() {
            0 => false,
            n => {
                self.budget.set(n - 1);
                true
            }
        }
    }

    fn model_body(&self, model: &ModelDef, body: &Body, depth: u32) -> Body {
        if depth >= MAX_DEFAULT_DEPTH {
            return body.clone();
        }

        let mut entries: Vec<BodyEntry> =
            Vec::with_capacity(body.entries.len() + model.fields.len());
        let mut present: Vec<&str> = Vec::new();

        // Pass 1: keep every authored entry, recursing into nested blocks that
        // resolve to a model / oneof / list-of-those.
        for entry in &body.entries {
            match &entry.kind {
                BodyEntryKind::NestedBlock(nb) => {
                    present.push(&nb.name.name);
                    let recursed = self.recurse_nested(model, &nb.name.name, &nb.body, depth);
                    entries.push(BodyEntry {
                        kind: BodyEntryKind::NestedBlock(NestedBlock {
                            name: nb.name.clone(),
                            body: recursed,
                        }),
                        span: entry.span,
                    });
                }
                BodyEntryKind::Property(p) => {
                    present.push(&p.name.name);
                    entries.push(entry.clone());
                }
                BodyEntryKind::Modifier(m) => {
                    present.push(&m.name.name);
                    entries.push(entry.clone());
                }
                _ => entries.push(entry.clone()),
            }
        }

        // Pass 2: inject defaults for absent fields. A declared default wins;
        // failing that, an absent *required* fully-defaultable nested model is
        // materialized from its own defaults (mirrors serde's `#[serde(default)]`
        // struct synthesis). An optional model field is left absent so serde
        // reads it as `None` — materializing it would turn `None` into
        // `Some(default)`, which is not what an `Option<T>` field means.
        for field in &model.fields {
            if present.contains(&field.name.as_str()) {
                continue;
            }
            if let Some(default) = &field.default_value {
                entries.push(property_entry(&field.name, field.span, default.clone()));
            } else if !field.optional {
                if let FieldTarget::Model(nested) = self.index.resolve_field(field) {
                    if self.is_defaultable(nested) && self.take_budget() {
                        let materialized = self.model_body(nested, &EMPTY_BODY, depth + 1);
                        entries.push(BodyEntry {
                            kind: BodyEntryKind::NestedBlock(NestedBlock {
                                name: Identifier::new(field.name.clone(), field.span),
                                body: materialized,
                            }),
                            span: field.span,
                        });
                    }
                }
            }
        }

        Body { entries }
    }

    /// Recurse into an authored nested block, dispatching on the parent field.
    /// A list field is handled by its inner *type* (not just the resolved target)
    /// so a `[](a | b)` union item's variant can be selected per item. An unknown
    /// field or a non-recursable target (object, bare union, leaf) passes through.
    fn recurse_nested(
        &self,
        model: &ModelDef,
        field_name: &str,
        nb_body: &Body,
        depth: u32,
    ) -> Body {
        let Some(field) = model.fields.iter().find(|f| f.name == field_name) else {
            return nb_body.clone();
        };
        // A set's items default exactly like a list's (RFC 0032): element
        // instances get their model defaults; uniqueness is validation's job.
        if let FieldType::List(inner) | FieldType::Set(inner) = &field.field_type {
            return self.list_body(inner, nb_body, depth + 1);
        }
        match self.index.resolve_field(field) {
            FieldTarget::Model(m) => self.model_body(m, nb_body, depth + 1),
            FieldTarget::OneOf(o) => self.oneof_body(o, nb_body, depth + 1),
            // Arm bodies carry only selectors and reference targets (RFC
            // 0007) — nothing to default.
            FieldTarget::ListOf(_)
            | FieldTarget::SetOf(_)
            | FieldTarget::Object
            | FieldTarget::Union
            | FieldTarget::Arms { .. }
            | FieldTarget::Leaf => nb_body.clone(),
        }
    }

    /// Default each named item of a list field against its inner type, selecting
    /// the union variant per item via the shared `resolve_type_in_body`. Items are
    /// never synthesized — an absent list stays empty.
    fn list_body(&self, inner: &FieldType, body: &Body, depth: u32) -> Body {
        map_named_items(body, |item_body| {
            let target = self.index.resolve_type_in_body(inner, item_body);
            self.default_against(target, item_body, depth)
        })
    }

    /// Default `body` against an already-resolved target — the defaulter's analogue
    /// of the validator's `validate_target_instance`. A `ListOf` target (a union's
    /// list variant, e.g. the `[]step` arm of `(step | []step)`) defaults each named
    /// item against the item target.
    fn default_against(&self, target: FieldTarget, body: &Body, depth: u32) -> Body {
        match target {
            FieldTarget::Model(m) => self.model_body(m, body, depth),
            FieldTarget::OneOf(o) => self.oneof_body(o, body, depth),
            FieldTarget::ListOf(inner) | FieldTarget::SetOf(inner) => {
                map_named_items(body, |item_body| match inner.as_ref() {
                    FieldTarget::Model(m) => self.model_body(m, item_body, depth + 1),
                    FieldTarget::OneOf(o) => self.oneof_body(o, item_body, depth + 1),
                    _ => item_body.clone(),
                })
            }
            FieldTarget::Object
            | FieldTarget::Union
            | FieldTarget::Arms { .. }
            | FieldTarget::Leaf => body.clone(),
        }
    }

    /// Default a `oneof` instance: resolve the present discriminator to a variant
    /// model and default that variant's fields into the same body. When the
    /// discriminator is absent but the `oneof` declares a default arm, the
    /// discriminator is injected (before serde sees it) and the default variant is
    /// then defaulted. An absent discriminator with no default, or an unknown
    /// discriminator, is left untouched for validation to report.
    fn oneof_body(&self, oneof: &OneOfDef, body: &Body, depth: u32) -> Body {
        if depth >= MAX_DEFAULT_DEPTH {
            return body.clone();
        }
        // The authored discriminator *value*, if the property is present at all.
        let authored = body.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::Property(p) if p.name.name == oneof.discriminator => {
                Some(&p.value.value)
            }
            _ => None,
        });
        // Resolve the effective discriminator and whether it must be injected.
        let (discriminator, inject) = match authored {
            Some(value) => match value.as_str() {
                Some(s) => (s.to_string(), false),
                // Present but not a string: leave it for validation to report. Do
                // *not* inject — that would duplicate the discriminator property.
                None => return body.clone(),
            },
            // Absent: inject the declared default arm, if the union has one.
            None => match &oneof.default_discriminator {
                Some(default) => (default.clone(), true),
                None => return body.clone(),
            },
        };
        let Some((_, variant)) = oneof
            .variants
            .iter()
            .find(|(v, _)| v.as_str() == discriminator.as_str())
        else {
            return body.clone();
        };
        let Some(model) = self.index.model(variant) else {
            return body.clone();
        };
        if inject {
            let injected =
                inject_discriminator(body, &oneof.discriminator, &discriminator, oneof.span);
            self.model_body(model, &injected, depth)
        } else {
            self.model_body(model, body, depth)
        }
    }

    fn is_defaultable(&self, model: &ModelDef) -> bool {
        self.defaultable.contains(model.name.as_str())
    }
}

/// Compute the set of fully-defaultable model names by least fixpoint: a model
/// joins the set once every required, non-defaulted field is a model-ref to a
/// model already in the set. Starting empty and only ever adding members means a
/// required reference cycle never becomes defaultable (it can never be satisfied
/// without an authored value), and the whole computation is polynomial —
/// `O(models² · fields)` worst case — never the exponential blow-up a naive
/// per-field recursion would suffer on a diamond-shaped schema.
fn compute_defaultable(index: &SchemaIndex) -> std::collections::HashSet<&str> {
    let mut set: std::collections::HashSet<&str> = std::collections::HashSet::new();
    loop {
        let mut changed = false;
        for model in index.models() {
            if set.contains(model.name.as_str()) {
                continue;
            }
            let satisfiable = model.fields.iter().all(|f| {
                f.optional
                    || f.default_value.is_some()
                    || matches!(
                        index.resolve_field(f),
                        FieldTarget::Model(m) if set.contains(m.name.as_str())
                    )
            });
            if satisfiable {
                set.insert(model.name.as_str());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    set
}

/// Rebuild a body, replacing each named list item's body with `f(item_body)` and
/// leaving every other entry untouched. Centralizes the list-item rewrite shared by
/// the list-field and union-list-variant defaulting paths.
fn map_named_items(body: &Body, f: impl Fn(&Body) -> Body) -> Body {
    let entries = body
        .entries
        .iter()
        .map(|entry| match &entry.kind {
            BodyEntryKind::ListItem(item) => match &item.kind {
                ListItemKind::Named {
                    name,
                    body: item_body,
                } => BodyEntry {
                    kind: BodyEntryKind::ListItem(ListItem {
                        kind: ListItemKind::Named {
                            name: name.clone(),
                            body: f(item_body),
                        },
                        span: item.span,
                    }),
                    span: entry.span,
                },
                _ => entry.clone(),
            },
            _ => entry.clone(),
        })
        .collect();
    Body { entries }
}

/// Build a synthesized `Property` body entry (a value the instance omitted). The
/// injected name carries `name_span`; the entry span follows the value's own span.
/// Shared by scalar-default injection and `oneof` discriminator injection.
fn property_entry(name: &str, name_span: Span, value: SpannedValue) -> BodyEntry {
    BodyEntry {
        span: value.span,
        kind: BodyEntryKind::Property(Property {
            name: Identifier::new(name.to_string(), name_span),
            value,
        }),
    }
}

/// Append a synthesized discriminator property (`name = "value"`) to a body, used
/// when a `oneof` instance omits the discriminator and the union declares a default.
/// The injected name/value carry the `oneof` declaration span (they are not authored).
fn inject_discriminator(body: &Body, name: &str, value: &str, span: Span) -> Body {
    let mut entries = body.entries.clone();
    entries.push(property_entry(
        name,
        span,
        SpannedValue::new(Value::String(value.to_string()), span),
    ));
    Body { entries }
}

/// Empty body used as the seed when materializing a fully-defaultable nested
/// model that the instance omitted entirely.
const EMPTY_BODY: Body = Body {
    entries: Vec::new(),
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::ValueResolver;
    use crate::types::Value;

    #[test]
    fn scalar_shorthand_deserializes_into_struct() {
        // The de-path (RFC 0005 §10): a bare scalar and a scalar-with-body in a
        // `[]resource` field deserialize into the element struct — the `!` field is
        // filled from the scalar, the body fills the rest.
        #[derive(serde::Deserialize)]
        struct Resource {
            path: String,
            method: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct Svc {
            resources: Vec<Resource>,
        }

        let index = index_from(
            "model resource:\n    path string+\n    method string?\n\nmodel svc:\n    resources []resource\n",
        );
        let body = body_of(
            "svc s:\n    resources:\n        - \"/api\"\n        - \"/health\":\n            method = \"GET\"\n",
        );
        let svc: Svc = from_body_defaulted(&index, "svc", &body, &ValueResolver::env()).unwrap();

        assert_eq!(svc.resources.len(), 2);
        assert_eq!(svc.resources[0].path, "/api");
        assert_eq!(svc.resources[0].method, None);
        assert_eq!(svc.resources[1].path, "/health");
        assert_eq!(svc.resources[1].method.as_deref(), Some("GET"));
    }

    #[test]
    fn apply_positional_recurses_into_oneof_variant() {
        // A scalar shorthand nested inside a `oneof` variant is materialized too — the
        // de-path recurses into the resolved variant, matching the validator (closes a
        // validator/`de` agreement gap).
        let index = index_from(
            "model resource:\n    path string+\n\nmodel deployAction:\n    kind string\n    resources []resource\noneof action by kind:\n    \"deploy\" => deployAction\nmodel wrapper:\n    action action\n",
        );
        let body = body_of(
            "wrapper w:\n    action:\n        kind = \"deploy\"\n        resources:\n            - \"/api\"\n",
        );
        let out = crate::identity::apply_positional(&index, "wrapper", &body);

        let nested = |b: &Body, name: &str| {
            b.entries
                .iter()
                .find_map(|e| match &e.kind {
                    BodyEntryKind::NestedBlock(nb) if nb.name.name == name => Some(nb.body.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no nested block '{name}'"))
        };
        let resources = nested(&nested(&out, "action"), "resources");
        let item = resources
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::ListItem(i) => Some(i),
                _ => None,
            })
            .expect("a list item");
        let ListItemKind::Shorthand { body: Some(b), .. } = &item.kind else {
            panic!("scalar inside the oneof variant should be materialized");
        };
        assert!(
            b.entries
                .iter()
                .any(|e| matches!(&e.kind, BodyEntryKind::Property(p) if p.name.name == "path")),
            "`path` materialized inside the oneof variant"
        );
    }

    fn index_from(schema: &str) -> SchemaIndex {
        let mut ex = crate::cst::extract_schema(schema).0;
        crate::schema::resolve_model_inheritance(&mut ex);
        SchemaIndex::build(ex.models, ex.enums, ex.oneofs)
    }

    fn body_of(src: &str) -> Body {
        let file = crate::cst::parse_to_ast(src).unwrap();
        match &file.declarations[0].kind {
            DeclarationKind::Block(b) => b.body.clone(),
            _ => panic!("expected block"),
        }
    }

    fn prop<'a>(body: &'a Body, name: &str) -> Option<&'a Value> {
        body.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::Property(p) if p.name.name == name => Some(&p.value.value),
            _ => None,
        })
    }

    fn nested<'a>(body: &'a Body, name: &str) -> Option<&'a Body> {
        body.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == name => Some(&nb.body),
            _ => None,
        })
    }

    fn item<'a>(body: &'a Body, item_name: &str) -> Option<&'a Body> {
        body.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::ListItem(it) => match &it.kind {
                ListItemKind::Named { name, body } if name.name == item_name => Some(body),
                _ => None,
            },
            _ => None,
        })
    }

    #[test]
    fn scalar_default_injected_when_absent() {
        let idx = index_from(
            "model prompt:\n    outputFormat string = \"text\"\n    temperature number = 0.7\n",
        );
        let out = apply_defaults(
            &idx,
            "prompt",
            &body_of("prompt P:\n    temperature = 0.9\n"),
        );
        assert_eq!(
            prop(&out, "outputFormat"),
            Some(&Value::String("text".into()))
        );
        assert_eq!(prop(&out, "temperature"), Some(&Value::number(0.9)));
    }

    #[test]
    fn explicit_value_wins_over_default() {
        let idx = index_from("model prompt:\n    outputFormat string = \"text\"\n");
        let out = apply_defaults(
            &idx,
            "prompt",
            &body_of("prompt P:\n    outputFormat = \"json\"\n"),
        );
        assert_eq!(
            prop(&out, "outputFormat"),
            Some(&Value::String("json".into()))
        );
        // Exactly one outputFormat entry — the default did not duplicate it.
        let count = out
            .entries
            .iter()
            .filter(
                |e| matches!(&e.kind, BodyEntryKind::Property(p) if p.name.name == "outputFormat"),
            )
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn nested_block_recursion_injects_child_defaults() {
        let idx = index_from(
            "model inner:\n    a string = \"x\"\n    b string = \"y\"\n\nmodel outer:\n    sub inner\n",
        );
        let out = apply_defaults(
            &idx,
            "outer",
            &body_of("outer O:\n    sub:\n        b = \"override\"\n"),
        );
        let sub = nested(&out, "sub").expect("sub block preserved");
        assert_eq!(prop(sub, "a"), Some(&Value::String("x".into())));
        assert_eq!(prop(sub, "b"), Some(&Value::String("override".into())));
    }

    #[test]
    fn absent_fully_defaultable_nested_model_is_materialized() {
        let idx = index_from(
            "model inner:\n    a string = \"x\"\n    b string = \"y\"\n\nmodel outer:\n    sub inner\n    name string = \"o\"\n",
        );
        let out = apply_defaults(&idx, "outer", &body_of("outer O:\n    name = \"keep\"\n"));
        let sub = nested(&out, "sub").expect("fully-defaultable sub materialized");
        assert_eq!(prop(sub, "a"), Some(&Value::String("x".into())));
        assert_eq!(prop(sub, "b"), Some(&Value::String("y".into())));
        assert_eq!(prop(&out, "name"), Some(&Value::String("keep".into())));
    }

    #[test]
    fn absent_optional_nested_model_is_not_materialized() {
        // Even when fully defaultable, an *optional* nested model must stay
        // absent (serde reads it as `None`); only required ones materialize.
        let idx = index_from(
            "model inner:\n    a string = \"x\"\n\nmodel outer:\n    sub inner?\n    name string = \"o\"\n",
        );
        let out = apply_defaults(&idx, "outer", &body_of("outer O:\n    name = \"keep\"\n"));
        assert!(
            nested(&out, "sub").is_none(),
            "optional fully-defaultable model must not be materialized"
        );
    }

    #[test]
    fn absent_non_defaultable_nested_model_is_not_materialized() {
        let idx = index_from(
            "model inner:\n    required string\n    a string = \"x\"\n\nmodel outer:\n    sub inner\n",
        );
        let out = apply_defaults(
            &idx,
            "outer",
            &body_of("outer O:\n    sub:\n        required = \"r\"\n"),
        );
        // Present sub recurses (a injected), but an absent sub would NOT be materialized.
        assert!(nested(&out, "sub").is_some());

        let out2 = apply_defaults(&idx, "outer", &body_of("outer O:\n    other = \"z\"\n"));
        assert!(
            nested(&out2, "sub").is_none(),
            "non-defaultable model must not be materialized"
        );
    }

    #[test]
    fn list_items_get_per_item_defaults() {
        let idx = index_from(
            "model step:\n    retries number = 3\n    other string?\n\nmodel flow:\n    steps []step\n",
        );
        let src = "flow F:\n    steps:\n        - StepA:\n            retries = 5\n        - StepB:\n            other = \"x\"\n";
        let out = apply_defaults(&idx, "flow", &body_of(src));
        let steps = nested(&out, "steps").expect("steps block");
        assert_eq!(
            prop(item(steps, "StepA").unwrap(), "retries"),
            Some(&Value::number(5.0))
        );
        assert_eq!(
            prop(item(steps, "StepB").unwrap(), "retries"),
            Some(&Value::number(3.0))
        );
    }

    #[test]
    fn union_list_items_get_variant_defaults() {
        // `parallel [](step | []step)` — each item is a step OR a list of steps;
        // the variant is selected per item by body shape (A1), then defaulted (A2).
        let idx = index_from(
            "model step:\n    retries number = 3\n    name string?\n\nmodel flow:\n    parallel [](step | []step)\n",
        );
        let src = "flow F:\n    parallel:\n        - Scalar:\n            name = \"a\"\n        - Listy:\n            - Sub:\n                name = \"b\"\n";
        let out = apply_defaults(&idx, "flow", &body_of(src));
        let parallel = nested(&out, "parallel").expect("parallel block");

        // Scalar item → `step` variant → retries default injected.
        let scalar = item(parallel, "Scalar").expect("Scalar item");
        assert_eq!(prop(scalar, "retries"), Some(&Value::number(3.0)));

        // Listy item → `[]step` variant → each of its sub-items defaulted.
        let listy = item(parallel, "Listy").expect("Listy item");
        let sub = item(listy, "Sub").expect("Sub item under Listy");
        assert_eq!(prop(sub, "retries"), Some(&Value::number(3.0)));
    }

    #[test]
    fn oneof_root_defaults_selected_variant() {
        let idx = index_from(
            "model emailLog:\n    level string = \"info\"\n\nmodel emailPostmark:\n    serverToken string\n\noneof email by provider:\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n",
        );
        let out = apply_defaults(
            &idx,
            "email",
            &body_of("email E:\n    provider = \"log\"\n"),
        );
        assert_eq!(prop(&out, "provider"), Some(&Value::String("log".into())));
        assert_eq!(prop(&out, "level"), Some(&Value::String("info".into())));
    }

    #[test]
    fn oneof_default_discriminator_injected_when_absent() {
        // `oneof email by provider = "log"`: an instance omitting `provider` gets
        // the default tag injected, then the selected variant's fields defaulted.
        let idx = index_from(
            "model emailLog:\n    level string = \"info\"\n\nmodel emailPostmark:\n    serverToken string\n\noneof email by provider = \"log\":\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n",
        );
        let out = apply_defaults(&idx, "email", &body_of("email E:\n    unrelated = \"z\"\n"));
        assert_eq!(prop(&out, "provider"), Some(&Value::String("log".into())));
        assert_eq!(prop(&out, "level"), Some(&Value::String("info".into())));
    }

    #[test]
    fn oneof_present_discriminator_wins_over_default() {
        let idx = index_from(
            "model emailLog:\n    level string = \"info\"\n\nmodel emailPostmark:\n    serverToken string = \"t\"\n\noneof email by provider = \"log\":\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n",
        );
        // Authored `provider = "postmark"` must win over the default `"log"`.
        let out = apply_defaults(
            &idx,
            "email",
            &body_of("email E:\n    provider = \"postmark\"\n"),
        );
        assert_eq!(
            prop(&out, "provider"),
            Some(&Value::String("postmark".into()))
        );
        assert_eq!(prop(&out, "serverToken"), Some(&Value::String("t".into())));
        assert!(
            prop(&out, "level").is_none(),
            "log-variant field must not appear"
        );
    }

    #[test]
    fn oneof_present_non_string_discriminator_not_duplicated() {
        // A present-but-non-string discriminator is left for validation; the default
        // must NOT be injected — that would produce a duplicate `provider` property.
        let idx = index_from(
            "model emailLog:\n    level string = \"info\"\n\noneof email by provider = \"log\":\n    \"log\" -> emailLog\n",
        );
        let out = apply_defaults(&idx, "email", &body_of("email E:\n    provider = 5\n"));
        let provider_count = out
            .entries
            .iter()
            .filter(|e| matches!(&e.kind, BodyEntryKind::Property(p) if p.name.name == "provider"))
            .count();
        assert_eq!(
            provider_count, 1,
            "must not inject a duplicate discriminator"
        );
        assert!(
            prop(&out, "level").is_none(),
            "no variant defaulting for an invalid discriminator"
        );
    }

    #[test]
    fn nested_oneof_field_default_discriminator_injected() {
        // A `oneof` used as a *nested model field* (not the root) also gets its
        // default discriminator injected and variant defaulted — exercising the
        // recurse_nested → OneOf → oneof_body composition.
        let idx = index_from(
            "model emailLog:\n    level string = \"info\"\n\noneof email by provider = \"log\":\n    \"log\" -> emailLog\n\nmodel config:\n    notify email\n",
        );
        let out = apply_defaults(
            &idx,
            "config",
            &body_of("config C:\n    notify:\n        unrelated = \"x\"\n"),
        );
        let notify = nested(&out, "notify").expect("notify block");
        assert_eq!(prop(notify, "provider"), Some(&Value::String("log".into())));
        assert_eq!(prop(notify, "level"), Some(&Value::String("info".into())));
    }

    #[test]
    fn oneof_absent_discriminator_is_noop() {
        let idx = index_from(
            "model emailLog:\n    level string = \"info\"\n\noneof email by provider:\n    \"log\" -> emailLog\n",
        );
        let out = apply_defaults(&idx, "email", &body_of("email E:\n    unrelated = \"z\"\n"));
        assert!(
            prop(&out, "level").is_none(),
            "no variant chosen without a discriminator"
        );
    }

    #[test]
    fn unmodeled_root_is_noop() {
        let idx = index_from("model prompt:\n    x string = \"d\"\n");
        let body = body_of("widget W:\n    a = \"b\"\n");
        let out = apply_defaults(&idx, "widget", &body);
        assert_eq!(out.entries.len(), body.entries.len());
        assert_eq!(prop(&out, "a"), Some(&Value::String("b".into())));
    }

    #[test]
    fn object_field_is_passed_through() {
        let idx = index_from("model cfg:\n    meta object\n");
        let out = apply_defaults(
            &idx,
            "cfg",
            &body_of("cfg C:\n    meta:\n        anything = \"x\"\n"),
        );
        let meta = nested(&out, "meta").expect("meta preserved");
        assert_eq!(prop(meta, "anything"), Some(&Value::String("x".into())));
    }

    #[test]
    fn diamond_defaultable_schema_is_bounded() {
        // Each level has two required refs to the next, so a naive per-field
        // recursion / unbounded materialization would be exponential (2^depth).
        // The memoized defaultable set + materialization budget keep this
        // polynomial and finite: the test simply completing proves the bound.
        let depth = 40;
        let mut src = format!("model l{depth}:\n    v string = \"x\"\n\n");
        for i in (0..depth).rev() {
            src.push_str(&format!(
                "model l{i}:\n    a l{}\n    b l{}\n\n",
                i + 1,
                i + 1
            ));
        }
        let idx = index_from(&src);
        // l0 is fully defaultable; materializing an empty instance must terminate
        // and stay within the budget rather than emitting 2^40 blocks.
        let out = apply_defaults(
            &idx,
            "l0",
            &body_of("l0 X:\n    a:\n        v = \"keep\"\n"),
        );
        let nodes = count_nested(&out);
        assert!(
            nodes <= MAX_MATERIALIZED_MODELS as usize + 8,
            "materialization must stay within budget; synthesized {nodes} nested blocks"
        );
    }

    fn count_nested(body: &Body) -> usize {
        body.entries
            .iter()
            .map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) => 1 + count_nested(&nb.body),
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn self_referential_required_model_terminates() {
        // `child node` is required and self-referential, so `node` is not fully
        // defaultable: materialization must not be attempted and must not loop.
        let idx = index_from("model node:\n    name string = \"n\"\n    child node\n");
        let out = apply_defaults(&idx, "node", &body_of("node N:\n    name = \"x\"\n"));
        assert!(nested(&out, "child").is_none());
        assert_eq!(prop(&out, "name"), Some(&Value::String("x".into())));
    }

    #[test]
    fn secret_valued_default_is_resolved_last() {
        #[derive(serde::Deserialize)]
        struct Svc {
            #[serde(rename = "apiKey")]
            api_key: String,
            region: String,
        }
        let idx = index_from(
            "model svc:\n    apiKey string = $ENV.DEFAULT_KEY\n    region string = \"us\"\n",
        );
        let resolver =
            ValueResolver::new(|k| (k == "DEFAULT_KEY").then(|| "resolved-secret".to_string()));
        let svc: Svc = from_body_defaulted(
            &idx,
            "svc",
            &body_of("svc S:\n    region = \"eu\"\n"),
            &resolver,
        )
        .unwrap();
        assert_eq!(svc.api_key, "resolved-secret");
        assert_eq!(svc.region, "eu");
    }

    #[test]
    fn from_block_defaulted_uses_keyword_as_root() {
        #[derive(serde::Deserialize)]
        struct Prompt {
            #[serde(rename = "outputFormat")]
            output_format: String,
        }
        let idx = index_from("model prompt:\n    outputFormat string = \"text\"\n");
        let file = crate::cst::parse_to_ast("prompt P:\n    other = \"z\"\n").unwrap();
        let block = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => b,
            _ => panic!(),
        };
        let resolver = ValueResolver::new(|_| None);
        let p: Prompt = from_block_defaulted(&idx, block, &resolver).unwrap();
        assert_eq!(p.output_format, "text");
    }

    #[test]
    fn spec_example_default_is_applied() {
        // spec/models.md §"Field Presence Rules": a field with `= value` may be
        // omitted and the default is used. Before this pass, omitting
        // `sessionDuration` failed (missing required field); now it is injected.
        #[derive(serde::Deserialize)]
        struct WebProfile {
            #[serde(rename = "siteName")]
            site_name: String,
            #[serde(rename = "sessionDuration")]
            session_duration: String,
        }
        let idx = index_from(
            "model webProfile:\n    siteName string\n    debug string?\n    sessionDuration duration = \"24h\"\n",
        );
        let resolver = ValueResolver::new(|_| None);
        let wp: WebProfile = from_body_defaulted(
            &idx,
            "webProfile",
            &body_of("webProfile P:\n    siteName = \"acme\"\n"),
            &resolver,
        )
        .unwrap();
        assert_eq!(wp.site_name, "acme");
        assert_eq!(wp.session_duration, "24h");
    }

    #[test]
    fn overridden_shared_secret_is_not_resolved() {
        // Resolve-last laziness: a `.key` shared secret that every item overrides
        // is dropped by the shared-merge before resolution, so it never fails —
        // whereas resolving the raw body (resolve-first) would error.
        let body = body_of(
            "workflow W:\n    .token = $ENV.MISSING\n    - StepA:\n        token = \"explicit\"\n",
        );
        let resolver = ValueResolver::new(|_| None);
        assert!(
            resolver.resolve_body(&body).is_err(),
            "resolve-first fails on the shared secret"
        );
        let shared = apply_shared_properties(&body);
        assert!(
            resolver.resolve_body(&shared).is_ok(),
            "resolve-last: overridden shared secret is never resolved"
        );
    }
}
