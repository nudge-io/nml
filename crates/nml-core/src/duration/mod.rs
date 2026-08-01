//! Exact duration values (RFC 0017, amended for compound literals).
//!
//! Storage is **canonical integer segments** (coarse→fine, no zeros, no
//! duplicates) with the total nanoseconds cached at construction, so
//! comparison is a field read and [`Duration::as_std`] is infallible.
//! Comparison is **semantic** (`30s == 30000ms == 0h30s`-less spellings);
//! rendering is [`Display`](std::fmt::Display), the one spelling
//! normalizer (`1h30m`, attached, coarse→fine).
//!
//! Two entry grammars, one build path: strict source literals
//! ([`parse_components_cst`]) versus Postel coercion text ([`parse_text`]).

mod canonicalize;
mod decompose;
mod parse;

use crate::span::Span;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

pub use parse::{ComponentTokens, is_duration_suffix_shape, parse_components_cst, parse_text};

/// A duration's unit. **This enum is complete by construction — match it
/// exhaustively.** It is closed at both ends by design arguments: below,
/// `ns` is the resolution of the value model itself; above, `h` is the
/// largest *exact* unit — everything coarser (`d`, `w`, `mo`, `y`) is
/// calendar arithmetic, permanently excluded (RFC 0017 §8; `720h` is
/// exact where `30d` is not). No variant will ever be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DurationUnit {
    Hours,
    Minutes,
    Seconds,
    Milliseconds,
    Microseconds,
    Nanoseconds,
}

/// Serializes as the unit's source suffix (`"s"`), matching the literal
/// grammar — the `nml parse` wire form is an API.
impl Serialize for DurationUnit {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.suffix())
    }
}

impl DurationUnit {
    /// Every unit, **coarsest first** — the one ladder every ordered
    /// derivation rides.
    pub const ALL: [DurationUnit; 6] = [
        DurationUnit::Hours,
        DurationUnit::Minutes,
        DurationUnit::Seconds,
        DurationUnit::Milliseconds,
        DurationUnit::Microseconds,
        DurationUnit::Nanoseconds,
    ];

    /// The unit's source suffix (`"h"`, `"m"`, `"s"`, `"ms"`, `"us"`,
    /// `"ns"`). ASCII `us`, never `µs`: the source-character policy
    /// rejects the micro sign raw.
    pub fn suffix(self) -> &'static str {
        match self {
            DurationUnit::Hours => "h",
            DurationUnit::Minutes => "m",
            DurationUnit::Seconds => "s",
            DurationUnit::Milliseconds => "ms",
            DurationUnit::Microseconds => "us",
            DurationUnit::Nanoseconds => "ns",
        }
    }

    /// The unit's human name — the single vocabulary the LSP's completion
    /// details (and any future surface) derive from.
    pub fn name(self) -> &'static str {
        match self {
            DurationUnit::Hours => "hours",
            DurationUnit::Minutes => "minutes",
            DurationUnit::Seconds => "seconds",
            DurationUnit::Milliseconds => "milliseconds",
            DurationUnit::Microseconds => "microseconds",
            DurationUnit::Nanoseconds => "nanoseconds",
        }
    }

    /// Classify a suffix. **The single authority for what counts as a
    /// duration unit** — the literal decoder, the `de` coercion, the
    /// parser's shape predicate, and the LSP's unit completion all route
    /// through here, so the grammar cannot fork. Exact full-string match;
    /// case-sensitive by design (`30S` is a rejection with a fix, never a
    /// case-fold — RFC 0017 §1).
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
            DurationUnit::Microseconds => 1_000,
            DurationUnit::Nanoseconds => 1,
        }
    }

    /// The units strictly finer than this one, coarsest first — the
    /// candidate order for the fractional-magnitude fix and the LSP's
    /// mid-compound unit completion.
    pub fn finer(self) -> &'static [DurationUnit] {
        let i = Self::ALL
            .iter()
            .position(|u| *u == self)
            .expect("ALL is total over the enum by construction");
        &Self::ALL[i + 1..]
    }

    /// The suffixes as teaching prose (`"h, m, s, ms, us, or ns"`) — the
    /// one renderer behind every message that enumerates the unit set.
    pub(crate) fn suffix_list_prose() -> String {
        let mut out = String::new();
        for (i, u) in Self::ALL.iter().enumerate() {
            if i + 1 == Self::ALL.len() {
                out.push_str("or ");
            }
            out.push_str(u.suffix());
            if i + 1 < Self::ALL.len() {
                out.push_str(", ");
            }
        }
        out
    }
}

