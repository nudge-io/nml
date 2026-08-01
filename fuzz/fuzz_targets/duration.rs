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
            let permitted: Vec<String> = std::iter::once(DurationTextError::Malformed.to_string())
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

    // ── the literal decoder (`30s`, compound `1h30m` in source) ─────────
    // Whitespace-split pieces alternate magnitude/unit ("1 h 30 m" is the
    // two-component literal `1h30m`), a trailing unpaired magnitude takes
    // `s` so bare numbers still decode — the fuzzer thereby drives the
    // MULTI-component paths (duplicate-unit merged fix, per-component
    // rejection order) that a single component never reaches. Spans
    // mirror a real attached literal so the machine-fix arithmetic is
    // exercised on coherent, not degenerate, ranges.
    let pieces: Vec<&str> = s.split(' ').collect();
    let mut components = Vec::new();
    let mut offset = 0usize;
    let mut i = 0;
    while i < pieces.len() {
        let magnitude = pieces[i];
        let unit = pieces.get(i + 1).copied().unwrap_or("s");
        let magnitude_span = Span::new(offset, offset + magnitude.len());
        let unit_span = Span::new(magnitude_span.end, magnitude_span.end + unit.len());
        components.push(nml_core::duration::ComponentTokens {
            magnitude,
            unit,
            magnitude_span,
            unit_span,
        });
        offset = unit_span.end;
        i += 2;
    }
    let span = Span::new(0, offset);
    match nml_core::duration::parse_components_cst(&components, span) {
        Ok(d) => {
            assert!(
                d.total_nanos() <= u64::MAX as u128 * 1_000_000_000 + 999_999_999,
                "literal decode escaped the domain gate for {s:?}"
            );
            // Acceptance implies every authored suffix was a real unit.
            assert!(
                components
                    .iter()
                    .all(|c| DurationUnit::from_suffix(c.unit).is_some()),
                "accepted literal with a non-unit suffix for {s:?}"
            );
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
            // NML3005/NML3007 fixes are duration respellings by
            // construction — a replacement the coercion grammar cannot
            // re-read would be a corrupting machine fix.
            let fix_is_a_spelling = diag.code.is_some_and(|c| {
                let c = c.to_string();
                c == "NML3005" || c == "NML3007"
            });
            for suggestion in &diag.suggestions {
                assert!(
                    suggestion.span.start <= suggestion.span.end,
                    "inverted fix span for {s:?}"
                );
                if fix_is_a_spelling {
                    assert!(
                        Duration::parse_text(&suggestion.replacement).is_ok(),
                        "unparseable machine fix for {s:?}: {:?}",
                        suggestion.replacement
                    );
                }
            }
        }
    }
});
