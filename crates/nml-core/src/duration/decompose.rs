//! Fractional magnitude decomposition via exact `decimal::Number`
//! arithmetic — never floats, never truncation, never unchecked overflow.

use super::{Duration, DurationSegment, DurationUnit};
use crate::decimal::Number;

/// Decompose a fractional magnitude into exact integer segments, coarse→fine
/// (`1.5` hours → `[1h, 30m]`). `None` when the value has no exact
/// representation on the unit ladder (`0.5ns`) or exceeds `u128` working
/// range. All arithmetic is checked: the coefficient can carry 34 digits
/// and an hour is 3.6×10¹² ns, so the intermediate products genuinely can
/// exceed `u128` on coercion input — overflow must be a rejection, not a
/// wrap (release) or a panic (debug).
pub(crate) fn decompose_fractional(n: Number, unit: DurationUnit) -> Option<Vec<DurationSegment>> {
    let coeff = u128::try_from(n.coeff()).ok()?;
    let scale = u32::try_from(n.scale()).ok()?;
    let pow10 = 10u128.checked_pow(scale)?;
    // The value in nanoseconds, scaled by pow10 (kept scaled so division
    // remainders stay exact integers).
    let mut remainder = coeff.checked_mul(unit.nanos() as u128)?;

    let mut segments = Vec::new();
    for u in DurationUnit::ALL.iter() {
        if u.nanos() > unit.nanos() {
            continue;
        }
        let step = pow10.checked_mul(u.nanos() as u128)?;
        let whole = remainder / step;
        if whole > 0 {
            let magnitude = u64::try_from(whole).ok()?;
            segments.push(DurationSegment {
                magnitude,
                unit: *u,
            });
            // whole * step <= remainder by construction — cannot overflow.
            remainder -= whole * step;
        }
    }

    if remainder != 0 {
        return None;
    }
    Some(segments)
}

