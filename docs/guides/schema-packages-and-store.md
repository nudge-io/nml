# Build and publish schema packages

A schema package is your tool's schemas + manifest (bindings, modifiers,
directive vocabulary) as one content-addressed unit. **Identity is the
content hash** — versions are human labels — so "which schema is my editor
using" always has an exact answer.

```rust source=docs/guides/examples/cookbook/examples/packages_and_store.rs
    // Content-addressed identity: same bytes, same hash, everywhere.
    let hash = package.content_hash();
    println!("skylight {} @ {}", package.manifest.version, hash8(&hash));
```

```rust source=docs/guides/examples/cookbook/examples/packages_and_store.rs
    store.publish(&package)?;
    // Idempotent: re-publishing identical content is a no-op, not an error.
    store.publish(&package)?;

    // What a consumer does: read the current slot by package name.
    let current = store.read_current("skylight")?;
    assert_eq!(current.package.content_hash(), hash);
```

Full program: [`packages_and_store.rs`](examples/cookbook/examples/packages_and_store.rs)
— `cargo run -p nml-cookbook --example packages_and_store`.

Construction: `SchemaPackage::from_dir` (a directory holding
`<name>.package.nml` plus sources — the committed-file channel) or
`from_parts` (manifest text plus a source resolver — the embedded channel,
usually `include_str!`). Publishing: `Store::user()` is the per-user store
your users' editors read (your tool's `schema sync` publishes there);
`Store::at(dir)` for tests and hermetic setups.

**How it reaches users:** commit a `<name>.package.nml` in their project
(zero-config editor validation), publish to the store, or [embed the whole
server](embed-the-lsp.md) so the schema ships inside your binary and can
never be stale.
