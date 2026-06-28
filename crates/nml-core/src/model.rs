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
    /// The model's scalar-shorthand field (`name type!`): the one field a bare
    /// scalar list item fills. At most one per model (enforced at schema load).
    pub shorthand: bool,
    /// The declared default, retaining its parsed type and source span. `None`
    /// when the field has no `= value`. The span points at the default literal so
    /// type-check diagnostics can locate it precisely.
    pub default_value: Option<SpannedValue>,
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
        }
    }
}
