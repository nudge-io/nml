# Format user files idempotently

`format_source` is the whole API: canonical style out, comments preserved.
Idempotence — `format(format(x)) == format(x)` — is what makes it safe on
every save, in pre-commit hooks, and in `<your-tool> fmt`.

```rust source=docs/guides/examples/cookbook/examples/format_files.rs
    let once = format_source(messy)?;
    let twice = format_source(&once)?;

    assert_eq!(once, twice, "formatting must be idempotent");
    assert!(once.contains("// keep me"), "comments are preserved");
    assert!(once.contains("port = 8080"), "canonical spacing");
```

Full program: [`format_files.rs`](examples/cookbook/examples/format_files.rs)
— `cargo run -p nml-cookbook --example format_files`.

**Write atomically.** `format_source` returns a `String`; your tool owns
the write. Use temp-file-plus-rename in the target directory so a crash
mid-write never truncates a user's config (the `nml fmt` CLI does exactly
this). On a parse error the function returns the error rather than a
"best-effort" reformat — never write anything in that case.

**In the editor**, the same formatter runs behind the LSP's
document-formatting request (with canonical `" & "` conjunction spacing
and `as`-annotation preservation) — [embed the server](embed-the-lsp.md)
and your users get format-on-save against the identical engine.
