use crate::span::Span;
use crate::types::PrimitiveType;
use serde::Serialize;

/// A model definition parsed from `model name:`.
#[derive(Debug, Clone, Serialize)]
pub struct ModelDef {
    pub name: String,
    pub traits: Vec<String>,
    pub fields: Vec<FieldDef>,
    pub span: Span,
}

/// A trait definition parsed from `trait name:`.
#[derive(Debug, Clone, Serialize)]
pub struct TraitDef {
    pub name: String,
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

/// A field definition within a model or trait.
#[derive(Debug, Clone, Serialize)]
pub struct FieldDef {
    pub name: String,
    pub field_type: FieldType,
    pub optional: bool,
    pub default_value: Option<String>,
    pub constraints: Vec<Constraint>,
    pub span: Span,
}

/// The type of a field.
#[derive(Debug, Clone, Serialize)]
pub enum FieldType {
    Primitive(PrimitiveType),
    List(Box<FieldType>),
    RefOnly(Box<FieldType>),
    RoleRef,
    ModelRef(String),
    Modifier(String),
    InlineObject(Vec<FieldDef>),
    SharedProperty(Vec<FieldDef>),
}

/// A constraint on a field.
#[derive(Debug, Clone, Serialize)]
pub enum Constraint {
    Unique,
    Secret,
    Token,
    Distinct,
    Shorthand,
    Integer,
    Min(f64),
    Max(f64),
    MinLength(usize),
    MaxLength(usize),
    Pattern(String),
    Currency(Vec<String>),
}

impl Constraint {
    pub fn name(&self) -> &str {
        match self {
            Constraint::Unique => "unique",
            Constraint::Secret => "secret",
            Constraint::Token => "token",
            Constraint::Distinct => "distinct",
            Constraint::Shorthand => "shorthand",
            Constraint::Integer => "integer",
            Constraint::Min(_) => "min",
            Constraint::Max(_) => "max",
            Constraint::MinLength(_) => "minLength",
            Constraint::MaxLength(_) => "maxLength",
            Constraint::Pattern(_) => "pattern",
            Constraint::Currency(_) => "currency",
        }
    }
}
