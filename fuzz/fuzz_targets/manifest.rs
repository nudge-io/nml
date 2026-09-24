//! Fuzz the MANIFEST loader and the analyses that run over what it
//! produces — the one input a hostile REPOSITORY writes that the kernel
//! parses with rules of its own, and the one every front end re-reads on
//! every invocation.
//!
//! `parse_manifest` is the meta-validation pipeline (the embedded
//! `package.model.nml`, `extract_manifest`, the `formatVersion` gate, the
//! one segment rule every glob field shares, the `budgetUnits` nesting
//! rule and the `layers:` grant's own rules); `shadow_warnings` is what
//! the editor then runs over the typed form, per diagnostics pull, on the
//! manifest DOCUMENT.
//!
//! **The load-bearing invariant is that those analyses are BOUNDED.**
//! `shadow_warnings` compares every binding's globs against every earlier
//! binding's — quadratic in a count the manifest declares — and only the
//! per-comparison step used to be bounded: a manifest inside every
//! published bound took 108 s for 447 bindings and 127 s for one
//! `textDocument/diagnostic` on it (r113). A per-input wall clock is what
//! turns that class into a crash a fuzzer can find, and the budget the
//! fix introduced (`MAX_SHADOW_WORK`) is what keeps it under the bound.
//! The value is generous by two orders of magnitude against the measured
//! worst case (~1 s), so a loaded machine cannot make it flake.
//!
//! Span integrity is checked for the same reason the `validate` target
//! checks it: a manifest finding is located, the editor maps it against
//! the manifest's own text, and a code action is minted from it.

#![no_main]

use libfuzzer_sys::fuzz_target;

/// What ONE manifest's analyses may take. Two orders of magnitude above
/// the bounded worst case, so only a REGRESSION of the bound reaches it.
const BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let start = std::time::Instant::now();
    let Ok(manifest) = nml_validate::package::parse_manifest(source) else {
        // A refusal is an outcome, not a failure — but it must be a
        // BOUNDED one: the meta-validation pipeline runs before any
        // rule of the loader's own.
        assert!(
            start.elapsed() < BUDGET,
            "refusing a manifest took {:?}",
            start.elapsed()
        );
        return;
    };
    let warnings = manifest.shadow_warnings();
    let elapsed = start.elapsed();
    assert!(
        elapsed < BUDGET,
        "the manifest analyses are not bounded: {elapsed:?} for {} binding(s)",
        manifest.validators.len()
    );
    for d in &warnings {
        let Some(span) = d.span else { continue };
        assert!(span.start <= span.end, "reversed span {span:?}");
        assert!(span.end <= source.len(), "span past the source {span:?}");
        assert!(
            source.is_char_boundary(span.start) && source.is_char_boundary(span.end),
            "span off a char boundary {span:?}"
        );
    }
});
