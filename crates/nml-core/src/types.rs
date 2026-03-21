use crate::money::Money;
use crate::span::Span;
use serde::Serialize;

/// A segment of a template string: either literal text or a `{{...}}` expression.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum TemplateSegment {
    Literal(String),
    Expression {
        namespace: String,
        path: Vec<String>,
        raw: String,
        span: Span,
    },
}

/// A parsed value in NML.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Value {
    String(String),
    TemplateString(Vec<TemplateSegment>),
    Number(f64),
    Money(Money),
    Bool(bool),
    Duration(String),
    Path(String),
    Secret(String),
    Role(String),
    Reference(String),
    Array(Vec<SpannedValue>),
    Fallback(Box<SpannedValue>, Box<SpannedValue>),
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
    /// Flexible key-value nested block; accepts any keys with scalar values.
    Object,
    /// Role reference type; values use `@keyword/name` syntax.
    Role,
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
            "object" => Some(PrimitiveType::Object),
            "role" => Some(PrimitiveType::Role),
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
            PrimitiveType::Object => "object",
            PrimitiveType::Role => "role",
        }
    }
}

/// Error returned when a `Value` cannot be converted to the requested type.
#[derive(Debug, Clone, thiserror::Error)]
#[error("expected {expected}, got {actual}")]
pub struct ValueTypeError {
    pub expected: &'static str,
    pub actual: &'static str,
}

impl Value {
    /// Returns a human-readable name for the value's variant.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::String(_) | Value::TemplateString(_) => "string",
            Value::Number(_) => "number",
            Value::Money(_) => "money",
            Value::Bool(_) => "bool",
            Value::Duration(_) => "duration",
            Value::Path(_) => "path",
            Value::Secret(_) => "secret",
            Value::Role(_) => "role",
            Value::Reference(_) => "reference",
            Value::Array(_) => "array",
            Value::Fallback(_, _) => "fallback",
        }
    }

    /// Extract as a string slice (String, Path, Duration, Secret, Reference, Role).
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s)
            | Value::Path(s)
            | Value::Duration(s)
            | Value::Secret(s)
            | Value::Reference(s)
            | Value::Role(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Extract as a number.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Extract as a boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Extract as an array of spanned values.
    pub fn as_array(&self) -> Option<&[SpannedValue]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }
}

impl TryFrom<&Value> for String {
    type Error = ValueTypeError;

    fn try_from(value: &Value) -> Result<String, Self::Error> {
        match value {
            Value::String(s) => Ok(s.clone()),
            Value::TemplateString(segs) => Ok(crate::template::segments_to_string(segs)),
            Value::Path(s) | Value::Duration(s) | Value::Secret(s) => Ok(s.clone()),
            Value::Reference(s) | Value::Role(s) => Ok(s.clone()),
            _ => Err(ValueTypeError {
                expected: "string",
                actual: value.type_name(),
            }),
        }
    }
}

impl TryFrom<&Value> for f64 {
    type Error = ValueTypeError;

    fn try_from(value: &Value) -> Result<f64, Self::Error> {
        match value {
            Value::Number(n) => Ok(*n),
            _ => Err(ValueTypeError {
                expected: "number",
                actual: value.type_name(),
            }),
        }
    }
}

impl TryFrom<&Value> for i64 {
    type Error = ValueTypeError;

    fn try_from(value: &Value) -> Result<i64, Self::Error> {
        match value {
            Value::Number(n) => Ok(*n as i64),
            _ => Err(ValueTypeError {
                expected: "number",
                actual: value.type_name(),
            }),
        }
    }
}

impl TryFrom<&Value> for bool {
    type Error = ValueTypeError;

    fn try_from(value: &Value) -> Result<bool, Self::Error> {
        match value {
            Value::Bool(b) => Ok(*b),
            _ => Err(ValueTypeError {
                expected: "bool",
                actual: value.type_name(),
            }),
        }
    }
}

impl TryFrom<&Value> for Vec<String> {
    type Error = ValueTypeError;

