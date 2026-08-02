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

/// The exact decimal number (RFC 0016). Lives in [`crate::decimal`];
/// re-exported here so the established `types::Number` path keeps working.
///
/// NML's `number` domain is the finite decimal128 value space: `2.50`
/// stores coefficient 250 at scale 2 and displays `"2.50"`; integers up to
/// 34 significant digits survive exactly. Equality/ordering/hashing are
/// numeric (`2 == 2.0`, `1.5 == 1.50`); conversions are
/// [`Number::to_i64`] (exact-or-None) and [`Number::to_f64`]
/// (correctly-rounded, the only sanctioned decimal→binary edge).
pub use crate::decimal::Number;

/// A parsed value in NML.
///
/// # Extracting typed data
///
/// Two flavors, one rule. The `to_*`/`as_*` methods are `Option` probes
/// for the common 64-bit tier ([`Value::to_i64`], [`Value::to_u64`],
/// [`Value::to_f64`]); the `TryFrom<&Value>` impls are the typed-error
/// flavor for config extraction. Integer `TryFrom` has exactly one rung —
/// `i128`, the width every Rust integer type through `u64` narrows from
/// exactly via `T::try_from` (a `u128` consumer needs
/// [`Number::to_u128`]: values in `(i128::MAX, u128::MAX]` exist) — so
/// no such target ever funnels through a narrower width
/// and silently loses range (the `(i64::MAX, u64::MAX]` band is the
/// classic casualty). Wider and narrower conversions live on [`Number`],
/// the single conversion home: extend the ladder there, never this
/// family.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Value {
    String(String),
    TemplateString(Vec<TemplateSegment>),
    Number(Number),
    Money(Money),
    Duration(Duration),
    Bool(bool),
    Secret(String),
    Role(String),
    Reference(String),
    Array(Vec<SpannedValue>),
    Fallback(Box<SpannedValue>, Box<SpannedValue>),
    /// Environment-resolved text, minted EXCLUSIVELY by
    /// [`crate::ValueResolver`]'s `$ENV` substitution — no other code
    /// constructs it, so holding one IS proof the text came from the
    /// environment, not from a source literal. That provenance is what
    /// gates the typed string coercions (string→number/bool/duration):
    /// they fire for `Resolved` and never for a source-authored quoted
    /// literal, which gets the same replaced-syntax teaching the schema
    /// validator gives. String-shaped everywhere else (string fields,
    /// enum discriminants, buffered serde contexts).
    Resolved(ResolvedText),
}

/// The payload of [`Value::Resolved`]: env-resolved text plus the
/// reference that produced it (`$ENV.KEY`, source spelling). The value is
/// frequently secret material, so leak-resistance is STRUCTURAL, not
/// convention: `Debug` prints `‹resolved $ENV.KEY›` (the reference is
/// source-visible; the content never prints) and `Serialize` emits the
/// bare marker `‹resolved›` — any future `dbg!`, error interpolation, or
/// stray serialization of a resolved tree is safe by construction.
/// Diagnostics NAME the variable (`$ENV.KEY is not duration syntax`)
/// while redacting the value — actionable and leak-free at once.
#[derive(Clone, PartialEq)]
pub struct ResolvedText {
    text: String,
    var: String,
}

impl ResolvedText {
    /// `var` is the full source spelling of the reference (`$ENV.KEY`);
    /// `text` is what it resolved to.
    pub fn new(var: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            var: var.into(),
        }
    }

    /// The resolved text. Handle like the secret it may be: never log,
    /// never serialize — the blanket impls on this type already refuse.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The `$ENV.KEY` reference that produced this value — safe in any
    /// diagnostic (it is source text), and the actionable half of an
    /// error: it names the knob to fix.
    pub fn var(&self) -> &str {
        &self.var
    }
}

impl std::fmt::Debug for ResolvedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "‹resolved {}›", self.var)
    }
}

impl Serialize for ResolvedText {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("‹resolved›")
    }
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

