//! Fuzz schema loading and validation — the largest logic surface in the
//! workspace (~10k lines) and, until this target, the only one with no
//! fuzzing at all. Every other target stops at parsing; this one runs the
//! pipeline a real `nml check` runs.
//!
//! One input serves as both schema and instance, which is not a trick but
//! the language's own single-file workflow (RFC 0012: `model cache` above
//! `cache Foo:` validates with no flags). So a single mutation stream
//! exercises definition-side rules (composition, oneof integrity, facet
//! declarations) and value-side rules (type checks, set uniqueness, facet
//! enforcement) together, the way they meet in practice.
//!
//! **The load-bearing invariant is span integrity.** Validation is where
//! machine-applicable suggestions are minted, and `nml fix` *applies* them
//! to real files without a human in the loop. A suggestion whose span is
//! reversed, out of bounds, or mid-UTF-8 would corrupt a file — so this
//! target checks every emitted span against the source it came from.
//! `cst::edit::splice` refuses such spans as defense in depth; the point
//! here is that no producer should ever mint one.
//!
//! Reaching `Number::is_multiple_of` is a second motive: its modular
//! exponentiation runs up to ~12k iterations over attacker-chosen scales,
//! and no other target can reach it (facets live only in schemas).

#![no_main]

use libfuzzer_sys::fuzz_target;
use nml_validate::schema::SchemaValidator;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };

    // Loading must survive arbitrary source: it parses internally and
    // reports parse failures as attributed diagnostics rather than
    // aborting, so there is no "valid input" precondition to satisfy.
    let (schema, load_diags) = nml_validate::loader::load_schema(&[("fuzz.nml", source)]);
    check_spans(&load_diags, source, "load");

    // Definition- and instance-side validation, against the same document
    // — the self-validating-file path (RFC 0012).
    let (file, parse_diags) = nml_core::cst::parse_to_ast_all(source);
    check_spans(&parse_diags, source, "parse");
    if schema.is_empty() {
        return;
    }
    let validator = SchemaValidator::from(schema).composition_checked_at_load();
    check_spans(
        &validator.validate_definitions(&file),
        source,
        "definitions",
    );
    check_spans(&validator.validate(&file), source, "instances");
});

/// Every diagnostic span — and every machine-applicable suggestion span —
/// must address the source it was derived from: ordered, in bounds, and on
/// character boundaries. The suggestion check is the one that matters most:
/// those spans get spliced into files by `nml fix`.
fn check_spans(diags: &[nml_core::diagnostic::Diagnostic], source: &str, phase: &str) {
    for d in diags {
        if let Some(span) = d.span {
            assert!(
                span.start <= span.end,
                "{phase}: inverted diagnostic span {span:?}"
            );
        }
        for s in &d.suggestions {
            assert!(
                s.span.start <= s.span.end,
                "{phase}: inverted suggestion span {:?} for {:?}",
                s.span,
                s.replacement
            );
            assert!(
                s.span.end <= source.len(),
                "{phase}: suggestion span {:?} past end of {}-byte source",
                s.span,
                source.len()
            );
            assert!(
                source.is_char_boundary(s.span.start) && source.is_char_boundary(s.span.end),
                "{phase}: suggestion span {:?} is not on character boundaries — \
                 applying it would corrupt the file",
                s.span
            );
        }
        // Rendering walks the payload and bounds every echo; it must not
        // panic, and a control character must never reach output raw (a
        // hostile file could otherwise smuggle terminal escapes).
        let rendered = d.rendered_message();
        assert!(
            !rendered.contains('\u{1b}'),
            "{phase}: raw escape character reached rendered output"
        );
    }
}
