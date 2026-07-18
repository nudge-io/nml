//! Identity materialization for named declarations (RFC 0005 §5/§10).
//!
//! A declaration's *identity token* fills a declared field:
//!   - an **ident** name — a list item `- editor:` *or* a block `role editor:` →
//!     the model's `name` field ([`materialize_named`]);
//!   - a **scalar** list key (`- "/api"`) → the model's shorthand (`!`) field
//!     ([`materialize_item`]).
//!
//! These are the single definition of that rule. They are traversal-free — the caller
//! supplies the already-resolved element model — so the validator (which validates the
//! enriched body) and the deserialize pipeline (which deserializes it) share one
//! implementation and agree by construction.
//!
//! Injection is **lenient**: an explicit value in the body wins over the identity
//! token (matching `de`'s `NamedItemDeserializer`), so the token is a default the
//! author may override without ceremony and the validator stays in agreement with the
//! deserializer. The one diagnostic it returns is **dropped-key** — a *scalar* whose
//! model declares no shorthand field (a genuine loss). An *ident* whose model declares
//! no `name` is **not** a drop: it is the `de` runtime fallback, so it is silent.

use crate::ast::{
    Arm, ArmSelector, ArmTarget, Body, BodyEntry, BodyEntryKind, Identifier, ListItem,
    ListItemKind, NestedBlock, Property,
};
use crate::error::NmlError;
use crate::model::{FieldType, ModelDef, OneOfDef};
use crate::schema_index::{FieldTarget, SchemaIndex};
use crate::span::Span;
use crate::types::{SpannedValue, Value};

/// The conventional field a *named* declaration's identity fills.
const NAME_FIELD: &str = "name";

/// Seed for a bodyless scalar item (`- "/api"`): the shorthand value is injected into
/// this empty body, producing a one-property instance.
const EMPTY_BODY: Body = Body {
    entries: Vec::new(),
};

/// Bounds recursion into nested structure, mirroring the defaulter's
/// `MAX_DEFAULT_DEPTH`. The pass runs on untrusted instance bodies.
const MAX_POSITIONAL_DEPTH: u32 = 64;

/// Result of [`materialize_item`] / [`materialize_named`]: the enriched body, any
/// diagnostics, and whether the body is a usable instance to validate.
pub struct Materialized {
    /// The item's body with its identity injected (best-effort).
    pub body: Body,
    /// Diagnostics produced while materializing. The only one is **dropped-key** (a
    /// scalar whose model declares no shorthand field). Injection itself never errors
    /// — an explicit value simply wins (see [`inject`]).
    pub diagnostics: Vec<NmlError>,
    /// `false` when the item could not be placed — a scalar with no shorthand field,
    /// or a reference/link. Callers surface `diagnostics` but must **not** run
    /// instance validation on `body` (an empty body would add noise — e.g. spurious
    /// "missing required field" errors on top of the dropped-key diagnostic).
    pub validatable: bool,
}

/// Materialize a **named declaration's** identity into its body: inject `name` from
/// the declaration's name, if the model declares a `name` field. A model with no
/// `name` field is the runtime-fallback case (`NamedItemDeserializer`): not injected.
/// An explicit `name` in the body wins (lenient — see [`inject`]). Shared by list-item
/// named keys (`- editor:`) and block declarations (`role editor:`).
pub fn materialize_named(name: &Identifier, body: &Body, model: &ModelDef) -> Materialized {
    let body = if model.fields.iter().any(|f| f.name == NAME_FIELD) {
        inject(
            body,
            NAME_FIELD,
            SpannedValue::new(Value::String(name.name.clone()), name.span),
        )
    } else {
        body.clone()
    };
    Materialized {
        body,
        diagnostics: Vec::new(),
        validatable: true,
    }
}

