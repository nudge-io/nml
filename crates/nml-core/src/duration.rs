//! Exact duration values (RFC 0017): the authored integer magnitude and the
//! authored unit, compared semantically.
//!
//! Storage is **faithful** — never rescaled — so `fmt` renders `72h` as
//! `72h`; comparison is **semantic** via [`Duration::total_nanos`], so
//! `30s == 30000ms`. That split is the type's whole point: the reload
//! differ must classify `30s` → `30000ms` as no change (nudge's RFC 0032),
//! while the formatter must never rewrite the author's unit.
//!
//! The value domain is bounded at construction
//! ([`Duration::new`]), so [`Duration::as_std`] is infallible: every
//! constructed value converts to a [`std::time::Duration`] without a
//! fallible edge leaking into consumers — the same
//! reject-at-decode posture money takes for amounts beyond `i64` minor
//! units (`NML3003`).

use crate::error::NmlError;
use crate::span::Span;
use serde::{Serialize, Serializer};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

/// A duration's unit. Units are wall-clock-exact by design: calendar units
/// (`d`, `w`, `mo`, `y`) are deliberately absent — a day is not always
/// 86,400 seconds, and a configuration language that pretends otherwise
/// ships timezone bugs (RFC 0017 §8; `720h` is exact where `30d` is not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationUnit {
    Hours,
    Minutes,
    Seconds,
    Milliseconds,
}

/// Serializes as the unit's source suffix (`"s"`), matching the literal
/// grammar — the `nml parse` wire form is an API, and the suffix is its
/// one stable spelling.
impl Serialize for DurationUnit {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.suffix())
    }
}

impl DurationUnit {
    /// Every unit, coarsest first — the iteration order behind
    /// [`DurationUnit::finer`] and the did-you-mean candidate list.
    pub const ALL: [DurationUnit; 4] = [
        DurationUnit::Hours,
        DurationUnit::Minutes,
        DurationUnit::Seconds,
        DurationUnit::Milliseconds,
    ];

    /// The unit's source suffix (`"h"`, `"m"`, `"s"`, `"ms"`).
    pub fn suffix(self) -> &'static str {
        match self {
            DurationUnit::Hours => "h",
            DurationUnit::Minutes => "m",
            DurationUnit::Seconds => "s",
            DurationUnit::Milliseconds => "ms",
        }
    }

    /// Classify a suffix. **The single authority for what counts as a
    /// duration unit** — the literal decoder, the `de` coercion, and the
    /// LSP's unit completion all route through here, so the grammar cannot
    /// fork. Case-sensitive by design: `30S` is a rejection with a fix,
    /// never a case-fold, so `M`/`m` stays unambiguous forever (RFC 0017
    /// §1 — a language accepting both would owe an answer to "is `30M`
    /// minutes or months?").
    pub fn from_suffix(text: &str) -> Option<DurationUnit> {
        Self::ALL.into_iter().find(|u| u.suffix() == text)
    }

    /// Nanoseconds per one of this unit.
    pub fn nanos(self) -> u64 {
        match self {
            DurationUnit::Hours => 3_600_000_000_000,
            DurationUnit::Minutes => 60_000_000_000,
            DurationUnit::Seconds => 1_000_000_000,
            DurationUnit::Milliseconds => 1_000_000,
        }
    }

    /// The units strictly finer than this one, coarsest first — the
    /// candidate order for the fractional-magnitude fix (`30.5s` →
    /// `30500ms` prefers the coarsest unit that makes the value integral).
    fn finer(self) -> &'static [DurationUnit] {
        match self {
            DurationUnit::Hours => &[
                DurationUnit::Minutes,
                DurationUnit::Seconds,
                DurationUnit::Milliseconds,
            ],
            DurationUnit::Minutes => &[DurationUnit::Seconds, DurationUnit::Milliseconds],
            DurationUnit::Seconds => &[DurationUnit::Milliseconds],
            DurationUnit::Milliseconds => &[],
        }
    }
}

/// `std::time::Duration::MAX` in nanoseconds — the value-domain ceiling.
/// (`u64::MAX` seconds + 999,999,999 ns; far inside `u128`, so
/// [`Duration::total_nanos`] arithmetic can never overflow.)
const STD_MAX_NANOS: u128 = u64::MAX as u128 * 1_000_000_000 + 999_999_999;

