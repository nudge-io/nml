//! The two duration grammars, one build path (RFC 0017 amendment).
//!
//! **Source literals** (`parse_components_cst`) are strict: duplicate
//! units are the NML3007 teaching diagnostic (with the merged fix),
//! fractions are NML3005 — authored source gets a diagnostic instead of
//! a guess. **Coercion text** (`parse_text` — `$ENV` resolution, quoted
//! wire values) is Postel for machine-emitted data: duplicate units merge
//! silently and exact fractions decompose (`"1.5h"` → `1h30m`), the
//! Go-superset posture.
//!
//! Both delegate to [`build_from_raw`], so the value semantics cannot
//! fork. Coercion errors carry a *reason* and **never the input** — any
//! coerced string could be a resolved secret.

use super::canonicalize::canonicalize;
use super::decompose::{decompose_fractional, fractional_respelling};
use super::{
    Duration, DurationErrorKind, DurationSegment, DurationTextError, DurationUnit, MAX_SEGMENTS,
};
use crate::decimal::Number;
use crate::error::NmlError;
use crate::span::Span;

/// Which grammar's judgment applies to duplicate units and fractions.
enum Policy {
    /// Authored source: diagnose, never guess.
    Strict,
    /// Machine-emitted text: merge duplicates, decompose exact fractions.
    Coercion,
}

/// One parsed component before normalization: the exact authored
/// magnitude (possibly fractional — the policy decides) and its unit.
struct RawComponent {
    number: Number,
    unit: DurationUnit,
}

/// One component's tokens from the CST: magnitude text (with any authored
/// sign on the first component), unit suffix text, and both spans.
#[derive(Debug, Clone, Copy)]
pub struct ComponentTokens<'a> {
    pub magnitude: &'a str,
    pub unit: &'a str,
    pub magnitude_span: Span,
    pub unit_span: Span,
}

/// Parse a duration from coercion **text** (`"30s"`, `"1h30m"`, `"1h 30m"`).
///
/// The never-echo rule holds: both error variants render fixed prose
/// derived from the kind and unit alone (fuzz-enforced closed set).
pub fn parse_text(text: &str) -> Result<Duration, DurationTextError> {
    let components = parse_text_components(text).ok_or(DurationTextError::Malformed)?;
    build_from_raw(&components, Policy::Coercion, Span::empty(0)).map_err(|e| match e {
        NmlError::Duration {
            kind: DurationErrorKind::OutOfRange { unit, .. },
            ..
        } => DurationTextError::OutOfRange(unit),
        _ => DurationTextError::Malformed,
    })
}

fn parse_text_components(text: &str) -> Option<Vec<RawComponent>> {
    let trimmed = text.trim_ascii();
    let mut components = Vec::new();
    let mut rest = trimmed;
    while !rest.is_empty() {
        rest = rest.trim_ascii_start();
        if rest.is_empty() {
            break;
        }
        let split = rest
            .bytes()
            .position(|b| !b.is_ascii_digit() && b != b'_' && b != b'.')
            .unwrap_or(rest.len());
        if split == 0 {
            return None;
        }
        let (magnitude_text, after_magnitude) = rest.split_at(split);
        rest = after_magnitude.trim_ascii_start();
        let unit_len = rest.bytes().take_while(|b| b.is_ascii_alphabetic()).count();
        if unit_len == 0 {
            return None;
        }
        let (unit_text, after_unit) = rest.split_at(unit_len);
        rest = after_unit;
        let unit = DurationUnit::from_suffix(unit_text)?;
        let number = Number::parse_literal(magnitude_text).ok()?;
        components.push(RawComponent { number, unit });
    }
    if components.is_empty() {
        return None;
    }
    Some(components)
}

