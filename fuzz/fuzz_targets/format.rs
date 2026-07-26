//! Fuzz the formatter's foundational promise: for any VALID document, the
//! formatter's output parses (it never emits a document the parser
//! rejects — the invariant behind the edge-space/quote-run/brace/tab
//! protections) and formatting is idempotent. Three instances of this
//! class were found by hand; this target searches for the rest.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    // The round-trip promise is scoped to well-formed input.
    let Ok(formatted) = nml_fmt::formatter::format_source(s) else {
        return;
    };
    let reformatted = nml_fmt::formatter::format_source(&formatted)
        .expect("formatter emitted a document the parser rejects");
    assert_eq!(formatted, reformatted, "formatting is not idempotent");
});
