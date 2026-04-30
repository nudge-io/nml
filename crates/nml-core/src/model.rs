use crate::span::Span;
use crate::types::PrimitiveType;
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

/// A field definition within a model.
#[derive(Debug, Clone, Serialize)]
pub struct FieldDef {
    pub name: String,
    pub field_type: FieldType,
    pub optional: bool,
    pub default_value: Option<String>,
    pub span: Span,
}

/// The type of a field.
#[derive(Debug, Clone, Serialize)]
pub enum FieldType {
    Primitive(PrimitiveType),
    List(Box<FieldType>),
    ModelRef(String),
    Modifier(String),
    Union(Vec<FieldType>),
}
