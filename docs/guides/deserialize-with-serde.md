# Deserialize into structs with serde

The workhorse: an NML block body straight into your `Deserialize` types —
nested blocks become nested structs, arrays become `Vec`s, and integer
fields are exact (a fractional or out-of-range value is a typed error,
never a silent cast).

```nml check
service Api:
    host = "0.0.0.0"
    port = 8080
    tags = ["edge", "public"]

    database:
        url = "postgres://localhost/api"
        poolSize = 10
```

```rust source=docs/guides/examples/cookbook/examples/deserialize.rs
    let service: Service = from_body(body)?;
    assert_eq!(service.port, 8080);
    assert_eq!(service.database.pool_size, 10);
    assert_eq!(service.tags, ["edge", "public"]);
```

Entry points, named for their inputs: `de::from_body(&Body)`,
`de::from_value(&Value)`, and `de::from_body_resolved(&Body, &ValueResolver)`
when the config contains `$ENV` references or fallback chains (see the
[secret resolver recipe](custom-secret-resolver.md)).

Full program: [`deserialize.rs`](examples/cookbook/examples/deserialize.rs)
— `cargo run -p nml-cookbook --example deserialize`.

**Naming:** NML convention is `camelCase` keys; use `#[serde(rename)]` (or
`rename_all = "camelCase"`) on the Rust side, as the example does for
`poolSize`. Named list items and label injection work exactly as serde
expects — a `- name:` item's label lands in the field your struct maps it
to.