/// The segment capacity IS the unit count: a canonical duration holds at
/// most one segment per unit, so the bound is structural, not chosen.
pub(crate) const MAX_SEGMENTS: usize = 6;
const _: () = assert!(MAX_SEGMENTS == DurationUnit::ALL.len());

/// `std::time::Duration::MAX` in nanoseconds — the value-domain ceiling.
/// (`u64::MAX` seconds + 999,999,999 ns; far inside `u128`, so total
/// arithmetic can never overflow.)
const STD_MAX_NANOS: u128 = u64::MAX as u128 * 1_000_000_000 + 999_999_999;

/// One integer magnitude paired with a unit — a single component of a
/// (possibly compound) duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct DurationSegment {
    pub magnitude: u64,
    pub unit: DurationUnit,
}

/// Exact duration: canonical integer segments (coarse→fine) with the
/// total nanoseconds cached at the construction gate.
///
/// `PartialEq`/`Eq`/`Hash`/`Ord` are **manual, over the cached total** —
/// deliberately not derived. A derived equality would compare segments,
/// making `90m != 1h30m`; the reload differ would then report a spurious
/// change — the exact defect this type exists to close (RFC 0017 §2).
/// Cosmetic-respelling detection is therefore
/// `a == b && a.segments() != b.segments()`.
#[derive(Debug, Clone, Copy)]
pub struct Duration {
    segments: [DurationSegment; MAX_SEGMENTS],
    len: u8,
    nanos: u128,
}

impl Duration {
    /// The largest magnitude representable in `unit`: the value domain is
    /// `total_nanos() <= std::time::Duration::MAX`, and the magnitude
    /// must also fit the `u64` storage (which binds for `ms`/`us`/`ns`).
    pub fn max_magnitude(unit: DurationUnit) -> u64 {
        u64::try_from(STD_MAX_NANOS / unit.nanos() as u128).unwrap_or(u64::MAX)
    }

    /// Construct a single-segment duration, `None` when `magnitude` is
    /// outside the unit's domain. Zero normalizes to the canonical zero
    /// (`0s`): a value with no nonzero component has no unit to preserve.
    pub fn new(magnitude: u64, unit: DurationUnit) -> Option<Duration> {
        let seg = if magnitude == 0 {
            DurationSegment {
                magnitude: 0,
                unit: DurationUnit::Seconds,
            }
        } else {
            DurationSegment { magnitude, unit }
        };
        Self::from_segments(&[seg])
    }

    /// The construction gate — sole constructor, **strictly canonical**:
    /// 1..=6 segments, strictly coarse→fine (no duplicates), each
    /// magnitude in its unit's domain, no zero segments (except the sole
    /// canonical zero `0s`), total within `std::time::Duration::MAX`.
    /// Every accepted value round-trips `Display` → parse unchanged.
    pub fn from_segments(segments: &[DurationSegment]) -> Option<Duration> {
        if segments.is_empty() || segments.len() > MAX_SEGMENTS {
            return None;
        }
        let is_canonical_zero = segments.len() == 1
            && segments[0].magnitude == 0
            && segments[0].unit == DurationUnit::Seconds;
        let mut storage = [DurationSegment {
            magnitude: 0,
            unit: DurationUnit::Seconds,
        }; MAX_SEGMENTS];
        let mut nanos: u128 = 0;
        let mut prev_rank: Option<u8> = None;
        for (i, seg) in segments.iter().enumerate() {
            if seg.magnitude == 0 && !is_canonical_zero {
                return None;
            }
            if seg.magnitude > Self::max_magnitude(seg.unit) {
                return None;
            }
            let rank = canonicalize::unit_rank(seg.unit);
            if prev_rank.is_some_and(|p| rank <= p) {
                return None;
            }
            prev_rank = Some(rank);
            nanos += seg.magnitude as u128 * seg.unit.nanos() as u128;
            storage[i] = *seg;
        }
        if nanos > STD_MAX_NANOS {
            return None;
        }
        Some(Duration {
            segments: storage,
            len: segments.len() as u8,
            nanos,
        })
    }

    /// The canonical segments, coarse→fine (`[1h, 30m]` for `1h30m`).
    pub fn segments(&self) -> &[DurationSegment] {
        &self.segments[..self.len as usize]
    }

