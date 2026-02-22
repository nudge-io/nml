use crate::money::Money;
use crate::span::Span;
use serde::Serialize;

/// A parsed value in NML.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Value {
    String(String),
    Number(f64),
    Money(Money),
    Bool(bool),
    Duration(String),
    Path(String),
    Secret(String),
    RoleRef(String),
    Reference(String),
    Array(Vec<SpannedValue>),
}

/// A value with its source location.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpannedValue {
    pub value: Value,
    pub span: Span,
}

impl SpannedValue {
    pub fn new(value: Value, span: Span) -> Self {
        Self { value, span }
    }
}

/// The primitive type names recognized in model definitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PrimitiveType {
    String,
    Number,
    Money,
    Bool,
    Duration,
    Path,
    Secret,
}

impl PrimitiveType {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "string" => Some(PrimitiveType::String),
            "number" => Some(PrimitiveType::Number),
            "money" => Some(PrimitiveType::Money),
            "bool" => Some(PrimitiveType::Bool),
            "duration" => Some(PrimitiveType::Duration),
            "path" => Some(PrimitiveType::Path),
            "secret" => Some(PrimitiveType::Secret),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            PrimitiveType::String => "string",
            PrimitiveType::Number => "number",
            PrimitiveType::Money => "money",
            PrimitiveType::Bool => "bool",
            PrimitiveType::Duration => "duration",
            PrimitiveType::Path => "path",
            PrimitiveType::Secret => "secret",
        }
    }
}
