//! Fuzz the full document parse: never panics, and the lossless CST
//! invariant holds — the tree's text is byte-identical to the source
//! (RFC 0004), for every input including hostile numeric literals.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let parse = nml_core::cst::parse(s);
    assert_eq!(
        parse.syntax().text().to_string(),
        s,
        "lossless CST invariant violated"
    );
    // The semantic pipeline is input-reachable too (nudge decodes tenant
    // config): the source-character policy, lowering, and the TOTAL value
    // decoders (U+FFFD escape recovery, multiline shape rules, line
    // continuation) must never panic, and the findings list stays bounded
    // (RFC 0009 exact-count honesty caps every collection site).
    let (_file, diags) = nml_core::cst::parse_to_ast_all(s);
    assert!(diags.len() <= 512, "diagnostics not bounded: {}", diags.len());
});
