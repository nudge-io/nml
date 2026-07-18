use crate::span::Span;
use crate::types::{PrimitiveType, SpannedValue};
use serde::Serialize;

/// A model definition parsed from `model name:` or `model name is parent:`.
#[derive(Debug, Clone, Serialize)]
pub struct ModelDef {
    pub name: String,
    pub extends: Vec<String>,
    pub fields: Vec<FieldDef>,
    pub span: Span,
}

/// An enum definition parsed from `enum name:`.
#[derive(Debug, Clone, Serialize)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<String>,
    pub span: Span,
}

/// A discriminated-union definition extracted from `oneof Name by <field>:`.
///
/// Selects one of several variant models by the value of a discriminator
/// field. Validation dispatches an instance to the variant model named by the
/// discriminator's value.
#[derive(Debug, Clone, Serialize)]
pub struct OneOfDef {
    pub name: String,
    /// Field whose value selects the variant.
    pub discriminator: String,
    /// Optional enum type for the discriminator. When present, the arm keys must
    /// exactly cover the enum's variants (enforced at schema load).
    pub discriminator_type: Option<String>,
    /// Default discriminator value, injected when an instance omits it. Always one
    /// of the `variants`' keys (enforced at schema load).
    pub default_discriminator: Option<String>,
    /// `(discriminator_value, variant_model_name)` pairs, in source order.
    pub variants: Vec<(String, String)>,
    pub span: Span,
}

/// A field definition within a model.
#[derive(Debug, Clone, Serialize)]
pub struct FieldDef {
    pub name: String,
    pub field_type: FieldType,
    pub optional: bool,
    /// The model's positional/scalar-shorthand field (`name type+`, RFC 0005
    /// §16): the one field a bare scalar list item fills. At most one per model
    /// (enforced at schema load).
    pub shorthand: bool,
    /// The declared default, retaining its parsed type and source span. `None`
    /// when the field has no `= value`. The span points at the default literal so
    /// type-check diagnostics can locate it precisely.
    pub default_value: Option<SpannedValue>,
    /// Trailing `#name`/`#name(value)` directives (RFC 0032), source order.
    /// Opaque metadata — consumers interpret (see [`crate::types::Directive`]).
    pub directives: Vec<crate::types::Directive>,
    /// The leading own-line comment block documenting the field (RFC 0004 §4.3
    /// comment attachment), `//` markers stripped, lines joined. Presentation
    /// metadata for tooling (hover, completion) ONLY: it must never influence
    /// validation or RFC 0032 reload/diff semantics — `FieldDef` deliberately
    /// derives no `PartialEq`, and semantic comparison happens on `Value`s
    /// (`Value::semantic_eq`), never on `FieldDef`s wholesale.
    pub doc: Option<String>,
    pub span: Span,
}

/// The type of a field.
#[derive(Debug, Clone, Serialize)]
pub enum FieldType {
    Primitive(PrimitiveType),
    List(Box<FieldType>),
    ModelRef(String),
    /// A typed modifier field (`|allow []string?`); the inner type is the
    /// declared type of the modifier's value.
    Modifier(Box<FieldType>),
    Union(Vec<FieldType>),
    /// `(K -> V)` — a typed arm set (RFC 0007): the field's body is ordered,
    /// first-match `(@selector | else) -> Target` arms. `key` types the
    /// selectors (`role`, `string`, or an enum; `else` is always legal);
    /// `target` types the arm targets — completion/intent for reference
    /// targets (consumer-resolved, never existence-checked; RFC 0007 §4.1),
    /// full validation for inline-block targets.
    Arms {
        key: Box<FieldType>,
        target: Box<FieldType>,
    },
    /// `set<T>` — an unordered, **unique**-element collection (RFC 0032).
    /// Duplicate elements are a load-time validation error (value-level
    /// identity: for a union element type, the admitting arm is irrelevant).
    /// Unlike `List`, element order never carries meaning — diffs are
    /// order-insensitive (`SetDelta`), and authored order is preserved in
    /// source but semantically inert.
    Set(Box<FieldType>),
}

/// Renders the type in NML source syntax: `[]string`, `(step | []step)`,
/// `[](string | number)`.
///
/// A modifier's type displays as its declared inner type -- the `|` sigil
/// belongs to the field *name* (`|allow []string`), not the type.
impl std::fmt::Display for FieldType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldType::Primitive(p) => f.write_str(p.as_str()),
            FieldType::List(inner) => write!(f, "[]{inner}"),
            FieldType::ModelRef(name) => f.write_str(name),
            FieldType::Modifier(inner) => write!(f, "{inner}"),
            FieldType::Union(variants) => {
                f.write_str("(")?;
                for (i, v) in variants.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" | ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str(")")
            }
            FieldType::Arms { key, target } => write!(f, "({key} -> {target})"),
            FieldType::Set(inner) => {
                // Canonical form: bare union inside the angles (`set<a | b>`)
                // — the angles already bound it, so the union's grouping
                // parens would be redundant (RFC 0032 Decision 4).
                f.write_str("set<")?;
                match inner.as_ref() {
                    FieldType::Union(variants) => {
                        for (i, v) in variants.iter().enumerate() {
                            if i > 0 {
                                f.write_str(" | ")?;
                            }
                            write!(f, "{v}")?;
                        }
                    }
                    other => write!(f, "{other}")?,
                }
                f.write_str(">")
            }
        }
    }
}