/// The exact duration (RFC 0017). Lives in [`crate::duration`];
/// re-exported here beside [`Value`] like [`Number`] is.
///
/// NML's `duration` domain is `total_nanos ≤ std::time::Duration::MAX`,
/// stored faithfully as the authored `(magnitude, unit)` pair and compared
/// semantically (`30s == 30000ms`); [`Duration::as_std`] is infallible by
/// construction.
pub use crate::duration::{Duration, DurationUnit};

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
    /// Exact time duration with unit suffix (RFC 0017; e.g. `30s`,
    /// `250ms`). The parser recognises duration literal syntax and stores
    /// values as the authored `(magnitude, unit)` pair, compared
    /// semantically (`30s == 30000ms`).
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
#[error(
    "unknown primitive type `{0}` (expected one of: string, number, money, bool, duration, path, secret, object, role)"
)]
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
    /// Construct a numeric value from an `i64` (the one integer `From`;
    /// see [`Number::from_u64`] and the `TryFrom` impls for wider types).
    /// Decimal literals use the const [`num!`](crate::num) macro —
    /// `Value::number(num!(2.50))` — since `From<f64>` is deliberately
    /// absent under exact decimals (RFC 0016 §1.9).
    pub fn number(n: impl Into<Number>) -> Value {
        Value::Number(n.into())
    }

    /// Returns a human-readable name for the value's variant.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::String(_) | Value::TemplateString(_) | Value::Resolved(_) => "string",
            Value::Number(_) => "number",
            Value::Money(_) => "money",
            Value::Duration(_) => "duration",
            Value::Bool(_) => "bool",
            Value::Secret(_) => "secret",
            Value::Role(_) => "role",
            Value::Reference(_) => "reference",
            Value::Array(_) => "array",
            Value::Fallback(_, _) => "fallback",
        }
    }

    /// Extract as a string slice (String, Secret, Reference, Role, and
    /// env-Resolved text). The Resolved arm is LOAD-BEARING: consumer
    /// code resolves secrets and reads them back through here
    /// (`resolved.as_str().unwrap_or_default()` is a live pattern) — a
    /// `None` for Resolved would silently turn every such credential
    /// into the empty string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) | Value::Secret(s) | Value::Reference(s) | Value::Role(s) => {
                Some(s.as_str())
            }
            Value::Resolved(r) => Some(r.as_str()),
            _ => None,
        }
    }

    /// Extract as an `f64` via the correctly-rounded conversion (lossy for
    /// integers above 2^53 and precision beyond binary64; see
    /// [`Value::to_i64`] for exact integer extraction, or match
    /// [`Value::Number`] for the exact decimal itself).
    pub fn to_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(n.to_f64()),
            _ => None,
        }
    }

    /// Extract as an exact integer. Fractional or out-of-range numbers
    /// yield `None` (never truncation).
    pub fn to_i64(&self) -> Option<i64> {
        match self {
            Value::Number(n) => n.to_i64(),
            _ => None,
        }
    }

    /// Extract as an exact `u64`. Fractional, negative, or out-of-range
    /// numbers yield `None` (never truncation). Present so a `u64`-typed
    /// consumer need not route through [`Value::to_i64`], which would
    /// reject the `(i64::MAX, u64::MAX]` band RFC 0016 made expressible
    /// — and would do so with a misleading "not an integer" message.
    pub fn to_u64(&self) -> Option<u64> {
        match self {
            Value::Number(n) => n.to_u64(),
            _ => None,
        }
    }

    /// Extract as an exact [`Duration`] (`Copy`; convert with
    /// [`Duration::as_std`] for a `std::time::Duration`, or use the
    /// `TryFrom<&Value>` impl below directly).
    pub fn as_duration(&self) -> Option<Duration> {
        match self {
            Value::Duration(d) => Some(*d),
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
            Value::Secret(s) => Ok(s.clone()),
            Value::Reference(s) | Value::Role(s) => Ok(s.clone()),
            // Env-resolved text is string-shaped for every string
            // destination (same load-bearing rule as `as_str`).
            Value::Resolved(r) => Ok(r.as_str().to_owned()),
            _ => Err(ValueTypeError {
                expected: "string",
                actual: value.type_name(),
            }),
        }
    }
}

/// Correctly-rounded, hence lossy for integers above 2^53 and precision
/// beyond binary64 — same contract as [`Value::to_f64`]. For exact
/// integer extraction use the `i128` impl below.
impl TryFrom<&Value> for f64 {
    type Error = ValueTypeError;

    fn try_from(value: &Value) -> Result<f64, Self::Error> {
        match value {
            Value::Number(n) => Ok(n.to_f64()),
            _ => Err(ValueTypeError {
                expected: "number",
                actual: value.type_name(),
            }),
        }
    }
}