    /// The total in nanoseconds — the semantic comparison basis. A field
    /// read: the gate computed it once.
    pub fn total_nanos(&self) -> u128 {
        self.nanos
    }

    /// Convert to [`std::time::Duration`] — **infallible by
    /// construction**: the gate bounds the total (RFC 0017 §6).
    pub fn as_std(&self) -> std::time::Duration {
        std::time::Duration::new(
            (self.nanos / 1_000_000_000) as u64,
            (self.nanos % 1_000_000_000) as u32,
        )
    }

    /// Parse the coercion **text** grammar (`"30s"`, `"1h30m"`) — see
    /// [`parse::parse_text`].
    pub fn parse_text(text: &str) -> Result<Duration, DurationTextError> {
        parse::parse_text(text)
    }

    /// The total respelled in the coarsest single unit that is exact and
    /// in-domain (`1h30m` → `90m`). `None` when it would echo the authored
    /// spelling or the literal is single-segment.
    pub fn coarsest_exact(&self) -> Option<String> {
        if self.segments().len() <= 1 {
            return None;
        }
        let nanos = self.total_nanos();
        if nanos == 0 {
            return None;
        }
        for unit in DurationUnit::ALL {
            let unit_nanos = unit.nanos() as u128;
            if nanos % unit_nanos != 0 {
                continue;
            }
            let Ok(magnitude) = u64::try_from(nanos / unit_nanos) else {
                continue;
            };
            if let Some(d) = Duration::new(magnitude, unit) {
                let spelling = d.to_string();
                if spelling == self.to_string() {
                    return None;
                }
                return Some(spelling);
            }
        }
        None
    }

    /// The hover/inlay normalized total: the value in the coarsest of
    /// `ms`/`us`/`ns` that divides it exactly, grouped and suffixed.
    /// `None` when that unit is already authored or the value is zero.
    pub fn normalized_total(&self) -> Option<String> {
        let nanos = self.total_nanos();
        if nanos == 0 {
            return None;
        }
        for unit in [
            DurationUnit::Milliseconds,
            DurationUnit::Microseconds,
            DurationUnit::Nanoseconds,
        ] {
            let unit_nanos = unit.nanos() as u128;
            if nanos % unit_nanos != 0 {
                continue;
            }
            if self.segments().iter().any(|s| s.unit == unit) {
                return None;
            }
            return Some(format!(
                "{}{}",
                group_thousands(nanos / unit_nanos),
                unit.suffix()
            ));
        }
        None
    }
}

fn group_thousands(n: u128) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push('_');
        }
        out.push(ch);
    }
    out
}

/// The wire shape (an API, RFC 0017 §6 as amended): an externally visible
/// `segments` array of `{magnitude, unit}` pairs, canonical order.
impl Serialize for Duration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut st = serializer.serialize_struct("Duration", 1)?;
        st.serialize_field("segments", self.segments())?;
        st.end()
    }
}

/// Canonical source form: each magnitude attached to its suffix, segments
/// coarse→fine, no separators (`1h30m`) — the shape `fmt` emits and the
/// spec names canonical.
impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for seg in self.segments() {
            write!(f, "{}{}", seg.magnitude, seg.unit.suffix())?;
        }
        Ok(())
    }
}

// Semantic equality/ordering/hashing over the cached nanosecond total —
// see the type-level doc for why these are hand-written.
impl PartialEq for Duration {
    fn eq(&self, other: &Self) -> bool {
        self.nanos == other.nanos
    }
}
impl Eq for Duration {}

impl Ord for Duration {
    fn cmp(&self, other: &Self) -> Ordering {
        self.nanos.cmp(&other.nanos)
    }
}
impl PartialOrd for Duration {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Coherent with the semantic `Eq` (`hash(90m) == hash(1h30m)`), making
/// `Duration` a valid map key.
impl Hash for Duration {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.nanos.hash(state);
    }
}

/// The coercion text grammar, as `str::parse` — `"1h30m".parse::<Duration>()`.
impl std::str::FromStr for Duration {
    type Err = DurationTextError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Duration::parse_text(s)
    }
}

/// Why a coerced duration **text** failed ([`Duration::parse_text`]).
/// `Display` states the reason and never the input — the never-echo rule
/// of `de`'s coercion family (fuzz-enforced closed message set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationTextError {
    /// Not `components + units` (`h`, `m`, `s`, `ms`, `us`, `ns`).
    Malformed,
    /// Well-formed, but a magnitude (or merged total) exceeds the domain.
    OutOfRange(DurationUnit),
}