/// Decode a duration **literal** from its CST component tokens — the
/// strict source grammar. Rejection order per component: unknown unit,
/// sign, integrality, domain (the most fundamental defect first).
pub fn parse_components_cst(
    components: &[ComponentTokens<'_>],
    literal_span: Span,
) -> Result<Duration, NmlError> {
    let err = |kind: DurationErrorKind| NmlError::Duration {
        kind,
        span: literal_span,
    };
    let compound = components.len() > 1;
    let mut raw = Vec::with_capacity(components.len());
    for c in components {
        let Some(unit) = DurationUnit::from_suffix(c.unit) else {
            return Err(err(DurationErrorKind::UnknownUnit {
                unit: crate::error::echo_capture(c.unit),
                unit_span: c.unit_span,
            }));
        };
        let (negative, digits) = match c.magnitude.strip_prefix('-') {
            Some(digits) => (true, digits),
            None => (false, c.magnitude),
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
        let number = Number::parse_literal(digits).map_err(|e| match e {
            // The lexer only emits digit-shaped Number tokens, so only the
            // range kinds are reachable — a >34-digit magnitude is
            // truthfully out of the duration domain.
            crate::decimal::NumberError::Range(_) => out_of_range(),
            // A misplaced separator (`1__0s`) is the literal-layer
            // teaching diagnostic. The strip fix replaces the WHOLE
            // literal, so it is only machine-applicable when this
            // component IS the whole literal — in a compound, a
            // single-component replacement would delete its siblings.
            crate::decimal::NumberError::BadSeparator => NmlError::syntax(
                crate::error::ParseErrorKind::NumberBadSeparator {
                    raw: crate::error::echo_capture(digits),
                    stripped: if compound {
                        None
                    } else {
                        crate::error::strip_separators_fix(digits)
                            .map(|d| format!("{d}{}", unit.suffix()))
                    },
                },
                literal_span,
            ),
            // A trailing dot gets its own teaching diagnostic with the
            // delete-dot fix, anchored on THIS component's magnitude end
            // (the fix machinery deletes the span's final byte — the dot
            // — which is value-preserving in place, compound or not).
            crate::decimal::NumberError::TrailingDot => NmlError::syntax(
                crate::error::ParseErrorKind::NumberTrailingDot {
                    raw: crate::error::echo_capture(digits),
                },
                Span::new(literal_span.start, c.magnitude_span.end),
            ),
            crate::decimal::NumberError::Malformed => NmlError::syntax(
                crate::error::ParseErrorKind::InvalidNumber {
                    raw: crate::error::echo_capture(digits),
                },
                literal_span,
            ),
        })?;
        if number.scale() > 0 {
            return Err(err(DurationErrorKind::FractionalMagnitude {
                raw: crate::error::echo_capture(digits),
                // The respelling replaces the WHOLE literal — value-
                // preserving only when this component is the whole
                // literal. In a compound, replacing `1.5h30m` with
                // `1h30m` (the component's respelling) would silently
                // drop the `30m` while looking plausible.
                equivalent: if compound {
                    None
                } else {
                    fractional_respelling(number, unit)
                },
            }));
        }
        raw.push(RawComponent { number, unit });
    }
    build_from_raw(&raw, Policy::Strict, literal_span)
}

/// The one value-construction path both grammars share.
fn build_from_raw(
    raw: &[RawComponent],
    policy: Policy,
    literal_span: Span,
) -> Result<Duration, NmlError> {
    let err = |kind: DurationErrorKind| NmlError::Duration {
        kind,
        span: literal_span,
    };
    if raw.is_empty() {
        return Err(err(DurationErrorKind::MalformedCompound {
            break_span: literal_span,
        }));
    }

    if matches!(policy, Policy::Strict) {
        let mut seen = [false; MAX_SEGMENTS];
        for rc in raw {
            let idx = usize::from(super::canonicalize::unit_rank(rc.unit));
            if seen[idx] {
                // The fix is the whole literal's merged canonical form —
                // built through the coercion path, so it is value-
                // preserving by construction (or the more fundamental
                // out-of-range error propagates instead).
                let merged = build_from_raw(raw, Policy::Coercion, literal_span)?;
                return Err(err(DurationErrorKind::DuplicateUnit {
                    merged: merged.to_string(),
                }));
            }
            seen[idx] = true;
        }
    }

    let mut expanded: Vec<DurationSegment> = Vec::new();
    for rc in raw {
        if rc.number.scale() > 0 {
            if matches!(policy, Policy::Coercion) {
                if let Some(segments) = decompose_fractional(rc.number, rc.unit) {
                    expanded.extend(segments);
                    continue;
                }
            }
            return Err(err(DurationErrorKind::FractionalMagnitude {
                raw: rc.number.to_string(),
                equivalent: fractional_respelling(rc.number, rc.unit),
            }));
        }
        // An integral magnitude beyond u64 is truthfully out of the unit's
        // domain — NEVER a silent zero (a truncated magnitude would be a
        // wrong value accepted, the worst possible outcome for a timeout).
        let Some(magnitude) = rc.number.to_u64() else {
            return Err(err(DurationErrorKind::OutOfRange {
                unit: rc.unit,
                negative: false,
            }));
        };
        if magnitude > Duration::max_magnitude(rc.unit) {
            return Err(err(DurationErrorKind::OutOfRange {
                unit: rc.unit,
                negative: false,
            }));
        }
        expanded.push(DurationSegment {
            magnitude,
            unit: rc.unit,
        });
    }

    let merge = matches!(policy, Policy::Coercion);
    let canonical = canonicalize(&expanded, merge).map_err(|unit| {
        err(DurationErrorKind::OutOfRange {
            unit,
            negative: false,
        })
    })?;
    Duration::from_segments(&canonical).ok_or_else(|| {
        err(DurationErrorKind::OutOfRange {
            unit: canonical.first().map_or(DurationUnit::Seconds, |s| s.unit),
            negative: false,
        })
    })
}

/// True when `text` is a duration unit suffix — the parser's shape
/// predicate, delegating to the one classification authority
/// ([`DurationUnit::from_suffix`]) so the grammar cannot fork.
pub fn is_duration_suffix_shape(text: &str) -> bool {
    DurationUnit::from_suffix(text).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::codes;

    fn span() -> Span {
        Span::new(0, 10)
    }

    fn tokens<'a>(pairs: &[(&'a str, &'a str)]) -> Vec<ComponentTokens<'a>> {
        pairs
            .iter()
            .enumerate()
            .map(|(i, (magnitude, unit))| ComponentTokens {
                magnitude,
                unit,
                magnitude_span: Span::new(i * 4, i * 4 + 2),
                unit_span: Span::new(i * 4 + 2, i * 4 + 3),
            })
            .collect()
    }

    fn decode(pairs: &[(&str, &str)]) -> Result<Duration, NmlError> {
        parse_components_cst(&tokens(pairs), span())
    }

    fn kind_of(pairs: &[(&str, &str)]) -> DurationErrorKind {
        match decode(pairs).unwrap_err() {
            NmlError::Duration { kind, .. } => kind,
            other => panic!("expected a duration error, got {other:?}"),
        }
    }

    #[test]
    fn compound_literal_decodes_in_canonical_order() {
        let d = decode(&[("1", "h"), ("30", "m")]).unwrap();
        assert_eq!(d.to_string(), "1h30m");
        assert_eq!(d.total_nanos(), 90 * 60 * 1_000_000_000);
        // Any authored order; the decoded value is canonical coarse→fine.
        let d = decode(&[("30", "m"), ("1", "h")]).unwrap();
        assert_eq!(d.to_string(), "1h30m");
        // Zero components are spelling, dropped exactly.
        let d = decode(&[("1", "h"), ("0", "s")]).unwrap();
        assert_eq!(d.to_string(), "1h");
    }

    #[test]
    fn literal_rejection_order_and_codes() {
        // Unknown unit before sign: `-30x` classifies the suffix first.
        assert!(matches!(
            kind_of(&[("-30", "x")]),
            DurationErrorKind::UnknownUnit { .. }
        ));
        assert_eq!(kind_of(&[("30", "sec")]).code(), codes::UNKNOWN_UNIT);
        // Sign before integrality: `-30.5s` is the negativity error.
        assert!(matches!(
            kind_of(&[("-30.5", "s")]),
            DurationErrorKind::OutOfRange { negative: true, .. }
        ));
        assert_eq!(kind_of(&[("30.5", "s")]).code(), codes::FRACTIONAL_DURATION);
        assert_eq!(
            kind_of(&[("-30", "s")]).code(),
            codes::DURATION_OUT_OF_RANGE
        );
        // The RFC's 23-digit example: a valid Number, not a valid duration.
        assert_eq!(
            kind_of(&[("12345678901234567890123", "s")]).code(),
            codes::DURATION_OUT_OF_RANGE
        );
        // >34 digits: still the duration-domain error, never a panic.
        assert_eq!(
            kind_of(&[(&"9".repeat(40), "s")]).code(),
            codes::DURATION_OUT_OF_RANGE
        );
    }

    #[test]
    fn duplicate_unit_in_source_is_nml3007_with_the_merged_fix() {
        let kind = kind_of(&[("1", "h"), ("2", "h")]);
        assert_eq!(kind.code(), codes::DUPLICATE_DURATION_UNIT);
        match kind {
            DurationErrorKind::DuplicateUnit { merged } => assert_eq!(merged, "3h"),
            other => panic!("expected DuplicateUnit, got {other:?}"),
        }
        // The merged fix covers the WHOLE literal, other units included.
        match kind_of(&[("1", "h"), ("2", "h"), ("30", "m")]) {
            DurationErrorKind::DuplicateUnit { merged } => assert_eq!(merged, "3h30m"),
            other => panic!("expected DuplicateUnit, got {other:?}"),
        }
    }

    #[test]
    fn fractional_component_in_a_compound_has_no_machine_fix() {
        // `1.5h30m`: a whole-literal replacement with the component's
        // respelling (`1h30m`) would silently DROP the `30m` — and look
        // plausible doing it. The fix must be absent; the teaching
        // message alone stands.
        match kind_of(&[("1.5", "h"), ("30", "m")]) {
            DurationErrorKind::FractionalMagnitude { equivalent, .. } => {
                assert_eq!(equivalent, None);
            }
            other => panic!("expected FractionalMagnitude, got {other:?}"),
        }
        // Sole component: the granularity-preserving fix is present.
        match kind_of(&[("1.5", "h")]) {
            DurationErrorKind::FractionalMagnitude { equivalent, .. } => {
                assert_eq!(equivalent.as_deref(), Some("1h30m"));
            }
            other => panic!("expected FractionalMagnitude, got {other:?}"),
        }
    }

    #[test]
    fn bad_separator_in_a_compound_suppresses_the_whole_literal_fix() {
        let err = decode(&[("1__0", "s"), ("30", "m")]).unwrap_err();
        match err {
            NmlError::Syntax {
                kind: crate::error::ParseErrorKind::NumberBadSeparator { stripped, .. },
                ..
            } => assert_eq!(stripped, None, "a component-only fix would drop the 30m"),
            other => panic!("expected NumberBadSeparator, got {other:?}"),
        }
        // Sole component keeps the machine fix.
        let err = decode(&[("1__0", "s")]).unwrap_err();
        match err {
            NmlError::Syntax {
                kind: crate::error::ParseErrorKind::NumberBadSeparator { stripped, .. },
                ..
            } => assert_eq!(stripped.as_deref(), Some("10s")),
            other => panic!("expected NumberBadSeparator, got {other:?}"),
        }
    }

    #[test]
    fn unknown_unit_carries_the_suffix_subspan_for_the_fix() {
        let components = [ComponentTokens {
            magnitude: "30",
            unit: "S",
            magnitude_span: Span::new(10, 12),
            unit_span: Span::new(12, 13),
        }];
        let err = parse_components_cst(&components, Span::new(10, 13)).unwrap_err();
        let NmlError::Duration {
            kind: DurationErrorKind::UnknownUnit { unit, unit_span },
            ..
        } = err
        else {
            panic!("expected UnknownUnit: {err:?}");
        };
        assert_eq!(unit, "S");
        assert_eq!((unit_span.start, unit_span.end), (12, 13));
    }

    #[test]
    fn parse_text_accepts_the_coercion_grammar() {
        let show = |s: &str| Duration::parse_text(s).map(|d| d.to_string());
        assert_eq!(show("30s").as_deref(), Ok("30s"));
        assert_eq!(
            "30s".parse::<Duration>().map(|d| d.to_string()).as_deref(),
            Ok("30s")
        );
        assert_eq!(show(" 500 ms ").as_deref(), Ok("500ms"));
        assert_eq!(show("1_000_000ns").as_deref(), Ok("1000000ns"));
        assert_eq!(show("0ms").as_deref(), Ok("0s"), "the canonical zero");
        // Compound, spaced, and unordered forms.
        assert_eq!(show("1h30m").as_deref(), Ok("1h30m"));
        assert_eq!(show("1h 30m").as_deref(), Ok("1h30m"));
        assert_eq!(show("30m1h").as_deref(), Ok("1h30m"));
        // Postel: duplicates merge, exact fractions decompose.
        assert_eq!(show("1h2h").as_deref(), Ok("3h"));
        assert_eq!(show("1.5h").as_deref(), Ok("1h30m"));
        assert_eq!(show("0.5ms").as_deref(), Ok("500us"));
        assert_eq!(show("3.5s").as_deref(), Ok("3s500ms"));
    }

    #[test]
    fn parse_text_rejects_malformed_input() {
        for bad in [
            "", "30", "s", "30x", "30S", "-30s", "30 sec", "1h 30", "h30m", "1h30m!", "1__0s",
            "_1s", "1_s", "0.5ns", "1.h",
        ] {
            assert_eq!(
                Duration::parse_text(bad),
                Err(DurationTextError::Malformed),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn parse_text_out_of_range_is_an_error_never_a_silent_zero() {
        // REGRESSION: a >u64 integral magnitude once truncated to 0 via
        // `to_u64().unwrap_or(0)` and the zero-drop then yielded Ok(0s) —
        // an out-of-range timeout silently became "no timeout".
        let over = "99999999999999999999999s";
        assert_eq!(
            Duration::parse_text(over),
            Err(DurationTextError::OutOfRange(DurationUnit::Seconds)),
            "huge magnitude must reject, not coerce to zero"
        );
        let over_hours = format!(
            "{}h",
            Duration::max_magnitude(DurationUnit::Hours) as u128 + 1
        );
        assert_eq!(
            Duration::parse_text(&over_hours),
            Err(DurationTextError::OutOfRange(DurationUnit::Hours))
        );
        // Duplicate-merge overflow is a rejection, never a saturation.
        assert_eq!(
            Duration::parse_text("18446744073709551615ns 1ns"),
            Err(DurationTextError::OutOfRange(DurationUnit::Nanoseconds))
        );
        // Compound total past the std ceiling rejects at the gate.
        let max_h = Duration::max_magnitude(DurationUnit::Hours);
        assert!(Duration::parse_text(&format!("{max_h}h59m")).is_err());
    }

    #[test]
    fn parse_text_extreme_inputs_never_panic() {
        // Fraction arithmetic hits u128 ceilings on adversarial input —
        // every product is checked, so these are clean rejections.
        for s in [
            "9999999999999999999999999999999999.9h",
            "0.00000000000000000000000000000000001h",
            &format!("{}.5h", "9".repeat(33)),
        ] {
            assert!(Duration::parse_text(s).is_err(), "{s:?}");
        }
    }

    #[test]
    fn parse_text_errors_never_echo_the_input() {
        // The input may be a resolved secret — the reason must stand alone.
        for secret in ["hunter2", "hunter2h", "1h_hunter2"] {
            if let Err(e) = Duration::parse_text(secret) {
                let msg = e.to_string();
                assert!(!msg.contains("hunter"), "{msg}");
            }
        }
    }
}
