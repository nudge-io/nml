//! Fuzz duration parsing (RFC 0017). Both grammars must never panic, and
//! every accepted value must satisfy the round-trip, domain, and
//! separator-transparency invariants.
//!
//! Why this target exists separately from `number`: the coercion grammar
//! [`Duration::parse_text`] is reached from three externally-influenced
//! surfaces — the LSP's hover (any word under the cursor, in any open
//! document), schema validation (any string in a duration-typed field),
//! and `de`'s `$ENV` coercion (environment values) — none of which the
//! document/number targets exercise. Its shape (digits + separators +
//! a unit suffix) also mutates differently enough from a bare number that
//! it earns its own corpus rather than competing inside another's.
//!
//! The error path is fuzzed too, deliberately: a diagnostic carries span
//! arithmetic and machine-applicable fixes derived from the input, so
//! "rendering a rejection never panics" is as much an invariant as
//! "parsing a valid value is exact".

#![no_main]

use libfuzzer_sys::fuzz_target;
use nml_core::duration::{Duration, DurationTextError, DurationUnit};
use nml_core::span::Span;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };

    // ── the coercion grammar (`$ENV`, hover, validation) ────────────────
    match Duration::parse_text(s) {
        Ok(d) => {
            // Canonical form is itself parseable and equal — the property
            // `fmt` depends on when it renders from the decoded value.
            let shown = d.to_string();
            let again = Duration::parse_text(&shown)
                .unwrap_or_else(|e| panic!("reparse of {shown:?}: {e}"));
            assert_eq!(d, again, "round-trip of {s:?}");
            assert_eq!(d.cmp(&again), std::cmp::Ordering::Equal);

            // The domain check at construction is what makes `as_std`
            // infallible; it must also agree with the comparison basis.
            let std = d.as_std();
            assert_eq!(
                std.as_secs() as u128 * 1_000_000_000 + std.subsec_nanos() as u128,
                d.total_nanos(),
                "as_std disagrees with total_nanos for {s:?}"
            );
            assert!(d.total_nanos() <= u64::MAX as u128 * 1_000_000_000 + 999_999_999);

            // Separators are spelling, never value.
            if s.contains('_') {
                let stripped: String = s.chars().filter(|c| *c != '_').collect();
                let bare = Duration::parse_text(&stripped)
                    .unwrap_or_else(|e| panic!("stripped form of {s:?}: {e}"));
                assert_eq!(d, bare, "separator transparency of {s:?}");
            }
        }
        // **Security invariant, fuzzed rather than exampled.** The
        // resolver erases provenance, so any coerced string may be a
        // resolved `$ENV` secret; a coercion message that echoed it would
        // leak credentials into logs. Rather than guess with a substring
        // check (short inputs legitimately occur inside the fixed prose),
        // assert the stronger property the design actually promises: the
        // message is drawn from a CLOSED set determined by the error kind
        // and unit alone, so it cannot vary with the input at all. Adding
        // an input interpolation to that message fails this immediately.
        Err(e) => {
            let permitted: Vec<String> = std::iter::once(
                DurationTextError::Malformed.to_string(),
            )
            .chain(
                DurationUnit::ALL
                    .into_iter()
                    .map(|u| DurationTextError::OutOfRange(u).to_string()),
            )
            .collect();
            let text = e.to_string();
            assert!(
                permitted.contains(&text),
                "coercion message varies with input (possible secret leak) for {s:?}: {text:?}"
            );
        }
    }

    // ── the literal decoder (`30s` in source) ───────────────────────────
    // Split as money.rs does: "<magnitude> <unit>", unit defaulting to `s`.
    // Spans mirror a real literal so the machine-fix arithmetic is
    // exercised on coherent, not degenerate, ranges.
    let (raw, unit) = s.split_once(' ').unwrap_or((s, "s"));
    let span = Span::new(0, raw.len() + unit.len());
    let unit_span = Span::new(raw.len(), raw.len() + unit.len());
    match nml_core::duration::parse_duration_literal(raw, unit, span, unit_span) {
        Ok(d) => {
            assert_eq!(d.magnitude(), d.magnitude());
            assert!(DurationUnit::from_suffix(unit).is_some());
            // The literal's canonical form re-reads through the text
            // grammar: one value domain, two entry points.
            let shown = d.to_string();
            assert_eq!(
                Duration::parse_text(&shown).expect("canonical form parses"),
                d
            );
        }
        Err(e) => {
            // Diagnostic construction derives spans and replacements from
            // the payload — the class of arithmetic that panics on edge
            // input if it is wrong.
            let diag = e.to_diagnostic();
            let _ = diag.rendered_message();
            for suggestion in &diag.suggestions {
                assert!(
                    suggestion.span.start <= suggestion.span.end,
                    "inverted fix span for {s:?}"
                );
            }
        }
    }
});