/// Materialize `item`'s identity into its body against `model`. See the module docs.
pub fn materialize_item(item: &ListItem, model: &ModelDef) -> Materialized {
    match &item.kind {
        ListItemKind::Named { name, body } => materialize_named(name, body, model),
        // Scalar key → the shorthand (`!`) field, injected into the item's body (the
        // optional `- "/api": <body>` form) or a fresh one. No `!` field ⇒ dropped key,
        // and the item has no placement, so it is not validatable.
        ListItemKind::Shorthand { value, body } => {
            match model.fields.iter().find(|f| f.shorthand) {
                // A bare arm-set shorthand field (RFC 0007 §4.3 ⑤): the scalar
                // fills it via the canonical embedding `s ⇒ [else -> s]` — a
                // one-arm block whose `else` target mirrors the scalar's form,
                // exactly as the arm block itself distinguishes `-> Foo` from
                // `-> "foo"`. A scalar with no name/string form (e.g. a number)
                // is surfaced as loudly as the plain-scalar path's downstream
                // type error would be — never a silent empty target.
                Some(field) if matches!(field.field_type, FieldType::Arms { .. }) => {
                    match arm_fill_target(value) {
                        Some(target) => Materialized {
                            body: inject_arm(
                                body.as_ref().unwrap_or(&EMPTY_BODY),
                                &field.name,
                                target,
                                value.span,
                            ),
                            diagnostics: Vec::new(),
                            validatable: true,
                        },
                        None => Materialized {
                            body: Body {
                                entries: Vec::new(),
                            },
                            diagnostics: vec![error(
                                format!(
                                    "a {} cannot fill the arm-set shorthand field '{}' on model \
                                 '{}' (an arm target is a name or a string)",
                                    value.value.type_name(),
                                    field.name,
                                    model.name
                                ),
                                value.span,
                            )],
                            validatable: false,
                        },
                    }
                }
                Some(field) => Materialized {
                    body: inject(
                        body.as_ref().unwrap_or(&EMPTY_BODY),
                        &field.name,
                        value.clone(),
                    ),
                    diagnostics: Vec::new(),
                    validatable: true,
                },
                None => Materialized {
                    body: Body {
                        entries: Vec::new(),
                    },
                    diagnostics: vec![error(
                        format!(
                            "the value has no shorthand field on model '{}' and would be dropped",
                            model.name
                        ),
                        value.span,
                    )],
                    validatable: false,
                },
            }
        }
        // Links — never materialized, never validated as inline instances.
        ListItemKind::Reference(_) | ListItemKind::Role(_) => Materialized {
            body: Body {
                entries: Vec::new(),
            },
            diagnostics: Vec::new(),
            validatable: false,
        },
    }
}

/// Append `field = value` to a clone of `body` — **unless `body` already sets
/// `field`**, in which case the explicit value wins and the body is returned
/// unchanged. This is **lenient by design**: the identity token (a named key, block
/// name, or scalar) is a *default* the author may override without ceremony, and it
/// keeps the validator in agreement with `de` (`NamedItemDeserializer` already
/// prefers an explicit `name` via `has_explicit_name`). The injected property carries
/// the token's source span so a downstream type error points at the item.
fn inject(body: &Body, field: &str, value: SpannedValue) -> Body {
    if body
        .entries
        .iter()
        .any(|e| matches!(&e.kind, BodyEntryKind::Property(p) if p.name.name == field))
    {
        return body.clone(); // explicit value wins
    }
    let span = value.span;
    let mut entries = body.entries.clone();
    entries.push(BodyEntry {
        span,
        kind: BodyEntryKind::Property(Property {
            name: Identifier::new(field.to_string(), span),
            value,
        }),
    });
    Body { entries }
}

/// The `else`-arm target a scalar fill produces (RFC 0007 §4.3 ⑤), mirroring
/// the arm grammar's own name-vs-string split. NOTE: in *parsed* source a
/// bare `- Name` list item is a Reference **link** (RFC 0005) and a `- @x`
/// item is a Role link — neither reaches the shorthand fill — so the
/// `Reference` arm here serves programmatically built bodies (`identity` is a
/// public API over arbitrary `Body`s); parsed fills always produce literals.
/// `None` when the scalar has no name/string form (a number, a bool…) — the
/// caller turns that into a loud diagnostic, never a lossy default.
fn arm_fill_target(value: &SpannedValue) -> Option<ArmTarget> {
    match &value.value {
        Value::Reference(name) => Some(ArmTarget::Reference(Identifier::new(
            name.clone(),
            value.span,
        ))),
        other => String::try_from(other).ok().map(|s| ArmTarget::Literal {
            value: s,
            span: value.span,
        }),
    }
}

