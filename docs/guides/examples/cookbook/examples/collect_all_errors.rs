//! Recipe: collect *all* parse errors — the editor-grade experience.
//!
//! `parse` stops at the first error (the right default for a load path);
//! `parse_to_ast_all` recovers and reports everything at once, each finding
//! a structured `Diagnostic` with a stable code and derived hints.
use nml_core::cst::parse_to_ast_all;

fn main() {
    // Three distinct mistakes: a bad number, a tab in indentation is fine
    // here (spaces), an unterminated string, and an unknown escape.
    let source = "service Api:\n    x = 1.2.3\n    name = \"abc\n    y = \"a\\q\"\n";

    let (file, diagnostics) = parse_to_ast_all(source);

    // Recovery still produced a usable best-effort AST…
    assert_eq!(file.declarations.len(), 1);
    // …and every problem is reported, position-sorted, not just the first.
    assert!(diagnostics.len() >= 2);

    for d in &diagnostics {
        // Stable code + rendered message (hints derive from structured
        // suggestions — never parse message text).
        let code = d.code.map(|c| c.to_string()).unwrap_or_default();
        println!("{code}: {}", d.rendered_message());
    }
    assert!(
        diagnostics
            .iter()
            .any(|d| { d.code.is_some_and(|c| c.to_string() == "NML0013") })
    );

    println!("recipe OK: collect_all_errors");
}
