# Apply schema defaults before deserializing

Declare defaults once, in the schema — every consumer (CLI validation, the
editor, your deserialization) sees the same completed values, and your Rust
structs need no `#[serde(default)]` mirrors that could drift.

```nml check
model server:
    host string
    port number = 8080
    logLevel string = "info"
```

```rust source=docs/guides/examples/cookbook/examples/apply_defaults.rs
    let server: Server =
        from_document_defaulted(&index, &doc, "server", "Main", &ValueResolver::env())?;
    assert_eq!(server.port, 8080); // from the schema default
    assert_eq!(server.log_level, "info"); // from the schema default
    assert_eq!(server.host, "0.0.0.0"); // from the instance
```

The family, named for its inputs: `from_document_defaulted` (query view +
keyword + name), `from_body_defaulted` (a `&Body` you already hold), and
`apply_defaults` when you want the completed `Body` without deserializing.
All take the `SchemaIndex` (built from your extracted schema) and a
resolver — defaults fill first, references resolve second, serde runs last,
so a defaulted `$ENV` reference resolves like any other.

Full program: [`apply_defaults.rs`](examples/cookbook/examples/apply_defaults.rs)
— `cargo run -p nml-cookbook --example apply_defaults`.