/// Exact duration: the authored integer magnitude and the authored unit.
/// Storage is faithful (never rescaled) so `fmt` renders `72h` as `72h`;
/// comparison is semantic via [`Duration::total_nanos`].
///
/// `PartialEq`/`Eq`/`Hash`/`Ord` are **manual, over `total_nanos()`** —
/// deliberately not derived. A derived equality would compare the stored
/// `(magnitude, unit)` pair, making `30s != 30000ms`; nothing would fail
/// to compile and no existing test would go red, but the reload differ
/// would report a spurious change and force a needless restart — the
/// exact defect this type exists to close (RFC 0017 §2; `Number` in
/// [`crate::decimal`] is the same pattern over `normalized()`).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Duration {
    magnitude: u64,
    unit: DurationUnit,
}

impl Duration {
    /// The largest magnitude representable in `unit`: the value domain is
    /// `total_nanos() <= std::time::Duration::MAX`, and the magnitude must
    /// also fit the `u64` storage (which binds only for `ms`, whose
    /// domain ceiling exceeds `u64`).
    pub fn max_magnitude(unit: DurationUnit) -> u64 {
        u64::try_from(STD_MAX_NANOS / unit.nanos() as u128).unwrap_or(u64::MAX)
    }

    /// Construct a duration, `None` when `magnitude` is outside the
    /// unit's domain (see [`Duration::max_magnitude`]). The one gate that
    /// makes every downstream conversion infallible.
    pub fn new(magnitude: u64, unit: DurationUnit) -> Option<Duration> {
        (magnitude <= Self::max_magnitude(unit)).then_some(Duration { magnitude, unit })
    }

    /// The authored magnitude, exactly as written (`72` for `72h`).
    pub fn magnitude(self) -> u64 {
        self.magnitude
    }

    /// The authored unit (`Hours` for `72h`).
    pub fn unit(self) -> DurationUnit {
        self.unit
    }

    /// The total in nanoseconds — the semantic comparison basis. Always
    /// exact: `u64::MAX × 10^12` is far inside `u128`.
    pub fn total_nanos(self) -> u128 {
        self.magnitude as u128 * self.unit.nanos() as u128
    }

    /// Convert to [`std::time::Duration`] — **infallible by
    /// construction**: the domain check in [`Duration::new`] guarantees
    /// the total fits.
    pub fn as_std(self) -> std::time::Duration {
        let nanos = self.total_nanos();
        std::time::Duration::new(
            (nanos / 1_000_000_000) as u64,
            (nanos % 1_000_000_000) as u32,
        )
    }

    /// Parse the **text form** of a duration (`"30s"`) — the `de` coercion
    /// grammar for values that arrive as strings by construction
    /// (`$ENV` resolution, template output; RFC 0017 §3.1). Postel for
    /// machine-emitted data: outer whitespace and a gap before the unit
    /// are tolerated (`" 30 s "`); the value grammar itself — unsigned
    /// integer, one unit — is exactly the literal's. The error carries a
    /// *reason* and **never the input**: any coerced string could be a
    /// resolved secret.
    pub fn parse_text(text: &str) -> Result<Duration, DurationTextError> {
        let trimmed = text.trim_ascii();
        let split = trimmed
            .bytes()
            .position(|b| !b.is_ascii_digit())
            .unwrap_or(trimmed.len());
        let (digits, rest) = trimmed.split_at(split);
        let unit = DurationUnit::from_suffix(rest.trim_ascii_start())
            .ok_or(DurationTextError::Malformed)?;
        if digits.is_empty() {
            return Err(DurationTextError::Malformed);
        }
        let magnitude: u64 = digits
            .parse()
            .map_err(|_| DurationTextError::OutOfRange(unit))?;
        Duration::new(magnitude, unit).ok_or(DurationTextError::OutOfRange(unit))
    }
}

/// Canonical source form: magnitude attached to suffix (`30s`) — the shape
/// `fmt` emits and the spec names canonical. Attached diverges from money's
/// spaced form deliberately: a currency code is a noun after a quantity,
/// while a duration unit is a suffix in universal convention (systemd, Go,
/// Prometheus, ISO-8601 all attach).
impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.magnitude, self.unit.suffix())
    }
}