/// The machine-applicable respelling for a fractional magnitude,
/// **granularity-preserving**: the in-ladder decomposition keeps the
/// author at their chosen scale — `1.5h` → `1h30m` (not `90m`), `30.5s`
/// → `30s500ms`, `0.25h` → `15m`, `30.0s` → `30s`. Falls back to the
/// coarsest single integral finer unit for extreme magnitudes where the
/// decomposition's u128 working products overflow but a single unit
/// still fits. `None` when the value has no exact spelling at all
/// (`0.5ns`).
pub(crate) fn fractional_respelling(n: Number, unit: DurationUnit) -> Option<String> {
    if let Some(magnitude) = n.to_u64() {
        // Integral value in fractional form (`30.0s`) — same unit.
        return Duration::new(magnitude, unit).map(|d| d.to_string());
    }
    if let Some(d) = decompose_fractional(n, unit).and_then(|segs| Duration::from_segments(&segs)) {
        return Some(d.to_string());
    }
    let coeff = u128::try_from(n.coeff()).ok()?;
    let pow10 = 10u128.checked_pow(u32::try_from(n.scale()).ok()?)?;
    for finer in unit.finer() {
        let factor = (unit.nanos() / finer.nanos()) as u128;
        let Some(scaled) = coeff.checked_mul(factor) else {
            break;
        };
        if scaled % pow10 != 0 {
            continue;
        }
        let Ok(magnitude) = u64::try_from(scaled / pow10) else {
            continue;
        };
        if let Some(d) = Duration::new(magnitude, *finer) {
            return Some(d.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(text: &str) -> Number {
        Number::parse_literal(text).expect("test literal")
    }

    #[test]
    fn decomposes_exact_fractions() {
        let segs = decompose_fractional(num("1.5"), DurationUnit::Hours).unwrap();
        assert_eq!(
            segs,
            vec![
                DurationSegment {
                    magnitude: 1,
                    unit: DurationUnit::Hours
                },
                DurationSegment {
                    magnitude: 30,
                    unit: DurationUnit::Minutes
                },
            ]
        );
        // Sub-millisecond exactness rides the full ladder.
        let segs = decompose_fractional(num("0.5"), DurationUnit::Milliseconds).unwrap();
        assert_eq!(
            segs,
            vec![DurationSegment {
                magnitude: 500,
                unit: DurationUnit::Microseconds
            }]
        );
    }

    #[test]
    fn inexact_fraction_has_no_decomposition() {
        assert_eq!(
            decompose_fractional(num("0.5"), DurationUnit::Nanoseconds),
            None
        );
        assert_eq!(
            decompose_fractional(num("0.0000000001"), DurationUnit::Seconds),
            None
        );
    }

    #[test]
    fn zero_fraction_decomposes_to_no_segments() {
        assert_eq!(
            decompose_fractional(num("0.0"), DurationUnit::Hours),
            Some(vec![])
        );
    }

    #[test]
    fn extreme_magnitudes_reject_without_panicking() {
        // 34-digit coefficient × 3.6e12 ns/h overflows u128 — must be a
        // clean None on both the decompose and respelling paths.
        let huge = num("9999999999999999999999999999999999");
        assert_eq!(decompose_fractional(huge, DurationUnit::Hours), None);
        // Extreme scale: pow10 alone stays in u128 (checked_pow), but the
        // per-unit step product must also be checked.
        let tiny = num("0.00000000000000000000000000000000001");
        assert_eq!(decompose_fractional(tiny, DurationUnit::Hours), None);
        assert_eq!(fractional_respelling(tiny, DurationUnit::Hours), None);
    }

    #[test]
    fn respelling_preserves_the_authored_granularity() {
        // The fix reads like what a person at that scale would write:
        // the in-ladder decomposition, never a unit-flattened total
        // (`1h30m`, not `90m`; `30s500ms`, not `30500ms`).
        let case = |text: &str, unit| fractional_respelling(num(text), unit);
        assert_eq!(case("1.5", DurationUnit::Hours).as_deref(), Some("1h30m"));
        assert_eq!(
            case("30.5", DurationUnit::Seconds).as_deref(),
            Some("30s500ms")
        );
        assert_eq!(
            case("2.25", DurationUnit::Minutes).as_deref(),
            Some("2m15s")
        );
        // No whole part: the decomposition IS the single finer unit.
        assert_eq!(case("0.25", DurationUnit::Hours).as_deref(), Some("15m"));
        assert_eq!(case("0.001", DurationUnit::Seconds).as_deref(), Some("1ms"));
        assert_eq!(
            case("0.5", DurationUnit::Milliseconds).as_deref(),
            Some("500us")
        );
        assert_eq!(
            case("0.5", DurationUnit::Microseconds).as_deref(),
            Some("500ns")
        );
        // Integral value in fractional form: same unit.
        assert_eq!(case("30.0", DurationUnit::Seconds).as_deref(), Some("30s"));
        // ns is the resolution floor: nothing finer exists.
        assert_eq!(case("0.5", DurationUnit::Nanoseconds), None);
        assert_eq!(case("0.0000000001", DurationUnit::Seconds), None);
    }

    #[test]
    fn respelling_handles_extreme_magnitudes_exactly() {
        // 2×10¹⁶ + 0.5 seconds: every single finer unit overflows u64
        // (2×10¹⁹ ms and beyond); the decomposition is exact and
        // in-domain.
        let v = num("20000000000000000.5");
        let respelled = fractional_respelling(v, DurationUnit::Seconds).expect("compound exists");
        assert_eq!(respelled, "20000000000000000s500ms");
        let reparsed = Duration::parse_text(&respelled).expect("respelling parses");
        assert_eq!(
            reparsed.total_nanos(),
            20_000_000_000_000_000_500_000_000u128
        );
    }

    #[test]
    fn decompose_never_rescales_coarser_than_the_authored_unit() {
        // `90.5m` keeps the author's minute granularity: `[90m, 30s]`,
        // never `[1h, 30m, 30s]` — the same no-carry rule that keeps
        // `60m` from becoming `1h` (RFC 0017 §2, faithful storage).
        let segs = decompose_fractional(num("90.5"), DurationUnit::Minutes).unwrap();
        assert_eq!(
            segs,
            vec![
                DurationSegment {
                    magnitude: 90,
                    unit: DurationUnit::Minutes
                },
                DurationSegment {
                    magnitude: 30,
                    unit: DurationUnit::Seconds
                },
            ]
        );
        // Consequence at the extreme: a fractional second count whose
        // whole part exceeds u64 has NO spelling (rolling up to hours
        // would change granularity) — a clean rejection, never a panic.
        let v = num("18446744073709551616.5");
        assert_eq!(fractional_respelling(v, DurationUnit::Seconds), None);
    }
}
