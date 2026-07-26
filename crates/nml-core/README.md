# nml-core

Core library for **NML**, a typed, indentation-based configuration language —
parsing, querying, value resolution, and serde deserialization for embedding
NML in your Rust application.

```nml check
service MyApp:
    host = "0.0.0.0"
    port = 8080
    apiKey = $ENV.API_KEY | "dev-key"

    database:
        url = $ENV.DATABASE_URL
        pool_size = 10
```

```rust
use nml_core::{parse, Document, ValueResolver};
use nml_core::de::from_body_resolved;
use serde::Deserialize;

#[derive(Deserialize)]
struct ServiceConfig {
    host: String,
    port: u16, // NML numbers are exact — out-of-range or fractional is an error
    #[serde(rename = "apiKey")]
    api_key: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = r#"
service MyApp:
    host = "0.0.0.0"
    port = 8080
    apiKey = $ENV.API_KEY | "dev-key"
"#;
    let file = parse(source)?;
    let doc = Document::new(&file);
    let body = doc.block("service", "MyApp").body().ok_or("not found")?;

    // The resolver decides what `$ENV` references mean: `ValueResolver::env()`
    // reads real environment variables in production; any closure works —
    // a vault client, a test fixture — and receives the bare key ("API_KEY").
    let resolver = ValueResolver::new(|key| Some(format!("demo-{key}")));
    let config: ServiceConfig = from_body_resolved(body, &resolver)?;
    assert_eq!(config.port, 8080);
    assert_eq!(config.api_key, "demo-API_KEY");
    println!("{}:{}", config.host, config.port);
    Ok(())
}
```

## What's inside

- **Lossless CST parser** (rowan) — resilient parsing that recovers from
  errors and reports all of them (`parse_to_ast_all`), preserves comments,
  and supports byte-exact structural editing (`cst::edit`).
- **Typed values** — `string`, `number` (exact `i64` semantics), `money`
  (ISO 4217, integer minor units), `bool`, `duration` (`30s`, semantic
  equality across units, lands in `std::time::Duration`), `path`, `secret`
  (`$ENV.X` with fallback chains), `object`, `role`.
- **Query API** — fluent, serde-free reads (`Document::block(..).property(..)`).
- **Value resolution** — pluggable secret lookup (`ValueResolver`), fallback
  chains, shared-property inheritance, schema-driven defaulting.
- **Serde bridge** — deserialize blocks straight into your structs
  (`from_body`, `from_body_resolved`, `from_value`).
- **Semantic config diff** — schema-aware change detection (`diff_config`)
  with structured paths, set deltas, and file/default origins; the engine
  behind live-reload classification.
- **Schema model** — models, traits, enums, `oneof` unions, typed arm maps
  `(K -> V)`, `set<T>`, directives (`#name`), field constraints.

Schema *validation* lives in [`nml-validate`](https://crates.io/crates/nml-validate);
formatting in [`nml-fmt`](https://crates.io/crates/nml-fmt); the CLI is
[`nml-cli`](https://crates.io/crates/nml-cli); editor support is
[`nml-lsp`](https://crates.io/crates/nml-lsp).

## Documentation

- [Language guide](https://github.com/nudge-io/nml/blob/main/docs/language-guide.md)
- [Integration guide](https://github.com/nudge-io/nml/blob/main/docs/integration.md)
- [Language specification](https://github.com/nudge-io/nml/blob/main/spec/README.md)

## License

MIT OR Apache-2.0