// Semantic equality/ordering/hashing over the nanosecond total — see the
// type-level doc for why these are hand-written.
impl PartialEq for Duration {
    fn eq(&self, other: &Self) -> bool {
        self.total_nanos() == other.total_nanos()
    }
}
impl Eq for Duration {}

impl Ord for Duration {
    fn cmp(&self, other: &Self) -> Ordering {
        self.total_nanos().cmp(&other.total_nanos())
    }
}
impl PartialOrd for Duration {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Coherent with the semantic `Eq` (`hash(30s) == hash(30000ms)`), making
/// `Duration` a valid map key.
impl Hash for Duration {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.total_nanos().hash(state);
    }
}

/// The coercion text grammar, as `str::parse` — `"30s".parse::<Duration>()`
/// — the same ergonomic surface [`crate::decimal::Number`] offers.
impl std::str::FromStr for Duration {
    type Err = DurationTextError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Duration::parse_text(s)
    }
}

/// Why a coerced duration **text** failed ([`Duration::parse_text`]).
/// `Display` states the reason and never the input — the never-echo rule
/// of `de`'s coercion family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationTextError {
    /// Not `digits + unit` (`h`, `m`, `s`, `ms`).
    Malformed,
    /// Well-formed, but the magnitude exceeds the unit's domain.
    OutOfRange(DurationUnit),
}

impl fmt::Display for DurationTextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DurationTextError::Malformed => {
                write!(
                    f,
                    "not a duration: expected an integer with unit h, m, s, or ms"
                )
            }
            DurationTextError::OutOfRange(unit) => write!(
                f,
                "duration out of range: the maximum is {}{}",
                Duration::max_magnitude(*unit),
                unit.suffix()
            ),
        }
    }
}

/// Why a duration **literal** failed to decode (RFC 0017 §5) — fully
/// structured, like [`crate::money::MoneyErrorKind`]: message, stable
/// code, and any machine-applicable fix all derive from the payload,
/// never from message text.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DurationErrorKind {
    /// The trailing identifier is neither a currency code nor a duration
    /// unit (`30x`, `30S`, `30sec`). `unit_span` is the suffix token's own
    /// span, so the nearest-unit did-you-mean is machine-applicable.
    UnknownUnit { unit: String, unit_span: Span },
    /// The magnitude is not an integer (`30.5s`). `equivalent` is the
    /// same value respelled to make it integral — the coarsest unit that
    /// works (`30500ms`; `30.0s` → `30s`) — when one exists in-domain.
    FractionalMagnitude {
        raw: String,
        equivalent: Option<String>,
    },
    /// Outside the value domain: negative, or `total_nanos` beyond
    /// `std::time::Duration::MAX` (RFC 0017 §2 — the check that makes
    /// [`Duration::as_std`] infallible).
    OutOfRange { unit: DurationUnit, negative: bool },
}

impl DurationErrorKind {
    /// The human-facing message, derived from the payload.
    pub fn message(&self) -> String {
        use crate::error::echo;
        match self {
            DurationErrorKind::UnknownUnit { unit, .. } => format!(
                "unknown unit `{}` after a number: a duration unit is h, m, s, or ms; \
                 a currency code is 3 uppercase letters",
                echo(unit)
            ),
            DurationErrorKind::FractionalMagnitude { raw, .. } => format!(
                "a duration magnitude is a whole number, but \"{}\" has a fractional \
                 part — write the value in a finer unit",
                echo(raw)
            ),
            DurationErrorKind::OutOfRange { negative: true, .. } => {
                "a duration cannot be negative".to_string()
            }
            DurationErrorKind::OutOfRange { unit, .. } => format!(
                "duration out of range: the maximum is {}{}",
                Duration::max_magnitude(*unit),
                unit.suffix()
            ),
        }
    }

    /// The stable code — total: duration errors are always coded.
    pub fn code(&self) -> crate::diagnostic::Code {
        use crate::diagnostic::codes;
        match self {
            DurationErrorKind::UnknownUnit { .. } => codes::UNKNOWN_UNIT,
            DurationErrorKind::FractionalMagnitude { .. } => codes::FRACTIONAL_DURATION,
            DurationErrorKind::OutOfRange { .. } => codes::DURATION_OUT_OF_RANGE,
        }
    }
}

