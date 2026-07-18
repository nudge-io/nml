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

/// A numeric value: an exact 64-bit integer or an IEEE 754 double.
///
/// NML's `number` type covers whole numbers and decimals with a single
/// surface type. Internally, literals without a decimal point lex as
/// [`Number::Int`] so 64-bit integers survive exactly (a bare `f64`
/// silently corrupts integers above 2^53); literals with a decimal point
/// lex as [`Number::Float`].
///
/// Equality is numeric, not representational: `Int(3) == Float(3.0)`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(untagged)]
pub enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    /// The value as an `f64`. Lossy for integers above 2^53; use
    /// [`Number::as_i64`] when exactness matters.
    pub fn as_f64(self) -> f64 {
        match self {
            Number::Int(i) => i as f64,
            Number::Float(f) => f,
        }
    }

    /// The value as an exact `i64`. `Float`s convert only when they are
    /// whole and within range; fractional or out-of-range values yield
    /// `None` (never truncation).
    pub fn as_i64(self) -> Option<i64> {
        match self {
            Number::Int(i) => Some(i),
            Number::Float(f) => float_to_exact_i64(f),
        }
    }
}

/// Exact `f64` -> `i64` conversion: whole values within `i64` range only.
/// The upper bound is strict because `i64::MAX as f64` rounds up to 2^63,
/// which is one past `i64::MAX`.
fn float_to_exact_i64(f: f64) -> Option<i64> {
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    (f.fract() == 0.0 && (-TWO_POW_63..TWO_POW_63).contains(&f)).then_some(f as i64)
}

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Number::Int(a), Number::Int(b)) => a == b,
            (Number::Float(a), Number::Float(b)) => a == b,
            (Number::Int(a), Number::Float(b)) | (Number::Float(b), Number::Int(a)) => {
                float_to_exact_i64(*b) == Some(*a)
            }
        }
    }
}

impl PartialEq<f64> for Number {
    fn eq(&self, other: &f64) -> bool {
        *self == Number::Float(*other)
    }
}

impl PartialEq<i64> for Number {
    fn eq(&self, other: &i64) -> bool {
        *self == Number::Int(*other)
    }
}

impl From<i64> for Number {
    fn from(i: i64) -> Self {
        Number::Int(i)
    }
}

impl From<f64> for Number {
    fn from(f: f64) -> Self {
        Number::Float(f)
    }
}

impl std::fmt::Display for Number {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Number::Int(i) => write!(f, "{i}"),
            // A whole float keeps its decimal point (`2.0`, not `2`) so the
            // int/float distinction survives a format -> reparse round-trip
            // and the author's literal form is preserved.
            Number::Float(n) if n.fract() == 0.0 && n.is_finite() => write!(f, "{n:.1}"),
            Number::Float(n) => write!(f, "{n}"),
        }
    }
}

/// A parsed value in NML.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Value {
    String(String),
    TemplateString(Vec<TemplateSegment>),
    Number(Number),
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

/// A field directive (RFC 0032): `#name` / `#name(value)` trailing a field
/// definition. **Opaque to nml-core** — the language parses and syntax-checks
/// directives (one per key per field) but assigns no meaning; consumers (e.g.
/// nudge's `#live`/`#restart` reload classes, `#key` element identity)
/// interpret them. The single definition shared by the schema layer
/// (`model::FieldDef`) and the AST layer (`ast::FieldDefinition`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Directive {
    pub name: String,
    /// `#name(value)`'s argument, `None` for a bare `#name`.
    pub arg: Option<SpannedValue>,
    /// The whole directive's source span (`#` through the close).
    pub span: Span,
}

impl Value {
    /// Span-insensitive semantic equality (RFC 0032): compares **values only**,
    /// never source locations — two values are `semantic_eq` iff they mean the
    /// same thing, regardless of where (or in which file) they were written.
    ///
    /// The derived `PartialEq` is *span-sensitive* (`Array(Vec<SpannedValue>)`
    /// and `Fallback` recurse through `SpannedValue`, whose derived eq compares
    /// `Span`; a `TemplateString` expression segment carries a `span` and its
    /// `raw` source text), so `==` flags a value merely *moved to another line*
    /// as different — exactly the cosmetic false-positive a semantic diff or a
    /// set-uniqueness check must not produce. Use this instead for any
    /// meaning-level comparison.
    pub fn semantic_eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Array(a), Value::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.value.semantic_eq(&y.value))
            }
            (Value::Fallback(ap, af), Value::Fallback(bp, bf)) => {
                ap.value.semantic_eq(&bp.value) && af.value.semantic_eq(&bf.value)
            }
            (Value::TemplateString(a), Value::TemplateString(b)) => {
                a.len() == b.len()
                    && a.iter().zip(b).all(|(x, y)| match (x, y) {
                        (TemplateSegment::Literal(l), TemplateSegment::Literal(r)) => l == r,
                        (
                            TemplateSegment::Expression {
                                namespace: ln,
                                path: lp,
                                ..
                            },
                            TemplateSegment::Expression {
                                namespace: rn,
                                path: rp,
                                ..
                            },
                        ) => ln == rn && lp == rp,
                        _ => false,
                    })
            }
            // Every remaining variant is span-free, so derived equality IS
            // semantic there; mixed variants compare unequal, as they should.
            (a, b) => a == b,
        }
    }
}

