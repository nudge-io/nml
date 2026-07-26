# Parse a file and read values

When you need a few values without defining structs — feature flags, a port,
a list of names — parse and query directly. The query API is total: every
read returns an `Option`, and a type mismatch is `None`, never a panic or a
silent cast.

```nml check
const region = "us-east-1"

service Api:
    host = "0.0.0.0"
    port = 8080
    replicas = 3
```

```rust source=docs/guides/examples/cookbook/examples/parse_and_query.rs
    let port = doc.block("service", "Api").property("port").to_i64();
    assert_eq!(port, Some(8080));

    // Every block of a keyword, with its name.
    let services = doc.blocks("service");
    let names: Vec<&str> = services.iter().map(|(name, _)| *name).collect();
    assert_eq!(names, ["Api", "Worker"]);

    // Top-level consts resolve through the same document view.
    let region = doc.const_value("region").as_str().map(str::to_owned);
    assert_eq!(region.as_deref(), Some("us-east-1"));
```

Entry points: `nml_core::parse` (source → `File`, first error only — see
[collect all errors](collect-all-errors.md) for the report-everything mode)
and `Document::new(&file)` for the query view.

Full program: [`parse_and_query.rs`](examples/cookbook/examples/parse_and_query.rs)
— run it with `cargo run -p nml-cookbook --example parse_and_query`.

**Numbers are exact.** Every NML number is an exact decimal (RFC 0016) —
`0.20` is 0.20, integers survive to 34 significant digits, and nothing
rounds silently (out-of-domain literals are parse errors, `NML0014`).
`to_i64()` returns `None` for fractional values rather than truncating;
`to_f64()` is the explicit, correctly-rounded binary edge. If you want
typed extraction with real error messages, the
[serde recipe](deserialize-with-serde.md) is the better tool.
