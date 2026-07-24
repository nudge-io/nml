# NML

**NML is a typed configuration language you embed in your Rust application** —
with a real schema system, first-class secrets and money, and editor-grade
tooling your users inherit for free.

Config formats make you choose: human-friendly but untyped (YAML, TOML), or
typed but heavyweight to adopt (a DSL, a bolted-on JSON Schema). NML is both:
files read like clean indented config, and a `model` gives every field a type,
a default, and a validated shape — with errors that point at the exact span
and tell you what to write instead.

## The 30-second demo

A schema:

```nml check
enum logLevel:
    - "debug"
    - "info"
    - "warn"
    - "error"

model service:
    host string
    port number
    logLevel logLevel = "info"
    apiKey secret
```

A config file that typos an enum value:

```nml check schema=docs/examples/readme expect-error='invalid value "wran"'
service Api:
    host = "0.0.0.0"
    port = 8080
    logLevel = "wran"
    apiKey = $ENV.API_KEY
```

What you get (real output, verified in CI):

```text
app.nml:4:16: error[NML2000]: invalid value "wran" for 'logLevel': expected one of "debug", "info", "warn", "error" (did you mean "warn"?)
```

In the editor, the same diagnostic arrives with a machine-applicable
did-you-mean quick fix (`"wran"` → `"warn"`), schema-driven completion, and
hover docs — from the NML language server, which also builds to WASM and
ships inside the VS Code extension.

## Embed it in Rust

```rust
use nml_core::{parse, Document, ValueResolver};
use nml_core::de::from_body_resolved;
use serde::Deserialize;

#[derive(Deserialize)]
struct ServiceConfig {
    host: String,
    port: f64,
    #[serde(rename = "apiKey")]
    api_key: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = std::fs::read_to_string("app.nml")?;
    let file = parse(&source)?;
    let doc = Document::new(&file);
    let body = doc.block("service", "Api").body().ok_or("service Api not found")?;

    // Resolves $ENV secrets + fallback chains, then deserializes into your struct.
    let config: ServiceConfig = from_body_resolved(body, &ValueResolver::env())?;
    println!("listening on {}:{}", config.host, config.port);
    Ok(())
}
```

```toml
[dependencies]
nml-core = { git = "https://github.com/nudge-io/nml" }   # crates.io release: soon
serde = { version = "1", features = ["derive"] }
```

## Features

- **9 primitive types** — `string`, `number` (exact `i64` semantics), `money`
  (ISO 4217, integer minor units — `19.99 USD` never becomes `19.990000001`),
  `bool`, `duration`, `path`, `secret`, `object`, `role`
- **Schemas as part of the language** — `model`, `trait` (composable,
  non-instantiable mixins), `enum`,
  `oneof` discriminated unions, typed arm maps `(K -> V)`, `set<T>`,
  defaults, optional `?` and positional `+` markers
- **Secrets done right** — `secret` fields hold *references* (`$ENV.API_KEY`,
  with reference fallback chains like `$ENV.API_KEY | $ENV.API_KEY_DEV`),
  resolved by a pluggable `ValueResolver` (env, vault, your lookup). Literal
  credential strings in a `secret` field are a validation error by design —
  the secret value never lives in the committed file
- **Access control built in** — `|allow` / `|deny` modifiers with roles and
  parameterized role references
- **Self-validating files** — schema and config compose into one
  namespace, so a single file with `model` + instances fully validates
  (`nml check app.nml`, no flags); under a shipped schema package the
  vocabulary is closed instead, so tenant files can't redefine or extend
  your schemas
- **Serde-native embedding** — parse → resolve → apply schema defaults →
  deserialize into your structs; or use the fluent query API without serde
- **Semantic config diff** — schema-aware change detection with structured
  paths and set deltas; pair with `#live` / `#restart` directives to classify
  which changes hot-reload and which need a restart