/// The primitive type names recognized in model definitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PrimitiveType {
    String,
    Number,
    /// Exact currency value with ISO 4217 code (e.g. `19.99 USD`).
    /// The parser recognises currency literal syntax and stores values as
    /// integer minor units for precision.
    Money,
    Bool,
    /// Tooling hint for values representing time durations.
    /// No parser-level coercion -- treated as a string at runtime.
    Duration,
    /// Tooling hint for values representing filesystem paths.
    /// No parser-level coercion -- treated as a string at runtime.
    Path,
    Secret,
    /// Flexible key-value nested block; accepts any keys with scalar values.
    Object,
    /// Typed reference using `@kind/name` syntax (e.g. `@role/admin`).
    /// Despite the name, the underlying syntax is a generic tagged-reference
    /// pattern usable for any `@namespace/identifier` value.
    Role,
}

/// Error returned when a string is not a recognized primitive type name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown primitive type `{0}` (expected one of: string, number, money, bool, duration, path, secret, object, role)")]
pub struct UnknownPrimitiveType(pub String);

impl std::str::FromStr for PrimitiveType {
    type Err = UnknownPrimitiveType;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "string" => Ok(PrimitiveType::String),
            "number" => Ok(PrimitiveType::Number),
            "money" => Ok(PrimitiveType::Money),
            "bool" => Ok(PrimitiveType::Bool),
            "duration" => Ok(PrimitiveType::Duration),
            "path" => Ok(PrimitiveType::Path),
            "secret" => Ok(PrimitiveType::Secret),
            "object" => Ok(PrimitiveType::Object),
            "role" => Ok(PrimitiveType::Role),
            other => Err(UnknownPrimitiveType(other.to_string())),
        }
    }
}

impl PrimitiveType {
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
    /// Construct a numeric value from any integer or float.
    pub fn number(n: impl Into<Number>) -> Value {
        Value::Number(n.into())
    }

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