impl fmt::Display for DurationTextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DurationTextError::Malformed => write!(
                f,
                "not a duration: expected an integer with unit {}",
                DurationUnit::suffix_list_prose()
            ),
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
/// structured: message, stable code, and any machine-applicable fix all
/// derive from the payload, never from message text.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DurationErrorKind {
    /// The trailing identifier is neither a currency code nor a duration
    /// unit (`30x`, `30S`, `30sec`). `unit_span` is the suffix token's
    /// own span, so the nearest-unit did-you-mean is machine-applicable.
    UnknownUnit { unit: String, unit_span: Span },
    /// A magnitude is not an integer (`30.5s`). `equivalent` is the same
    /// value respelled to make it integral — present only when the
    /// whole-literal replacement is value-preserving (i.e. the literal
    /// has a single component).
    FractionalMagnitude {
        raw: String,
        equivalent: Option<String>,
    },
    /// Outside the value domain: negative, or a magnitude/total beyond
    /// `std::time::Duration::MAX` (the check that makes
    /// [`Duration::as_std`] infallible).
    OutOfRange { unit: DurationUnit, negative: bool },
    /// The same unit appears more than once in one authored literal
    /// (`1h2h`). `merged` is the whole literal's canonical merged form
    /// (`3h`) — value-preserving by construction, machine-applicable.
    /// Coercion text merges silently instead (Postel).
    DuplicateUnit { merged: String },
    /// A compound literal has a magnitude with no unit suffix (`1h30`).
    /// `break_span` is the dangling magnitude's own span. No machine fix:
    /// deleting or completing the magnitude would change the value.
    MalformedCompound { break_span: Span },
}

