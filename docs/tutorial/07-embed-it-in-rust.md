# 7 · Embed it in Rust

This is the chapter the previous six were building toward: Skylight's config
stops being a file you check and becomes typed data your program trusts.
You write the full embedding pipeline — parse, validate, resolve, apply
defaults, deserialize into serde structs — and it prints this (real output,
run in CI):

```text
Skylight https://status.skylight.dev on 0.0.0.0:8080 — log level info, retries 3, 4 endpoint(s)
  - Api https://api.skylight.dev (timeout 10s, every 60s, regions: us-east+eu-west)
  - Marketing https://www.skylight.dev (timeout 10s, every 60s, regions: all)
  - AdminConsole https://admin.skylight.dev (timeout 10s, every 60s, regions: all)
  - (unnamed) https://docs.skylight.dev (timeout 10s, every 60s, regions: all)
  database postgres://localhost/skylight (pool 10) — api key 20 chars, tags ["web", "api"], timeout 30s
```

Look at what's in there that no single line of config wrote: `log level
info`, `every 60s`, and `pool 10` are schema defaults; `timeout 10s` is the
list's shared `.timeout` beating the trait's `5s` default; the last endpoint
is a bare positional item (`- "https://docs.skylight.dev"`) that still
arrived fully materialized; `retries 3` came through a `const` reference;
the api key resolved through its fallback chain. The full precedence ladder
from Chapter 4 — schema default → shared property → the item's own value —
lands in your structs already settled.

The complete program lives in
[`examples/07/app/`](examples/07/app/); the chapter walks through it. One
layout change from Chapter 6: the endpoint list moves inline into the
`service` block ([`examples/07/app.nml`](examples/07/app.nml)) — the
pipeline deserializes *bodies*, and inline is the shape you want for data
your program consumes directly (more on references below).

## Set up

```toml
[dependencies]
nml-core = { git = "https://github.com/nudge-io/nml" }       # crates.io: soon
nml-validate = { git = "https://github.com/nudge-io/nml" }
serde = { version = "1", features = ["derive"] }
```

`nml-core` is the language (parse, query, resolve, serde bridge, defaults —
no tokio, no server); `nml-validate` adds schema loading and validation.

## The types you actually want

```rust source=docs/tutorial/examples/07/app/src/main.rs
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceConfig {
    host: String,
    port: u16,
    public_url: String,
    log_level: String,
    request_timeout: String,
    retries: u32,
    tags: Vec<String>,
    api_key: String,
    database: DatabaseConfig,
    endpoints: Vec<Endpoint>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DatabaseConfig {
    url: String,
    pool_size: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Endpoint {
    /// Injected from the list item's label (`- Api:` → `"Api"`). Bare
    /// positional items (`- "https://…"`) carry no label, so default it.
    #[serde(default)]
    name: String,
    url: String,
    timeout: String,
    check_interval: String,
    regions: Option<Vec<String>>,
}
```

(That block — like every Rust listing in this chapter — is checked in CI to
be a verbatim excerpt of the compiled program.)

Three things to notice:

- `#[serde(rename_all = "camelCase")]` maps NML's `camelCase` properties to
  Rust's `snake_case` fields.
- Nested blocks become nested structs; list items become `Vec<T>`, and each
  named item's label arrives as a `name` field — bare positional items have
  no label, so `name` defaults (they're anonymous by design).
- Your structs only declare what you need — body entries you don't ask for
  (`notifiers`, `landing`, `|allow`…) are simply not deserialized.

## The pipeline, step by step

Load the schema and refuse to start if it's broken — the same sources
`nml check --schema` reads:

```rust source=docs/tutorial/examples/07/app/src/main.rs
    // 1. Load the schema — the same files `nml check --schema` reads.
    let schema_src = fs::read_to_string("skylight.model.nml")?;
    let (schema, schema_diags) = load_schema(&[("skylight.model.nml", &schema_src)]);
    if !schema_diags.is_empty() {
        for d in &schema_diags {
            eprintln!("schema: {}", d.rendered_message());
        }
        return Err("schema failed to load".into());
    }

    // 2. Parse the config file.
    let source = fs::read_to_string("app.nml")?;
    let file = parse(&source)?;

    // 3. Validate, and refuse to run on errors — config bugs stop here,
    //    not at 3 a.m.
    let validator = SchemaValidator::new(
        schema.models.clone(),
        schema.enums.clone(),
        schema.oneofs.clone(),
    );
    let diagnostics = validator.validate(&file);
    for d in &diagnostics {
        eprintln!("{}: {}", d.severity, d.rendered_message());
    }
    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return Err("configuration is invalid".into());
    }
```

`rendered_message()` includes the did-you-mean hints, so your users get CLI-
quality errors from *your* binary. Try it — put `logLevel = "wran"` into
`app.nml` and run:

```text
error: invalid value "wran" for 'logLevel': expected one of "debug", "info", "warn", "error" (did you mean "warn"?)
Error: "configuration is invalid"
```

Next, decide what references mean. This is Chapter 2's promise kept: the
committed file holds *references*; the application decides how they
resolve — environment, vault, or a dev default so a fresh checkout runs
without credentials:

```rust source=docs/tutorial/examples/07/app/src/main.rs
    // 4. Decide what references mean: environment first, then dev fallbacks
    //    so a fresh checkout runs without real credentials. `const`
    //    references resolve from a snapshot of the file's declarations.
    let mut symbols = SymbolTable::new();
    symbols.register_file(&file);
    let consts = symbols.resolved_const_snapshot();
    let resolver = ValueResolver::new(|key| {
        std::env::var(key).ok().or_else(|| match key {
            "SKYLIGHT_API_KEY_DEV" => Some("dev-key-not-a-secret".to_string()),
            _ => None,
        })
    })
    .with_symbols(move |name| consts.get(name).cloned());
```

The closure answers `$ENV.X` lookups; `with_symbols` answers bare-name
references (`retries = MaxRetries`) from a resolved snapshot of the file's
`const` declarations.

Finally, defaults + deserialization in one call:

```rust source=docs/tutorial/examples/07/app/src/main.rs
    // 5. Apply schema defaults and deserialize into your types.
    let index = SchemaIndex::build(schema.models, schema.enums, schema.oneofs);
    let doc = Document::new(&file);
    let body = doc
        .block("service", "Api")
        .body()
        .ok_or("block `service Api` not found in app.nml")?;
    let config: ServiceConfig = from_body_defaulted(&index, "service", body, &resolver)?;
```

`from_body_defaulted` runs the whole tail of the pipeline: positional-item
materialization, shared-property merge (every body scope, at any depth),
schema defaults (including the trait-inherited ones — that's where `every
60s` comes from), reference resolution, then serde. Your structs receive
finished values; no `unwrap_or` sprinkled through the codebase.

The rest of the program is `println!`. Run it from the chapter directory
(`cargo run`) and you get the output at the top of the page.

## What about references between declarations?

Chapter 6's config said `endpoints = monitoredEndpoints` — a reference to
a top-level array declaration. This program deserializes the inline form,
but you don't have to restructure a config to embed it: the pipeline also
runs at **document scope**, where declaration references are materialized
for you (shared properties and items inlined, exactly as if written in
place):

```rust
let config: ServiceConfig =
    from_document_defaulted(&index, &doc, "service", "Api", &resolver)?;
```

`const` references resolve through `with_symbols` either way, and a
reference the document doesn't define passes through for validation to
report. The remaining late-bound cases are deliberate: reference *list
items* (`- SomeRef`) and role/landing targets stay yours to interpret —
the query API (`doc.blocks(…)`, `doc.array_body(…)`,
`doc.const_value(…)`) is how you look them up.

## Exercises

1. Deserialize `plan Pro` too: a `PlanConfig` with `name` and `trial_days`.
   Reuse the same index and resolver — only the root model changes.

   <details><summary>Solution</summary>

   ```rust
   #[derive(Debug, Deserialize)]
   #[serde(rename_all = "camelCase")]
   struct PlanConfig {
       name: String,
       trial_days: u32,
   }

   let body = doc
       .block("plan", "Pro")
       .body()
       .ok_or("block `plan Pro` not found")?;
   let plan: PlanConfig = from_body_defaulted(&index, "plan", body, &resolver)?;
   // "Professional", 30 days
   ```

   </details>

2. Delete the `"SKYLIGHT_API_KEY_DEV"` arm from the resolver and run again
   (with neither env var set). What happens, and when?

   <details><summary>Solution</summary>

   Deserialization fails with a resolve error naming the variable
   (`EnvNotSet("SKYLIGHT_API_KEY_DEV")` — the last reference in the
   fallback chain). It fails at *startup*, in step 5 — not at the first
   request that needs the key. Fail-fast is the point.

   </details>

## Common mistakes

- **Skipping validation before deserializing.** Serde errors say "missing
  field"; the validator says *which line, which model, and what to write
  instead*. Run the validator first, always.
- **Forgetting `rename_all = "camelCase"`.** NML convention is camelCase
  properties; without the attribute every field looks missing.
- **Resolving secrets by hand.** Don't `std::env::var` around your structs
  — give the resolver the policy once, and every secret and fallback chain
  goes through it.

## Recap

- The pipeline is five calls: `load_schema` → `parse` → `validate` →
  build a `ValueResolver` (+ `resolved_const_snapshot` for consts) →
  `from_body_defaulted` — and your structs receive finished, defaulted,
  resolved values.
- `rendered_message()` gives your binary CLI-quality, did-you-mean
  diagnostics for free; validate first, deserialize second.
- References stay late-bound: consts resolve via `with_symbols`,
  declaration references via the query API when *you* choose; inline the
  data you want as structs.

Next: [Chapter 8 — React to change](08-react-to-change.md): the config
changes while Skylight is running — the schema says which changes apply
live, and you build the classifier that enforces it.