    /// Extract as a number (lossy for integers above 2^53; see
    /// [`Value::as_i64`] for exact integer extraction).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(n.as_f64()),
            _ => None,
        }
    }

    /// Extract as an exact integer. Fractional or out-of-range numbers
    /// yield `None` (never truncation).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(n) => n.as_i64(),
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
            Value::Number(n) => Ok(n.as_f64()),
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
            // Reject fractional and out-of-range numbers rather than
            // silently truncating (e.g. 3.7 must not become 3).
            Value::Number(n) => n.as_i64().ok_or(ValueTypeError {
                expected: "integer",
                actual: "fractional or out-of-range number",
            }),
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
        assert_eq!("object".parse(), Ok(PrimitiveType::Object));
        assert_eq!(PrimitiveType::Object.as_str(), "object");
        assert_eq!(
            "blob".parse::<PrimitiveType>(),
            Err(UnknownPrimitiveType("blob".to_string()))
        );
    }

    #[test]
    fn try_from_string() {
        let v = Value::String("hello".into());
        assert_eq!(String::try_from(&v).unwrap(), "hello");
    }

    #[test]
    fn try_from_number() {
        let v = Value::number(42.0);
        assert_eq!(f64::try_from(&v).unwrap(), 42.0);
        assert_eq!(i64::try_from(&v).unwrap(), 42);
    }

    #[test]
    fn try_from_bool() {
        let v = Value::Bool(true);
        assert!(bool::try_from(&v).unwrap());
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
        assert_eq!(Value::number(1.0).as_str(), None);
    }

    #[test]
    fn as_f64_accessor() {
        assert_eq!(Value::number(2.5).as_f64(), Some(2.5));
        assert_eq!(Value::String("x".into()).as_f64(), None);
    }

    #[test]
    fn as_bool_accessor() {
        assert_eq!(Value::Bool(true).as_bool(), Some(true));
        assert_eq!(Value::number(1.0).as_bool(), None);
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
    // Number: exactness, equality, display
    // -------------------------------------------------------------------

    #[test]
    fn number_int_exact_above_2_pow_53() {
        // 2^53 + 1 is the smallest integer f64 cannot represent.
        let n = Number::Int(9_007_199_254_740_993);
        assert_eq!(n.as_i64(), Some(9_007_199_254_740_993));
        assert_eq!(Number::Int(i64::MAX).as_i64(), Some(i64::MAX));
        assert_eq!(Number::Int(i64::MIN).as_i64(), Some(i64::MIN));
    }

    #[test]
    fn number_equality_is_numeric() {
        assert_eq!(Number::Int(3), Number::Float(3.0));
        assert_eq!(Number::Float(3.0), Number::Int(3));
        assert_ne!(Number::Int(3), Number::Float(3.5));
        // i64::MAX as f64 rounds up to 2^63, which is NOT i64::MAX.
        assert_ne!(Number::Int(i64::MAX), Number::Float(i64::MAX as f64));
        // i64::MIN is exactly -2^63, which f64 represents exactly.
        assert_eq!(Number::Int(i64::MIN), Number::Float(i64::MIN as f64));
        assert_ne!(Number::Float(f64::NAN), Number::Float(f64::NAN));
    }

    #[test]
    fn number_float_to_i64_is_exact_or_none() {
        assert_eq!(Number::Float(3.5).as_i64(), None);
        assert_eq!(Number::Float(f64::NAN).as_i64(), None);
        assert_eq!(Number::Float(f64::INFINITY).as_i64(), None);
        // 2^63 is one past i64::MAX; must not wrap or saturate silently.
        assert_eq!(Number::Float(9_223_372_036_854_775_808.0).as_i64(), None);
        assert_eq!(
            Number::Float(-9_223_372_036_854_775_808.0).as_i64(),
            Some(i64::MIN)
        );
    }

    #[test]
    fn number_display() {
        assert_eq!(Number::Int(42).to_string(), "42");
        assert_eq!(Number::Int(i64::MAX).to_string(), "9223372036854775807");
        assert_eq!(Number::Float(2.5).to_string(), "2.5");
        // Whole floats keep their decimal point: the literal form
        // round-trips through format -> reparse without changing variant.
        assert_eq!(Number::Float(2.0).to_string(), "2.0");
    }

    #[test]
    fn number_display_roundtrips_variant() {
        // Display output must reparse to the same variant.
        for (n, text) in [
            (Number::Int(8080), "8080"),
            (Number::Float(8080.0), "8080.0"),
            (Number::Float(0.75), "0.75"),
        ] {
            assert_eq!(n.to_string(), text);
        }
    }

    #[test]
    fn value_as_i64_accessor() {
        assert_eq!(Value::number(42).as_i64(), Some(42));
        assert_eq!(Value::number(2.5).as_i64(), None);
        assert_eq!(Value::String("42".into()).as_i64(), None);
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
        let v = Value::number(1.0);
        let result = bool::try_from(&v);
        assert!(result.is_err());
    }

    #[test]
    fn i64_rejects_fractional() {
        // 3.7 is not an integer; conversion must fail rather than truncate.
        let v = Value::number(3.7);
        assert!(i64::try_from(&v).is_err());
    }

    #[test]
    fn i64_negative() {
        let v = Value::number(-100.0);
        assert_eq!(i64::try_from(&v).unwrap(), -100);
    }

    #[test]
    fn vec_string_from_mixed_array_fails() {
        let v = Value::Array(vec![
            SpannedValue::new(Value::String("a".into()), Span::empty(0)),
            SpannedValue::new(Value::number(42.0), Span::empty(0)),
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
        let v = Value::TemplateString(vec![crate::types::TemplateSegment::Expression {
            namespace: "args".into(),
            path: vec!["name".into()],
            raw: "args.name".into(),
            span: Span::empty(0),
        }]);
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
        assert_eq!(Value::number(0.0).type_name(), "number");
        assert_eq!(Value::Bool(false).type_name(), "bool");
        assert_eq!(Value::Duration("1s".into()).type_name(), "duration");
        assert_eq!(Value::Path("/x".into()).type_name(), "path");
        assert_eq!(Value::Secret("$ENV.X".into()).type_name(), "secret");
        assert_eq!(Value::Role("admin".into()).type_name(), "role");
        assert_eq!(Value::Reference("Ref".into()).type_name(), "reference");
        assert_eq!(Value::Array(vec![]).type_name(), "array");
    }
}