impl DurationErrorKind {
    /// The human-facing message, derived from the payload.
    pub fn message(&self) -> String {
        use crate::error::echo;
        match self {
            DurationErrorKind::UnknownUnit { unit, .. } => format!(
                "unknown unit `{}` after a number: a duration unit is {}; \
                 a currency code is 3 uppercase letters",
                echo(unit),
                DurationUnit::suffix_list_prose()
            ),
            DurationErrorKind::FractionalMagnitude { raw, .. } => format!(
                "a duration magnitude is a whole number, but \"{}\" has a fractional \
                 part — write the value as a compound literal (`1h30m`) or in a \
                 finer unit",
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
            DurationErrorKind::DuplicateUnit { merged } => format!(
                "a duration literal spells each unit at most once — merged, \
                 this value is `{}`",
                echo(merged)
            ),
            DurationErrorKind::MalformedCompound { .. } => format!(
                "a compound duration needs a unit ({}) after every magnitude",
                DurationUnit::suffix_list_prose()
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
            DurationErrorKind::DuplicateUnit { .. } => codes::DUPLICATE_DURATION_UNIT,
            DurationErrorKind::MalformedCompound { .. } => codes::MALFORMED_COMPOUND_DURATION,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dur(magnitude: u64, unit: DurationUnit) -> Duration {
        Duration::new(magnitude, unit).expect("in-domain test value")
    }

    fn seg(magnitude: u64, unit: DurationUnit) -> DurationSegment {
        DurationSegment { magnitude, unit }
    }

    #[test]
    fn equality_is_semantic_across_units_and_spellings() {
        assert_eq!(
            dur(30, DurationUnit::Seconds),
            dur(30_000, DurationUnit::Milliseconds)
        );
        assert_eq!(dur(1, DurationUnit::Hours), dur(60, DurationUnit::Minutes));
        assert_eq!(
            dur(90, DurationUnit::Minutes),
            Duration::parse_text("1h30m").unwrap()
        );
        assert_ne!(
            dur(90, DurationUnit::Minutes),
            Duration::parse_text("1h30m1s").unwrap()
        );
        // Zero is one value across every spelling.
        assert_eq!(
            dur(0, DurationUnit::Milliseconds),
            dur(0, DurationUnit::Hours)
        );
    }

    #[test]
    fn the_gate_is_strictly_canonical() {
        // Canonical forms pass.
        assert!(Duration::from_segments(&[seg(1, DurationUnit::Hours)]).is_some());
        assert!(
            Duration::from_segments(&[seg(1, DurationUnit::Hours), seg(30, DurationUnit::Minutes)])
                .is_some()
        );
        assert!(Duration::from_segments(&[seg(0, DurationUnit::Seconds)]).is_some());
        // Empty, unordered, duplicated, zero-in-compound, non-second sole
        // zero, and out-of-domain forms are all rejected.
        assert!(Duration::from_segments(&[]).is_none());
        assert!(
            Duration::from_segments(&[seg(30, DurationUnit::Minutes), seg(1, DurationUnit::Hours)])
                .is_none()
        );
        assert!(
            Duration::from_segments(&[seg(1, DurationUnit::Hours), seg(2, DurationUnit::Hours)])
                .is_none()
        );
        assert!(
            Duration::from_segments(&[seg(0, DurationUnit::Hours), seg(5, DurationUnit::Minutes)])
                .is_none()
        );
        assert!(Duration::from_segments(&[seg(0, DurationUnit::Milliseconds)]).is_none());
        assert!(
            Duration::from_segments(&[seg(u64::MAX, DurationUnit::Hours)]).is_none(),
            "per-segment domain"
        );
        // Total past the std ceiling rejects even when each segment fits.
        assert!(
            Duration::from_segments(&[
                seg(
                    Duration::max_magnitude(DurationUnit::Hours),
                    DurationUnit::Hours
                ),
                seg(59, DurationUnit::Minutes),
            ])
            .is_none()
        );
    }

    #[test]
    fn zero_normalizes_to_the_canonical_zero() {
        assert_eq!(dur(0, DurationUnit::Milliseconds).to_string(), "0s");
        assert_eq!(
            dur(0, DurationUnit::Milliseconds).segments(),
            &[seg(0, DurationUnit::Seconds)]
        );
    }

    #[test]
    fn display_is_the_canonical_attached_form() {
        assert_eq!(dur(72, DurationUnit::Hours).to_string(), "72h");
        assert_eq!(Duration::parse_text("1h 30m").unwrap().to_string(), "1h30m");
        assert_eq!(dur(0, DurationUnit::Seconds).to_string(), "0s");
    }

    #[test]
    fn display_reparses_to_the_same_value() {
        // parse ∘ fmt is the identity on values (the fuzz invariant, pinned).
        for text in ["72h", "1h30m", "5m2s", "1h30m45s500ms250us100ns", "0s"] {
            let d = Duration::parse_text(text).unwrap();
            assert_eq!(Duration::parse_text(&d.to_string()).unwrap(), d, "{text}");
        }
    }

    #[test]
    fn ordering_and_hash_laws_hold_across_spellings() {
        use std::hash::{DefaultHasher, Hasher as _};
        let hash_of = |d: &Duration| {
            let mut h = DefaultHasher::new();
            d.hash(&mut h);
            h.finish()
        };
        let grid: Vec<Duration> = [
            "0s", "1ns", "999ns", "1us", "1ms", "1s", "59s", "1m", "90m", "1h30m", "1h30m1s", "2h",
            "72h",
        ]
        .iter()
        .map(|t| Duration::parse_text(t).unwrap())
        .collect();
        for a in &grid {
            for b in &grid {
                assert_eq!(a.cmp(b), a.total_nanos().cmp(&b.total_nanos()));
                assert_eq!(*a == *b, a.cmp(b) == Ordering::Equal, "{a} vs {b}");
                if a == b {
                    assert_eq!(hash_of(a), hash_of(b), "{a} vs {b}");
                }
            }
        }
        // The compound and its single-unit respelling are one value.
        assert_eq!(
            hash_of(&Duration::parse_text("90m").unwrap()),
            hash_of(&Duration::parse_text("1h30m").unwrap())
        );
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
        assert_eq!(Duration::max_magnitude(DurationUnit::Seconds), u64::MAX);
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
            Duration::parse_text("1h30m").unwrap().as_std(),
            std::time::Duration::from_secs(90 * 60)
        );
    }

    #[test]
    fn wire_shape_is_the_segments_array_pinned_exactly() {
        let single = serde_json::to_string(&dur(30, DurationUnit::Seconds)).unwrap();
        assert_eq!(single, r#"{"segments":[{"magnitude":30,"unit":"s"}]}"#);
        let compound = serde_json::to_string(&Duration::parse_text("1h30m").unwrap()).unwrap();
        assert_eq!(
            compound,
            r#"{"segments":[{"magnitude":1,"unit":"h"},{"magnitude":30,"unit":"m"}]}"#
        );
    }
}
