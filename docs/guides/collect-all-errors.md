# Collect *all* parse errors

`parse` stops at the first error — right for a load path, wrong for a UI.
`parse_to_ast_all` recovers and reports everything at once: each finding is
a structured `Diagnostic` with a stable code (`NML0013`), a span, and
machine-applicable suggestions — the same model the CLI and the language
server render.

```rust source=docs/guides/examples/cookbook/examples/collect_all_errors.rs
    let (file, diagnostics) = parse_to_ast_all(source);

    // Recovery still produced a usable best-effort AST…
    assert_eq!(file.declarations.len(), 1);
    // …and every problem is reported, position-sorted, not just the first.
    assert!(diagnostics.len() >= 2);
```

Full program: [`collect_all_errors.rs`](examples/cookbook/examples/collect_all_errors.rs)
— `cargo run -p nml-cookbook --example collect_all_errors`.

**Contract notes.** Codes are stable forever ([stability policy](../stability.md));
message *text* is not an interface — match on `code`, render with
`rendered_message()` (hints derive from structured suggestions; never parse
prose). Output is bounded by design: at most 128 diagnostics, with an exact
"N suppressed" info marker when clipping occurs — resilient parsing on
untrusted input is a deliberate DoS defense. Every code's full explanation
is in the [error index](../errors/README.md) and offline via
`nml explain NML0013`.