/// Fill a bare arm-set shorthand field with the canonical `[else -> s]`
/// embedding (RFC 0007 §4.3 ⑤): synthesize `field: { else -> <target> }`
/// unless the body already declares `field` **in any form** (an explicit arm
/// block wins; an explicit property is left alone too — the validator flags
/// it against the arms type, and injecting alongside it would only double the
/// noise). The same leniency as [`inject`].
fn inject_arm(body: &Body, field: &str, target: ArmTarget, span: Span) -> Body {
    let already_set = body.entries.iter().any(|e| match &e.kind {
        BodyEntryKind::NestedBlock(nb) => nb.name.name == field,
        BodyEntryKind::Property(p) => p.name.name == field,
        _ => false,
    });
    if already_set {
        return body.clone(); // explicit declaration wins
    }
    let arm = BodyEntry {
        span,
        kind: BodyEntryKind::Arm(Arm {
            selector: ArmSelector::Else,
            selector_span: span,
            target,
        }),
    };
    let block = BodyEntry {
        span,
        kind: BodyEntryKind::NestedBlock(NestedBlock {
            name: Identifier::new(field.to_string(), span),
            body: Body { entries: vec![arm] },
        }),
    };
    let mut entries = body.entries.clone();
    entries.push(block);
    Body { entries }
}

fn error(message: String, span: Span) -> NmlError {
    NmlError::Validation { message, span }
}

// ---------------------------------------------------------------------------
// The materialization pass (RFC 0005 §10) — runs before deserialization.
// ---------------------------------------------------------------------------

/// Materialize every **scalar** list item into a body so the schema-blind `de` can
/// deserialize it as a struct. Walks `body` against the model `root`, and for each
/// list field whose element model declares a shorthand (`!`) field, rewrites each
/// `Shorthand` item to carry a body with the scalar injected into that field
/// (reusing [`materialize_item`]). Named items and bodyless scalars whose element has
/// no `!` field are left unchanged — `de` handles them (`NamedItemDeserializer` for
/// names, the bare value otherwise). The pass recurses into nested bodies so nested
/// lists are materialized too.
///
/// Run **first** in the pipeline (`apply_positional → apply_shared_properties →
/// apply_defaults → resolve`) so an item's own scalar token beats a list-wide shared
/// property: materializing first makes the field present, and the lenient shared-merge
/// then yields to it.
pub fn apply_positional(index: &SchemaIndex, root: &str, body: &Body) -> Body {
    match index.model(root) {
        Some(model) => Positionalizer { index }.model_body(model, body, 0),
        // A non-model root carries no list-of-`!`-model fields to materialize here.
        None => body.clone(),
    }
}

struct Positionalizer<'a> {
    index: &'a SchemaIndex,
}