/// The one typed-error integer rung. **Handle the `Err`.** Since RFC 0016
/// every number ≤ 34 significant digits parses, so a value outside the
/// target range no longer dies at parse — it arrives here and fails a
/// conversion instead. Config code that writes
/// `if let Ok(n) = i128::try_from(&v) { limit = Some(n) }` with no `else`
/// therefore skips silently and leaves the limit **unset**: a guard
/// disarmed by an out-of-range value. (An audit of a downstream consumer
/// found exactly this in five proxy guards, one of them a connection
/// cap.) Convert with an explicit error, and narrow with `T::try_from`
/// rather than `as` — `4294967296 as u32` is `0`.
///
/// Why `i128` and not `i64`: it is the convergence width. Every Rust
/// integer type through `u64` narrows from it exactly via std
/// `T::try_from`, so one rung serves every target with no band gap —
/// an `i64` rung would reject the `(i64::MAX, u64::MAX]` values RFC 0016
/// made expressible, with an error claiming they are out of 64-bit
/// range when they are not.
impl TryFrom<&Value> for i128 {
    type Error = ValueTypeError;

    fn try_from(value: &Value) -> Result<i128, Self::Error> {
        match value {
            // Reject fractional and out-of-range numbers rather than
            // silently truncating (e.g. 3.7 must not become 3);
            // value-based, so the integer written `512.0` converts.
            Value::Number(n) => n.to_i128().ok_or(ValueTypeError {
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

/// Typed-error extraction of a duration-typed field's literal. Infallible
/// past the variant check: the decode-time domain bound (RFC 0017 §2)
/// guarantees every [`Value::Duration`] converts. String-shaped duration
/// text (`$ENV` resolution) is `de`'s `coerce_to_duration` territory, not
/// this impl's — `TryFrom` reads the raw AST, where a string is a string.
impl TryFrom<&Value> for std::time::Duration {
    type Error = ValueTypeError;

    fn try_from(value: &Value) -> Result<std::time::Duration, Self::Error> {
        match value {
            Value::Duration(d) => Ok(d.as_std()),
            _ => Err(ValueTypeError {
                expected: "duration",
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

    // ── Value::Resolved / ResolvedText contract pins ────────────────

    /// Leak-resistance is STRUCTURAL: `Debug` names the variable (source
    /// text, safe) and never the content; `Serialize` emits the bare
    /// marker. A future `dbg!` or stray serialization of a resolved tree
    /// cannot leak — pinned here so a derive ever replacing the manual
    /// impls fails loudly.
    #[test]
    fn resolved_text_debug_and_serialize_redact_content() {
        let v = Value::Resolved(ResolvedText::new("$ENV.API_KEY", "hunter2"));
        let dbg = format!("{v:?}");
        assert!(dbg.contains("$ENV.API_KEY"), "names the knob: {dbg}");
        assert!(!dbg.contains("hunter2"), "never the content: {dbg}");
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json, serde_json::json!({ "Resolved": "‹resolved›" }));
    }

    /// The load-bearing string-shape pins: consumer code reads resolved
    /// credentials back through `as_str`/`TryFrom<String>` — a `None`/
    /// `Err` here silently turns every such credential into the empty
    /// string (`resolved.as_str().unwrap_or_default()` is a live
    /// pattern in consumers).
    #[test]
    fn resolved_is_string_shaped_for_extraction() {
        let v = Value::Resolved(ResolvedText::new("$ENV.TOKEN", "tok-123"));
        assert_eq!(v.as_str(), Some("tok-123"));
        assert_eq!(String::try_from(&v).unwrap(), "tok-123");
        assert_eq!(v.type_name(), "string");
    }

    /// Provenance is semantically real: resolved text never equals a
    /// source literal with the same content (one unlocks coercion, the
    /// other does not), and two Resolved values are equal only with the
    /// same variable AND text. No live comparator sees mixed trees — the
    /// differ is source-side — so this is a contract pin, not a behavior
    /// change.
    #[test]
    fn resolved_equality_is_provenance_aware() {
        let r = Value::Resolved(ResolvedText::new("$ENV.A", "5m"));
        assert_ne!(r, Value::String("5m".into()));
        assert!(!r.semantic_eq(&Value::String("5m".into())));
        assert_eq!(r, Value::Resolved(ResolvedText::new("$ENV.A", "5m")));
        assert_ne!(r, Value::Resolved(ResolvedText::new("$ENV.B", "5m")));
    }

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
        let v = Value::number(crate::num!(42.0));
        assert_eq!(f64::try_from(&v).unwrap(), 42.0);
        assert_eq!(i128::try_from(&v).unwrap(), 42);
    }

    /// The i128 rung is total over every band a config integer can
    /// occupy — including `(i64::MAX, u64::MAX]`, the band an i64 rung
    /// would falsely reject — and is value-based (`512.0` IS 512),
    /// while fractions and beyond-i128 magnitudes error rather than
    /// truncate.
    #[test]
    fn try_from_i128_covers_every_integer_band() {
        let num = |s: &str| Value::Number(s.parse().unwrap());
        assert_eq!(
            i128::try_from(&num("9223372036854775808")).unwrap(),
            1i128 << 63
        );
        assert_eq!(
            i128::try_from(&num("18446744073709551615")).unwrap(),
            u64::MAX as i128
        );
        assert_eq!(i128::try_from(&num("512.0")).unwrap(), 512);
        assert_eq!(i128::try_from(&num("-100.0")).unwrap(), -100);
        assert!(i128::try_from(&num("3.7")).is_err());
        let beyond_i128 = format!("1{}", "0".repeat(39));
        assert!(i128::try_from(&num(&beyond_i128)).is_err());
        assert!(i128::try_from(&Value::String("42".into())).is_err());
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
        assert_eq!(Value::Reference("Ref".into()).as_str(), Some("Ref"));
        assert_eq!(Value::number(crate::num!(1.0)).as_str(), None);
    }

    #[test]
    fn value_to_f64_accessor() {
        assert_eq!(Value::number(crate::num!(2.5)).to_f64(), Some(2.5));
        assert_eq!(Value::String("x".into()).to_f64(), None);
    }

    #[test]
    fn as_bool_accessor() {
        assert_eq!(Value::Bool(true).as_bool(), Some(true));
        assert_eq!(Value::number(crate::num!(1.0)).as_bool(), None);
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
        let n = Number::from(9_007_199_254_740_993);
        assert_eq!(n.to_i64(), Some(9_007_199_254_740_993));
        assert_eq!(Number::from(i64::MAX).to_i64(), Some(i64::MAX));
        assert_eq!(Number::from(i64::MIN).to_i64(), Some(i64::MIN));
    }

    #[test]
    fn number_equality_is_numeric() {
        assert_eq!(Number::from(3), crate::num!(3.0));
        assert_eq!(crate::num!(3.0), Number::from(3));
        assert_ne!(Number::from(3), crate::num!(3.5));
        // Exactness at the f64 cliff: under the old float model
        // `i64::MAX as f64` collapsed to 2^63; the exact decimal keeps
        // every i64 distinct.
        assert_ne!(
            Number::from(i64::MAX),
            "9223372036854775808".parse::<Number>().unwrap()
        );
        assert_eq!(
            Number::from(i64::MIN),
            "-9223372036854775808".parse::<Number>().unwrap()
        );
        // Scale is presentation, not value (RFC 0016 §1.3).
        assert_eq!(crate::num!(1.5), crate::num!(1.50));
    }

    #[test]
    fn number_to_i64_is_exact_or_none() {
        assert_eq!(crate::num!(3.5).to_i64(), None);
        // Value-based integrality: 3.0 is integral even in fraction form.
        assert_eq!(crate::num!(3.0).to_i64(), Some(3));
        // 2^63 is one past i64::MAX; must not wrap or saturate silently.
        assert_eq!(
            "9223372036854775808".parse::<Number>().unwrap().to_i64(),
            None
        );
        assert_eq!(
            "-9223372036854775808".parse::<Number>().unwrap().to_i64(),
            Some(i64::MIN)
        );
    }

    #[test]
    fn number_display() {
        assert_eq!(Number::from(42).to_string(), "42");
        assert_eq!(Number::from(i64::MAX).to_string(), "9223372036854775807");
        assert_eq!(crate::num!(2.5).to_string(), "2.5");
        // The written scale is real information and survives Display
        // (`2.0` stays `2.0`, `2.50` stays `2.50` — no {n:.1} faking).
        assert_eq!(crate::num!(2.0).to_string(), "2.0");
        assert_eq!(crate::num!(2.50).to_string(), "2.50");
    }

    #[test]
    fn number_display_roundtrips_member() {
        // Display output reparses to the identical stored member.
        for (n, text) in [
            (Number::from(8080), "8080"),
            (crate::num!(8080.0), "8080.0"),
            (crate::num!(0.75), "0.75"),
        ] {
            assert_eq!(n.to_string(), text);
            assert_eq!(text.parse::<Number>().unwrap(), n);
        }
    }

    /// RFC 0016 §2: the diff-cosmetic classification pair, pinned at the
    /// semantic_eq layer (the RFC 0032 reload differ's comparator).
    /// Scale-only edits are cosmetic; the old f64 fall-through falsely
    /// equated distinct integers beyond 2^53 — that must never return.
    #[test]
    fn semantic_eq_numeric_pairs() {
        let n = |s: &str| Value::Number(s.parse::<Number>().unwrap());
        assert!(
            n("1.5").semantic_eq(&n("1.50")),
            "scale-only edit is cosmetic"
        );
        assert!(n("2").semantic_eq(&n("2.0")));
        assert!(
            !n("9007199254740993.0").semantic_eq(&n("9007199254740992.0")),
            "distinct values beyond 2^53 must NOT classify as cosmetic (the f64-era bug)"
        );
    }

    /// RFC 0017 §2: the duration analogue of the numeric pin above, at the
    /// same semantic_eq layer the RFC 0032 reload differ reads. Duration
    /// equality is MANUAL over `total_nanos()` — a derived `(magnitude,
    /// unit)` equality would compile, pass every other test, and silently
    /// report `30s` → `30000ms` as a change (forcing a needless restart);
    /// this is the test that goes red instead.
    #[test]
    fn semantic_eq_duration_pairs() {
        let d = |mag: u64, unit: DurationUnit| {
            Value::Duration(Duration::new(mag, unit).expect("in-domain"))
        };
        assert!(
            d(30, DurationUnit::Seconds).semantic_eq(&d(30_000, DurationUnit::Milliseconds)),
            "unit respelling is cosmetic"
        );
        assert!(!d(30, DurationUnit::Seconds).semantic_eq(&d(31, DurationUnit::Seconds)));
        assert!(!d(30, DurationUnit::Seconds).semantic_eq(&d(30, DurationUnit::Minutes)));
    }

    #[test]
    fn as_duration_and_try_from_std() {
        let d = Duration::new(90, DurationUnit::Minutes).unwrap();
        let v = Value::Duration(d);
        assert_eq!(v.as_duration(), Some(d));
        assert_eq!(
            std::time::Duration::try_from(&v).unwrap(),
            std::time::Duration::from_secs(90 * 60)
        );
        assert!(Value::String("90m".into()).as_duration().is_none());
        // TryFrom reads the raw AST: a string is a string — coercion is
        // `de`'s job, deliberately not duplicated here.
        assert!(std::time::Duration::try_from(&Value::String("90m".into())).is_err());
    }

    #[test]
    fn value_to_i64_accessor() {
        assert_eq!(Value::number(42).to_i64(), Some(42));
        assert_eq!(Value::number(crate::num!(2.5)).to_i64(), None);
        assert_eq!(Value::String("42".into()).to_i64(), None);
    }

    // -------------------------------------------------------------------
    // Phase 4: Value type conversion edge cases
    // -------------------------------------------------------------------

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
        let v = Value::number(crate::num!(1.0));
        let result = bool::try_from(&v);
        assert!(result.is_err());
    }

    #[test]
    fn vec_string_from_mixed_array_fails() {
        let v = Value::Array(vec![
            SpannedValue::new(Value::String("a".into()), Span::empty(0)),
            SpannedValue::new(Value::number(crate::num!(42.0)), Span::empty(0)),
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
    fn value_to_f64_string_returns_none() {
        assert!(Value::String("42".into()).to_f64().is_none());
    }

    #[test]
    fn as_bool_string_returns_none() {
        assert!(Value::String("true".into()).as_bool().is_none());
    }

    #[test]
    fn type_name_all_variants() {
        assert_eq!(Value::String("".into()).type_name(), "string");
        assert_eq!(Value::TemplateString(vec![]).type_name(), "string");
        assert_eq!(Value::number(crate::num!(0.0)).type_name(), "number");
        assert_eq!(Value::Bool(false).type_name(), "bool");
        assert_eq!(Value::Secret("$ENV.X".into()).type_name(), "secret");
        assert_eq!(Value::Role("admin".into()).type_name(), "role");
        assert_eq!(Value::Reference("Ref".into()).type_name(), "reference");
        assert_eq!(Value::Array(vec![]).type_name(), "array");
    }
}