/// Parse a duration literal (`30s`) from its decoded CST tokens.
///
/// `raw` is the magnitude text with any authored sign (`-30`); `unit_text`
/// is the trailing identifier, already known **not** to be a currency code
/// (the decoder routes 3-uppercase suffixes to money first); `span` covers
/// the whole literal and `unit_span` exactly the suffix token. Rejection
/// order — unit, sign, integrality, domain — puts the most fundamental
/// defect first (`-30x` is an unknown unit before it is a negative).
pub fn parse_duration_literal(
    raw: &str,
    unit_text: &str,
    span: Span,
    unit_span: Span,
) -> Result<Duration, NmlError> {
    let err = |kind: DurationErrorKind| NmlError::Duration { kind, span };
    let Some(unit) = DurationUnit::from_suffix(unit_text) else {
        return Err(err(DurationErrorKind::UnknownUnit {
            unit: crate::error::echo_capture(unit_text),
            unit_span,
        }));
    };
    let (negative, digits) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    if negative {
        return Err(err(DurationErrorKind::OutOfRange {
            unit,
            negative: true,
        }));
    }
    let out_of_range = || {
        err(DurationErrorKind::OutOfRange {
            unit,
            negative: false,
        })
    };
    let n = crate::decimal::Number::parse_literal(digits).map_err(|e| match e {
        // The lexer only emits digit-shaped Number tokens and the decoder
        // intercepts trailing dots upstream (NML0013, as for money), so
        // only the range kinds are reachable — a >34-digit magnitude is
        // truthfully out of the duration domain. Malformed/TrailingDot
        // degrade to the literal-layer number error rather than panic.
        crate::decimal::NumberError::Range(_) => out_of_range(),
        crate::decimal::NumberError::Malformed | crate::decimal::NumberError::TrailingDot => {
            NmlError::syntax(
                crate::error::ParseErrorKind::InvalidNumber {
                    raw: crate::error::echo_capture(digits),
                },
                span,
            )
        }
    })?;
    if n.scale() > 0 {
        return Err(err(DurationErrorKind::FractionalMagnitude {
            raw: crate::error::echo_capture(digits),
            equivalent: fractional_equivalent(n, unit),
        }));
    }
    let magnitude = n.to_u64().ok_or_else(out_of_range)?;
    Duration::new(magnitude, unit).ok_or_else(out_of_range)
}