impl Positionalizer<'_> {
    fn model_body(&self, model: &ModelDef, body: &Body, depth: u32) -> Body {
        if depth >= MAX_POSITIONAL_DEPTH {
            return body.clone();
        }
        let entries = body
            .entries
            .iter()
            .map(|entry| match &entry.kind {
                // A field written as a nested block: a list field materializes its
                // items; a model field recurses its body.
                BodyEntryKind::NestedBlock(nb) => BodyEntry {
                    span: entry.span,
                    kind: BodyEntryKind::NestedBlock(NestedBlock {
                        name: nb.name.clone(),
                        body: self.recurse_field(model, &nb.name.name, &nb.body, depth),
                    }),
                },
                _ => entry.clone(),
            })
            .collect();
        Body { entries }
    }

    fn recurse_field(&self, model: &ModelDef, field_name: &str, body: &Body, depth: u32) -> Body {
        let Some(field) = model.fields.iter().find(|f| f.name == field_name) else {
            return body.clone();
        };
        if let FieldType::List(inner) = &field.field_type {
            return self.list_body(inner, body, depth + 1);
        }
        match self.index.resolve_field(field) {
            FieldTarget::Model(m) => self.model_body(m, body, depth + 1),
            FieldTarget::OneOf(o) => self.oneof_body(o, body, depth + 1),
            _ => body.clone(),
        }
    }

    /// Recurse into a `oneof` instance's selected variant so nested shorthand lists
    /// materialize. The variant is resolved from the authored discriminator, else the
    /// union's default arm — mirroring the defaulter's `oneof_body` (the discriminator
    /// itself is injected later, by `apply_defaults`). An unresolvable discriminator
    /// leaves the body unchanged.
    fn oneof_body(&self, oneof: &OneOfDef, body: &Body, depth: u32) -> Body {
        let authored = body.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::Property(p) if p.name.name == oneof.discriminator => {
                p.value.value.as_str()
            }
            _ => None,
        });
        let Some(disc) = authored.or(oneof.default_discriminator.as_deref()) else {
            return body.clone();
        };
        match oneof
            .variants
            .iter()
            .find(|(v, _)| v.as_str() == disc)
            .and_then(|(_, m)| self.index.model(m))
        {
            Some(variant) => self.model_body(variant, body, depth),
            None => body.clone(),
        }
    }

    fn list_body(&self, inner: &FieldType, body: &Body, depth: u32) -> Body {
        let entries = body
            .entries
            .iter()
            .map(|entry| match &entry.kind {
                BodyEntryKind::ListItem(item) => BodyEntry {
                    span: entry.span,
                    kind: BodyEntryKind::ListItem(self.list_item(inner, item, depth)),
                },
                _ => entry.clone(),
            })
            .collect();
        Body { entries }
    }

    fn list_item(&self, inner: &FieldType, item: &ListItem, depth: u32) -> ListItem {
        // Resolve the element model. For a union list this is body-dependent; a
        // bodyless scalar can't select a variant, so it falls through unchanged
        // (scalar-on-union is out of scope, flagged by the validator — §10).
        let empty = Body {
            entries: Vec::new(),
        };
        let probe = item_body(item).unwrap_or(&empty);
        let FieldTarget::Model(m) = self.index.resolve_type_in_body(inner, probe) else {
            return item.clone();
        };
        match &item.kind {
            // Scalar with a shorthand target: inject the value (via the shared
            // primitive), then recurse the materialized body. `validatable == false`
            // means no `!` field (dropped key) — leave the bare value for `de`.
            ListItemKind::Shorthand { value, .. } => {
                let materialized = materialize_item(item, m);
                if materialized.validatable {
                    ListItem {
                        span: item.span,
                        kind: ListItemKind::Shorthand {
                            value: value.clone(),
                            body: Some(self.model_body(m, &materialized.body, depth + 1)),
                        },
                    }
                } else {
                    item.clone()
                }
            }
            // Named items keep their `de`-side name injection; just recurse the body.
            ListItemKind::Named { name, body } => ListItem {
                span: item.span,
                kind: ListItemKind::Named {
                    name: name.clone(),
                    body: self.model_body(m, body, depth + 1),
                },
            },
            // References/links — never materialized.
            _ => item.clone(),
        }
    }
}

