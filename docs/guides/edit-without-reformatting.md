# Edit a file without destroying formatting

Tools that rewrite users' configs by round-tripping through an AST destroy
comments and layout — the fastest way to make users hate your tool. The
CST splice API edits the **lossless** tree: everything outside the inserted
lines is preserved byte-for-byte.

```rust source=docs/guides/examples/cookbook/examples/cst_edit.rs
    let edited = insert_entry_at_path(
        source,
        &["service", "database"],
        "poolSize = 10",
        EntryPosition::Last,
    )
    .ok_or("edit refused")?;
```

Full program: [`cst_edit.rs`](examples/cookbook/examples/cst_edit.rs)
— `cargo run -p nml-cookbook --example cst_edit`.

**The path grammar** is heterogeneous by depth, on purpose: the first
segment addresses a top-level block by **keyword** (names are user-chosen
and unknowable to your code; the keyword is the stable address), each later
segment addresses a nested block by **name**.

**Refusal is the security property.** The API returns `None` rather than
guess: on sources that don't parse cleanly (splicing into an
error-recovered tree would act on a structural guess), on ambiguous paths
(two matching blocks — it will not pick one and silently misdirect the
write), and on snippets that don't parse as body entries (no string-pasting
injection). Treat `None` as "surface this to the user," never as "retry
with force."