- **Lossless CST** — resilient parsing (all errors, not just the first),
  comment-preserving formatting, byte-exact programmatic editing
- **Schema packages** — bundle your tool's schemas (`.package.nml`,
  blake3-addressed), publish to a per-user store, and your users' editors
  validate your config files automatically
- **Editor-grade tooling** — LSP with completion, hover, go-to-definition,
  rename, quick fixes; your CLI can embed it as `<your-tool> lsp`

## CLI

```bash
# installs the `nml` binary (until the crates.io release, use:
#   cargo install --git https://github.com/nudge-io/nml nml-cli)
cargo install nml-cli

nml parse <file>                  # dump the AST as JSON (reports ALL errors)
nml validate <file>               # duplicates + unresolved references
nml fmt <file>                    # canonical formatting, comment-preserving
nml check --schema <dir> <file>   # full validation; non-zero exit for CI
```

## How it compares

| | NML | TOML | YAML + JSON Schema | CUE | Pkl | Dhall |
|---|---|---|---|---|---|---|
| Schema in the language | ✓ | — | bolted on | ✓ | ✓ | types |
| Secrets / money / duration types | ✓ | — | — | — | partial | — |
| Diff engine for live reload | ✓ | — | — | — | — | — |
| Embed in Rust (serde-native) | ✓ | ✓ | ✓ | via Go | via bindings | ✓ |
| Ship schemas + LSP to *your* users | ✓ | — | — | — | — | — |
| Maturity / ecosystem | pre-1.0 | ★★★ | ★★★ | ★★ | ★★ | ★★ |

**When NOT to use NML (yet):** you want a flat key-value file with no schema
needs (keep TOML); your host application isn't Rust (bindings are on the
roadmap, not shipped); or you need a decade-stable format today — NML is
pre-1.0 and syntax can still change (breaking changes ship with migration
fixers: the tooling tells you exactly what to rewrite, and `nml fmt` and
editor quick-fixes apply what can be applied mechanically).

## Documentation

| Start here | |
|---|---|
| [Tutorial](docs/tutorial/README.md) | Nine chapters, one growing config — from first file to shipped schemas |
| [Language Guide](docs/language-guide.md) | Writing NML — syntax and features |
| [Integration Guide](docs/integration.md) | Embedding NML in a Rust project |
| [Language Specification](spec/README.md) | Formal grammar and semantics, for implementers |
| [Error Index](docs/errors/README.md) | Every `NML0000` diagnostic code, with verified examples and fixes |
| [Editor Integration](docs/reference/editors.md) | LSP + VS Code surfaces: diagnostics, quick fixes, hover explanations, completion |
| [Stability Policy](docs/stability.md) | What pre-1.0 means here; breaking changes ship with fixers |
| [RFC index](docs/rfcs/README.md) | Language evolution and design records |

Docs examples are executable: tagged ```` ```nml ```` blocks run through the
real CLI in CI (`just docs-test`), every tutorial chapter's finished config
is a validated fixture, and the tutorial's Rust programs compile and run in
CI with their printed output asserted — so what you read is what the tools do.

## Project structure

```
crates/
  nml-core/       Parsing (lossless CST), AST, serde, query, resolution, diff
  nml-validate/   Schema validation, schema packages, per-user package store
  nml-fmt/        Canonical comment-preserving formatter
  nml-lsp/        Language server (native + wasm32-wasip1)
nml-cli/          The `nml` binary
editors/vscode/   VS Code extension (bundles the WASM language server)
spec/             Language specification
docs/             Guides, integration docs, RFCs
```

## Building

```bash
just test    # cargo test --workspace
just lint    # clippy, warnings denied
just install # build + install the LSP and VS Code extension locally
```

Minimum supported Rust: see `rust-version` in [Cargo.toml](Cargo.toml).
Contributions welcome — see [CONTRIBUTING](CONTRIBUTING.md).

## License

MIT OR Apache-2.0