/// The body an inline item carries, if any (used to probe a union element's variant).
fn item_body(item: &ListItem) -> Option<&Body> {
    match &item.kind {
        ListItemKind::Named { body, .. } => Some(body),
        ListItemKind::Shorthand { body, .. } => body.as_ref(),
        ListItemKind::Reference(_) | ListItemKind::Role(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FieldDef, FieldType};
    use crate::types::PrimitiveType;

    fn s() -> Span {
        Span::empty(0)
    }

    fn fd(name: &str, shorthand: bool) -> FieldDef {
        FieldDef {
            name: name.to_string(),
            field_type: FieldType::Primitive(PrimitiveType::String),
            optional: false,
            shorthand,
            default_value: None,
            directives: Vec::new(),
            doc: None,
            span: s(),
        }
    }

    fn model(fields: Vec<FieldDef>) -> ModelDef {
        ModelDef {
            name: "m".into(),
            extends: vec![],
            fields,
            span: s(),
        }
    }

    fn named(name: &str, body: Body) -> ListItem {
        ListItem {
            span: s(),
            kind: ListItemKind::Named {
                name: Identifier::new(name, s()),
                body,
            },
        }
    }

    fn prop(name: &str, value: &str) -> BodyEntry {
        BodyEntry {
            span: s(),
            kind: BodyEntryKind::Property(Property {
                name: Identifier::new(name, s()),
                value: SpannedValue::new(Value::String(value.into()), s()),
            }),
        }
    }

    fn name_value(body: &Body) -> Option<&str> {
        body.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::Property(p) if p.name.name == "name" => match &p.value.value {
                Value::String(s) => Some(s.as_str()),
                _ => None,
            },
            _ => None,
        })
    }

    #[test]
    fn named_item_injects_name_when_declared() {
        let m = model(vec![fd("name", false), fd("description", false)]);
        let r = materialize_item(&named("editor", Body { entries: vec![] }), &m);
        assert!(r.diagnostics.is_empty() && r.validatable);
        assert_eq!(name_value(&r.body), Some("editor"));
    }

    #[test]
    fn named_item_without_name_field_is_untouched_no_diag() {
        // The runtime-fallback case (e.g. `model step`): not injected, not flagged,
        // but the authored body is still validatable.
        let m = model(vec![fd("run", false)]);
        let r = materialize_item(&named("classify", Body { entries: vec![] }), &m);
        assert!(r.diagnostics.is_empty() && r.validatable);
        assert_eq!(name_value(&r.body), None);
    }

    #[test]
    fn explicit_name_wins_over_key() {
        // Lenient: an explicit `name` overrides the key — no diagnostic, explicit value
        // retained (matching `de`'s `has_explicit_name`).
        let m = model(vec![fd("name", false)]);
        let item = named(
            "editor",
            Body {
                entries: vec![prop("name", "other")],
            },
        );
        let r = materialize_item(&item, &m);
        assert!(r.diagnostics.is_empty() && r.validatable);
        assert_eq!(name_value(&r.body), Some("other"));
    }

    #[test]
    fn scalar_fills_shorthand_field() {
        let m = model(vec![fd("name", false), fd("path", true)]);
        let item = ListItem {
            span: s(),
            kind: ListItemKind::Shorthand {
                value: SpannedValue::new(Value::String("/api".into()), s()),
                body: None,
            },
        };
        let r = materialize_item(&item, &m);
        assert!(r.diagnostics.is_empty() && r.validatable);
        let path = r.body.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::Property(p) if p.name.name == "path" => Some(&p.value.value),
            _ => None,
        });
        assert!(matches!(path, Some(Value::String(s)) if s == "/api"));
    }

    #[test]
    fn scalar_fills_arm_set_shorthand_as_an_else_arm() {
        // RFC 0007 §4.3 ⑤: a bare arm-set shorthand field is filled by the
        // canonical `s ⇒ [else -> s]` embedding — a quoted scalar → a literal
        // target, a bare name → a reference target, each mirroring the arm
        // grammar's own name-vs-string distinction.
        let mut arm_field = fd("dispatch", true);
        arm_field.field_type = FieldType::Arms {
            key: Box::new(FieldType::Primitive(PrimitiveType::Role)),
            target: Box::new(FieldType::Primitive(PrimitiveType::Path)),
        };
        let m = model(vec![fd("name", false), arm_field]);

        let arm_of = |body: &Body| -> Arm {
            let BodyEntryKind::NestedBlock(nb) = &body
                .entries
                .iter()
                .find(|e| matches!(&e.kind, BodyEntryKind::NestedBlock(nb) if nb.name.name == "dispatch"))
                .expect("dispatch block synthesized")
                .kind
            else {
                unreachable!()
            };
            match &nb.body.entries[0].kind {
                BodyEntryKind::Arm(a) => a.clone(),
                _ => panic!("expected an arm"),
            }
        };

        // Quoted scalar → `else -> "x.workflow.nml"` (literal).
        let lit_item = ListItem {
            span: s(),
            kind: ListItemKind::Shorthand {
                value: SpannedValue::new(Value::String("x.workflow.nml".into()), s()),
                body: None,
            },
        };
        let r = materialize_item(&lit_item, &m);
        assert!(r.diagnostics.is_empty() && r.validatable);
        let arm = arm_of(&r.body);
        assert!(matches!(arm.selector, ArmSelector::Else));
        assert!(
            matches!(arm.target, ArmTarget::Literal { value, .. } if value == "x.workflow.nml")
        );

        // Bare name → `else -> Fallback` (reference).
        let ref_item = ListItem {
            span: s(),
            kind: ListItemKind::Shorthand {
                value: SpannedValue::new(Value::Reference("Fallback".into()), s()),
                body: None,
            },
        };
        let arm = arm_of(&materialize_item(&ref_item, &m).body);
        assert!(matches!(arm.target, ArmTarget::Reference(id) if id.name == "Fallback"));

        // Explicit arm block wins (leniency).
        let explicit = ListItem {
            span: s(),
            kind: ListItemKind::Shorthand {
                value: SpannedValue::new(Value::String("ignored".into()), s()),
                body: Some(Body {
                    entries: vec![BodyEntry {
                        span: s(),
                        kind: BodyEntryKind::NestedBlock(NestedBlock {
                            name: Identifier::new("dispatch", s()),
                            body: Body {
                                entries: vec![BodyEntry {
                                    span: s(),
                                    kind: BodyEntryKind::Arm(Arm {
                                        selector: ArmSelector::Else,
                                        selector_span: s(),
                                        target: ArmTarget::Literal {
                                            value: "kept.workflow.nml".into(),
                                            span: s(),
                                        },
                                    }),
                                }],
                            },
                        }),
                    }],
                }),
            },
        };
        let arm = arm_of(&materialize_item(&explicit, &m).body);
        assert!(
            matches!(arm.target, ArmTarget::Literal { value, .. } if value == "kept.workflow.nml"),
            "explicit arm block wins over the scalar fill"
        );

        // A scalar with no name/string form (`- 42`) is a LOUD diagnostic,
        // never a silent empty target — matching the plain-scalar path's
        // downstream type-error loudness.
        let numeric = ListItem {
            span: s(),
            kind: ListItemKind::Shorthand {
                value: SpannedValue::new(Value::number(42), s()),
                body: None,
            },
        };
        let r = materialize_item(&numeric, &m);
        assert!(!r.validatable);
        assert_eq!(r.diagnostics.len(), 1);
        let NmlError::Validation { message, .. } = &r.diagnostics[0] else {
            panic!()
        };
        assert!(
            message.contains("cannot fill the arm-set shorthand"),
            "{message}"
        );

        // An explicit PROPERTY named like the field also suppresses the fill
        // (the validator flags it against the arms type; no doubled noise).
        let with_prop = ListItem {
            span: s(),
            kind: ListItemKind::Shorthand {
                value: SpannedValue::new(Value::String("ignored".into()), s()),
                body: Some(Body {
                    entries: vec![prop("dispatch", "explicit")],
                }),
            },
        };
        let r = materialize_item(&with_prop, &m);
        assert!(
            !r.body
                .entries
                .iter()
                .any(|e| matches!(&e.kind, BodyEntryKind::NestedBlock(_))),
            "no arm block synthesized beside an explicit property"
        );
    }

    #[test]
    fn scalar_without_shorthand_field_is_dropped_key() {
        let m = model(vec![fd("name", false)]);
        let item = ListItem {
            span: s(),
            kind: ListItemKind::Shorthand {
                value: SpannedValue::new(Value::String("/api".into()), s()),
                body: None,
            },
        };
        let r = materialize_item(&item, &m);
        // Dropped key → flagged AND not validatable (so the caller won't add noise).
        assert!(!r.validatable);
        assert_eq!(r.diagnostics.len(), 1);
        let NmlError::Validation { message, .. } = &r.diagnostics[0] else {
            panic!()
        };
        assert!(message.contains("no shorthand field"), "{message}");
    }
}
