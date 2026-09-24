//! Fuzz the formatter's foundational promise: for any VALID document, the
//! formatter's output parses (it never emits a document the parser
//! rejects — the invariant behind the edge-space/quote-run/brace/tab
//! protections), formatting is idempotent, and formatting PRESERVES the
//! document: the lowered tree of the output equals the input's, spans
//! aside — gofmt's and rustfmt's own invariant. Three instances of the
//! first class were found by hand; this target searches for the rest.

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
    assert_eq!(
        shape(s),
        shape(&formatted),
        "formatting changed the document's structure or values"
    );
});

/// The lowered tree as JSON with every span erased — structure and
/// values only. `format_source` accepted the text, so it lowers.
fn shape(source: &str) -> serde_json::Value {
    let file = nml_core::cst::parse_to_ast(source).expect("the formatter accepted it");
    let mut v = serde_json::to_value(&file).expect("the AST serializes");
    erase_spans(&mut v);
    v
}

fn erase_spans(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            map.retain(|k, _| !(k == "span" || k.ends_with("_span")));
            map.values_mut().for_each(erase_spans);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(erase_spans),
        _ => {}
    }
}