/// The machine-applicable respelling for a fractional magnitude: the value
/// in the coarsest unit (starting with the authored one) where it is an
/// exact non-negative integer inside the domain — `30.0s` → `30s`,
/// `30.5s` → `30500ms`, `1.5h` → `90m`; `0.5ms` has none.
fn fractional_equivalent(n: crate::decimal::Number, unit: DurationUnit) -> Option<String> {
    if let Some(mag) = n.to_u64() {
        // Integral value in fractional form (`30.0s`) — same unit.
        return Duration::new(mag, unit).map(|d| d.to_string());
    }
    let coeff = u128::try_from(n.coeff()).ok()?;
    let pow10 = 10u128.checked_pow(u32::try_from(n.scale()).ok()?)?;
    for finer in unit.finer() {
        let factor = (unit.nanos() / finer.nanos()) as u128;
        let scaled = coeff.checked_mul(factor)?;
        if scaled % pow10 != 0 {
            continue;
        }
        let mag = u64::try_from(scaled / pow10).ok()?;
        return Duration::new(mag, *finer).map(|d| d.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dur(magnitude: u64, unit: DurationUnit) -> Duration {
        Duration::new(magnitude, unit).expect("in-domain test value")
    }

    #[test]
    fn equality_is_semantic_across_units() {
        assert_eq!(
            dur(30, DurationUnit::Seconds),
            dur(30_000, DurationUnit::Milliseconds)
        );
        assert_eq!(dur(1, DurationUnit::Hours), dur(60, DurationUnit::Minutes));
        assert_eq!(
            dur(1, DurationUnit::Hours),
            dur(3_600, DurationUnit::Seconds)
        );
        assert_ne!(
            dur(30, DurationUnit::Seconds),
            dur(31, DurationUnit::Seconds)
        );
        assert_ne!(
            dur(30, DurationUnit::Seconds),
            dur(30_001, DurationUnit::Milliseconds)
        );
        // Zero is valid and semantically equal across units.
        assert_eq!(
            dur(0, DurationUnit::Seconds),
            dur(0, DurationUnit::Milliseconds)
        );
    }

    #[test]
    fn display_is_faithful_never_rescaled() {
        assert_eq!(dur(72, DurationUnit::Hours).to_string(), "72h");
        assert_eq!(
            dur(30_000, DurationUnit::Milliseconds).to_string(),
            "30000ms"
        );
        assert_eq!(dur(0, DurationUnit::Seconds).to_string(), "0s");
    }

    /// Equality/ordering/hashing laws over a cross-unit value grid:
    /// `Eq`-consistent `Ord`, hash coherence, and agreement with the
    /// nanosecond total (the hand-rolled sweep style of the cst fuzz
    /// batteries — deterministic, exhaustive over the grid).
    #[test]
    fn ordering_laws_across_units() {
        let mut grid: Vec<Duration> = Vec::new();
        for unit in DurationUnit::ALL {
            for mag in [0u64, 1, 2, 59, 60, 61, 999, 1_000, 3_600, 86_400] {
                grid.push(dur(mag, unit));
            }
        }
        let hash_of = |d: &Duration| {
            use std::hash::{DefaultHasher, Hasher as _};
            let mut h = DefaultHasher::new();
            d.hash(&mut h);
            h.finish()
        };
        for a in &grid {
            for b in &grid {
                assert_eq!(a.cmp(b), a.total_nanos().cmp(&b.total_nanos()));
                assert_eq!(a == b, a.cmp(b) == Ordering::Equal, "{a} vs {b}");
                if a == b {
                    assert_eq!(hash_of(a), hash_of(b), "{a} vs {b}");
                }
                for c in &grid {
                    if a <= b && b <= c {
                        assert!(a <= c, "transitivity: {a} {b} {c}");
                    }
                }
            }
        }
    }

    #[test]
    fn domain_boundary_per_unit() {
        for unit in DurationUnit::ALL {
            let max = Duration::max_magnitude(unit);
            assert!(Duration::new(max, unit).is_some(), "{}", unit.suffix());
            if max < u64::MAX {
                assert!(Duration::new(max + 1, unit).is_none(), "{}", unit.suffix());
            }
        }
        // Seconds: the std ceiling is exactly u64::MAX seconds.
        assert_eq!(Duration::max_magnitude(DurationUnit::Seconds), u64::MAX);
        // Milliseconds: the u64 storage binds before the std ceiling does.
        assert_eq!(
            Duration::max_magnitude(DurationUnit::Milliseconds),
            u64::MAX
        );
        // Hours/minutes: the std ceiling binds below u64::MAX.
        assert!(Duration::max_magnitude(DurationUnit::Hours) < u64::MAX);
        assert!(Duration::max_magnitude(DurationUnit::Minutes) < u64::MAX);
    }

    #[test]
    fn as_std_is_exact_at_the_boundary() {
        for unit in DurationUnit::ALL {
            let d = dur(Duration::max_magnitude(unit), unit);
            let std = d.as_std();
            assert_eq!(
                std.as_secs() as u128 * 1_000_000_000 + std.subsec_nanos() as u128,
                d.total_nanos()
            );
        }
        assert_eq!(
            dur(2, DurationUnit::Hours).as_std(),
            std::time::Duration::from_secs(7200)
        );
        assert_eq!(
            dur(1500, DurationUnit::Milliseconds).as_std(),
            std::time::Duration::from_millis(1500)
        );
    }

    #[test]
    fn parse_text_accepts_the_coercion_grammar() {
        assert_eq!(
            Duration::parse_text("30s"),
            Ok(dur(30, DurationUnit::Seconds))
        );
        // `FromStr` is the same grammar (`Number` parity).
        assert_eq!("30s".parse(), Ok(dur(30, DurationUnit::Seconds)));
        assert_eq!(
            Duration::parse_text(" 500 ms "),
            Ok(dur(500, DurationUnit::Milliseconds))
        );
        assert_eq!(
            Duration::parse_text("72h"),
            Ok(dur(72, DurationUnit::Hours))
        );
        assert_eq!(
            Duration::parse_text("0ms"),
            Ok(dur(0, DurationUnit::Milliseconds))
        );
        for bad in [
            "", "30", "s", "30x", "30S", "-30s", "3.5s", "30 sec", "1h30m",
        ] {
            assert_eq!(
                Duration::parse_text(bad),
                Err(DurationTextError::Malformed),
                "{bad:?}"
            );
        }
        let over = format!(
            "{}h",
            Duration::max_magnitude(DurationUnit::Hours) as u128 + 1
        );
        assert_eq!(
            Duration::parse_text(&over),
            Err(DurationTextError::OutOfRange(DurationUnit::Hours))
        );
    }

    #[test]
    fn parse_text_errors_never_echo_the_input() {
        // The input may be a resolved secret — the reason must stand alone.
        let err = Duration::parse_text("hunter2").unwrap_err().to_string();
        assert!(!err.contains("hunter2"), "{err}");
    }

    #[test]
    fn literal_rejection_order_and_codes() {
        use crate::diagnostic::codes;
        let span = Span::new(0, 4);
        let kind_of =
            |raw: &str, unit: &str| match parse_duration_literal(raw, unit, span, Span::new(2, 4))
                .unwrap_err()
            {
                NmlError::Duration { kind, .. } => kind,
                other => panic!("expected a duration error, got {other:?}"),
            };
        // Unknown unit before sign: `-30x` classifies the suffix first.
        assert!(matches!(
            kind_of("-30", "x"),
            DurationErrorKind::UnknownUnit { .. }
        ));
        assert_eq!(kind_of("30", "sec").code(), codes::UNKNOWN_UNIT);
        // Sign before integrality: `-30.5s` is the negativity error.
        assert!(matches!(
            kind_of("-30.5", "s"),
            DurationErrorKind::OutOfRange { negative: true, .. }
        ));
        assert_eq!(kind_of("30.5", "s").code(), codes::FRACTIONAL_DURATION);
        assert_eq!(kind_of("-30", "s").code(), codes::DURATION_OUT_OF_RANGE);
        // The RFC's 23-digit example: a valid Number, not a valid duration.
        assert_eq!(
            kind_of("12345678901234567890123", "s").code(),
            codes::DURATION_OUT_OF_RANGE
        );
        // >34 digits: still the duration-domain error, never a panic.
        assert_eq!(
            kind_of(&"9".repeat(40), "s").code(),
            codes::DURATION_OUT_OF_RANGE
        );
        assert!(parse_duration_literal("30", "s", span, Span::new(2, 4)).is_ok());
    }

    #[test]
    fn fractional_fix_prefers_the_coarsest_integral_unit() {
        let equivalent = |raw: &str, unit: &str| match parse_duration_literal(
            raw,
            unit,
            Span::new(0, 5),
            Span::new(4, 5),
        )
        .unwrap_err()
        {
            NmlError::Duration {
                kind: DurationErrorKind::FractionalMagnitude { equivalent, .. },
                ..
            } => equivalent,
            other => panic!("expected FractionalMagnitude, got {other:?}"),
        };
        assert_eq!(equivalent("30.5", "s").as_deref(), Some("30500ms"));
        assert_eq!(equivalent("1.5", "h").as_deref(), Some("90m"));
        assert_eq!(equivalent("0.25", "h").as_deref(), Some("15m"));
        assert_eq!(equivalent("1.75", "h").as_deref(), Some("105m"));
        assert_eq!(equivalent("0.001", "s").as_deref(), Some("1ms"));
        // Integral value in fractional form: same unit.
        assert_eq!(equivalent("30.0", "s").as_deref(), Some("30s"));
        // No finer unit can make it integral.
        assert_eq!(equivalent("0.5", "ms"), None);
        assert_eq!(equivalent("0.0001", "s"), None);
    }

    #[test]
    fn unknown_unit_carries_the_suffix_subspan_for_the_fix() {
        let err = parse_duration_literal("30", "S", Span::new(10, 13), Span::new(12, 13));
        let Err(NmlError::Duration {
            kind: DurationErrorKind::UnknownUnit { unit, unit_span },
            ..
        }) = err
        else {
            panic!("expected UnknownUnit: {err:?}");
        };
        assert_eq!(unit, "S");
        assert_eq!((unit_span.start, unit_span.end), (12, 13));
    }

    #[test]
    fn wire_shape_is_magnitude_and_suffix() {
        let json = serde_json::to_string(&dur(30, DurationUnit::Seconds)).unwrap();
        assert_eq!(json, r#"{"magnitude":30,"unit":"s"}"#);
    }
}