    fn try_from(value: &Value) -> Result<Vec<String>, Self::Error> {
        match value {
            Value::Array(items) => {
                let mut result = Vec::new();
                for item in items {
                    let s = String::try_from(&item.value)?;
                    result.push(s);
                }
                Ok(result)
            }
            _ => Err(ValueTypeError {
                expected: "array",
                actual: value.type_name(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_primitive_type_object() {
        assert_eq!(PrimitiveType::from_str("object"), Some(PrimitiveType::Object));
        assert_eq!(PrimitiveType::Object.as_str(), "object");
    }

    #[test]
    fn try_from_string() {
        let v = Value::String("hello".into());
        assert_eq!(String::try_from(&v).unwrap(), "hello");
    }

    #[test]
    fn try_from_number() {
        let v = Value::Number(42.0);
        assert_eq!(f64::try_from(&v).unwrap(), 42.0);
        assert_eq!(i64::try_from(&v).unwrap(), 42);
    }

    #[test]
    fn try_from_bool() {
        let v = Value::Bool(true);
        assert_eq!(bool::try_from(&v).unwrap(), true);
    }

    #[test]
    fn try_from_type_mismatch() {
        let v = Value::Bool(true);
        assert!(String::try_from(&v).is_err());
    }

    #[test]
    fn try_from_reference() {
        let v = Value::Reference("MyProvider".into());
        assert_eq!(String::try_from(&v).unwrap(), "MyProvider");
    }

    #[test]
    fn try_from_role_ref() {
        let v = Value::Role("admin".into());
        assert_eq!(String::try_from(&v).unwrap(), "admin");
    }

    #[test]
    fn as_str_accessors() {
        assert_eq!(Value::String("hi".into()).as_str(), Some("hi"));
        assert_eq!(Value::Path("/tmp".into()).as_str(), Some("/tmp"));
        assert_eq!(Value::Reference("Ref".into()).as_str(), Some("Ref"));
        assert_eq!(Value::Number(1.0).as_str(), None);
    }

    #[test]
    fn as_f64_accessor() {
        assert_eq!(Value::Number(3.14).as_f64(), Some(3.14));
        assert_eq!(Value::String("x".into()).as_f64(), None);
    }

    #[test]
    fn as_bool_accessor() {
        assert_eq!(Value::Bool(true).as_bool(), Some(true));
        assert_eq!(Value::Number(1.0).as_bool(), None);
    }

    #[test]
    fn as_array_accessor() {
        let arr = Value::Array(vec![SpannedValue::new(
            Value::String("a".into()),
            Span::empty(0),
        )]);
        assert!(arr.as_array().is_some());
        assert_eq!(arr.as_array().unwrap().len(), 1);
        assert!(Value::String("x".into()).as_array().is_none());
    }

    // -------------------------------------------------------------------
    // Phase 4: Value type conversion edge cases
    // -------------------------------------------------------------------

    #[test]
    fn try_from_path_to_string() {
        let v = Value::Path("/usr/local/bin".into());
        assert_eq!(String::try_from(&v).unwrap(), "/usr/local/bin");
    }

    #[test]
    fn try_from_duration_to_string() {
        let v = Value::Duration("30s".into());
        assert_eq!(String::try_from(&v).unwrap(), "30s");
    }

    #[test]
    fn try_from_secret_to_string() {
        let v = Value::Secret("$ENV.KEY".into());
        assert_eq!(String::try_from(&v).unwrap(), "$ENV.KEY");
    }

    #[test]
    fn try_from_money_to_string_fails() {
        let v = Value::Money(crate::money::Money {
            amount: 1999,
            currency: "USD".into(),
            exponent: 2,
        });
        let result = String::try_from(&v);
        assert!(result.is_err());
    }

    #[test]
    fn try_from_array_to_string_fails() {
        let v = Value::Array(vec![]);
        let result = String::try_from(&v);
        assert!(result.is_err());
    }

    #[test]
    fn try_from_bool_to_f64_fails() {
        let v = Value::Bool(true);
        let result = f64::try_from(&v);
        assert!(result.is_err());
    }

    #[test]
    fn try_from_string_to_bool_fails() {
        let v = Value::String("true".into());
        let result = bool::try_from(&v);
        assert!(result.is_err());
    }

    #[test]
    fn try_from_number_to_bool_fails() {
        let v = Value::Number(1.0);
        let result = bool::try_from(&v);
        assert!(result.is_err());
    }

    #[test]
    fn i64_truncation() {
        let v = Value::Number(3.14);
        assert_eq!(i64::try_from(&v).unwrap(), 3);
    }

    #[test]
    fn i64_negative() {
        let v = Value::Number(-100.0);
        assert_eq!(i64::try_from(&v).unwrap(), -100);
    }

    #[test]
    fn vec_string_from_mixed_array_fails() {
        let v = Value::Array(vec![
            SpannedValue::new(Value::String("a".into()), Span::empty(0)),
            SpannedValue::new(Value::Number(42.0), Span::empty(0)),
        ]);
        let result = Vec::<String>::try_from(&v);
        assert!(result.is_err());
    }

    #[test]
    fn vec_string_from_empty_array() {
        let v = Value::Array(vec![]);
        let result = Vec::<String>::try_from(&v).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn as_str_template_string_returns_none() {
        let v = Value::TemplateString(vec![
            crate::types::TemplateSegment::Expression {
                namespace: "args".into(),
                path: vec!["name".into()],
                raw: "args.name".into(),
                span: Span::empty(0),
            },
        ]);
        assert!(v.as_str().is_none());
    }

    #[test]
    fn as_str_money_returns_none() {
        let v = Value::Money(crate::money::Money {
            amount: 1000,
            currency: "USD".into(),
            exponent: 2,
        });
        assert!(v.as_str().is_none());
    }

    #[test]
    fn as_f64_string_returns_none() {
        assert!(Value::String("42".into()).as_f64().is_none());
    }

    #[test]
    fn as_bool_string_returns_none() {
        assert!(Value::String("true".into()).as_bool().is_none());
    }

    #[test]
    fn type_name_all_variants() {
        assert_eq!(Value::String("".into()).type_name(), "string");
        assert_eq!(Value::TemplateString(vec![]).type_name(), "string");
        assert_eq!(Value::Number(0.0).type_name(), "number");
        assert_eq!(Value::Bool(false).type_name(), "bool");
        assert_eq!(Value::Duration("1s".into()).type_name(), "duration");
        assert_eq!(Value::Path("/x".into()).type_name(), "path");
        assert_eq!(Value::Secret("$ENV.X".into()).type_name(), "secret");
        assert_eq!(Value::Role("admin".into()).type_name(), "role");
        assert_eq!(Value::Reference("Ref".into()).type_name(), "reference");
        assert_eq!(Value::Array(vec![]).type_name(), "array");
    }
}
