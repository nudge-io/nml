# Integrating NML into Your Project

This guide shows how to use NML as a configuration language in any Rust project.

## Add the Dependency

```toml
# Cargo.toml
[dependencies]
nml-core = "0.1"
serde = { version = "1", features = ["derive"] }  # optional, for struct deserialization
```

## Parse a Config File

```rust
use nml_core::{parse, Document};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = std::fs::read_to_string("config.nml")?;
    let file = parse(&source)?;
    let doc = Document::new(&file);

    // Query values
    let port = doc.block("service", "MyApp")
        .property("port")
        .as_f64()
        .unwrap_or(3000.0);

    let host = doc.block("service", "MyApp")
        .property("host")
        .as_str()
        .unwrap_or("localhost");

    println!("Starting on {host}:{port}");
    Ok(())
}
```

Example `config.nml`:

```nml
service MyApp:
    host = "0.0.0.0"
    port = 8080
    debug = true
    tags = ["web", "api"]

    database:
        url = "postgres://localhost/mydb"
        pool_size = 10
```

## Deserialize into Structs (Serde)

For typed access, deserialize NML blocks directly into Rust structs:

```rust
use serde::Deserialize;
use nml_core::{parse, Document};
use nml_core::de::from_block;

#[derive(Deserialize)]
struct ServiceConfig {
    host: String,
    port: f64,
    debug: bool,
    tags: Vec<String>,
}

fn load_config(path: &str) -> Result<ServiceConfig, Box<dyn std::error::Error>> {
    let source = std::fs::read_to_string(path)?;
    let file = parse(&source)?;
    let doc = Document::new(&file);
    let body = doc.block("service", "MyApp")
        .body()
        .ok_or("block 'service MyApp' not found")?;
    let config: ServiceConfig = from_block(body)?;
    Ok(config)
}
```

## Nested Blocks and Named Lists

The serde bridge handles nested NML structures automatically.

### Nested Blocks

A nested block deserializes into a nested struct field:

```nml
service MyApp:
    host = "0.0.0.0"
    port = 8080
    database:
        url = "postgres://localhost/mydb"
        pool_size = 10
```

```rust
#[derive(Deserialize)]
struct ServiceConfig {
    host: String,
    port: f64,
    database: DatabaseConfig,
}

#[derive(Deserialize)]
struct DatabaseConfig {
    url: String,
    pool_size: f64,
}
```

Optional nested blocks use `Option<T>`:

```rust
#[derive(Deserialize)]
struct ServiceConfig {
    host: String,
    database: Option<DatabaseConfig>,
}
```

### Named List Items

Named list items (`- ItemName: ...`) deserialize into a `Vec<T>` where each
item's label is injected as a `name` field:

```nml
workflow MyWorkflow:
    steps:
        - classify:
            provider = "openai"
        - respond:
            provider = "anthropic"
```

```rust
#[derive(Deserialize)]
struct WorkflowConfig {
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    name: String,       // injected from the list item label ("classify", "respond")
    provider: String,
}
```

If the item body contains an explicit `name` property, it takes precedence
over the injected label.

## Value Resolution

NML values like `$ENV.API_KEY` and fallback chains (`$ENV.KEY | "default"`)
need resolution before use. The `ValueResolver` handles this:

```rust
use nml_core::ValueResolver;

// Resolve from environment variables
let resolver = ValueResolver::env();

// Or provide a custom lookup function
let resolver = ValueResolver::new(|key| {
    match key {
        "API_KEY" => Some("sk-abc123".into()),
        _ => std::env::var(key).ok(),
    }
});
```

### Resolving Individual Values

```rust
use nml_core::types::Value;

let resolved = resolver.resolve(&value)?;
```

For `Value::Secret("$ENV.API_KEY")`, this returns `Value::String("sk-abc123")`.
For `Value::Fallback(primary, fallback)`, it tries the primary first, then
falls back.

### Resolving an Entire Body

```rust
let resolved_body = resolver.resolve_body(&body)?;
```

This recursively resolves all secrets and fallbacks within every property,
nested block, and list item.

### Combined Pipeline: `from_body_resolved`

For the common case of resolve + deserialize, use the combined pipeline:

```rust
use nml_core::de::from_body_resolved;

let config: ServiceConfig = from_body_resolved(body, &resolver)?;
```

This performs three steps in order:

1. **Value resolution** -- resolves `$ENV.X` secrets and fallback chains
2. **Shared property inheritance** -- merges `.key:` defaults into list items
3. **Serde deserialization** -- deserializes the resolved body into `T`

## Shared Property Inheritance

Shared properties (`.key:` syntax) define defaults that are inherited by all
list items in a block. Use `apply_shared_properties` to merge them before
manual processing, or use `from_body_resolved` which handles this
automatically:

```rust
use nml_core::resolve::apply_shared_properties;

let merged_body = apply_shared_properties(&body);
// All list items now include the shared property defaults
```

Items that define the same property override the shared default.

## Query API

The `Document` query API provides fluent access without serde:

```rust
let doc = Document::new(&file);

// Block properties
doc.block("database", "Primary").property("pool_size").as_f64();

// Nested blocks
doc.block("service", "MyApp").nested("database").property("url").as_str();

// Constants
doc.const_value("MaxRetries").as_f64();

// List all blocks of a kind
for (name, block) in doc.blocks("service") {
    println!("Found service: {name}");
}

// All declarations
for (keyword, name) in doc.declarations() {
    println!("{keyword} {name}");
}
```

## TryFrom Conversions

Extract typed values from the raw AST using `TryFrom<&Value>`:

```rust
use nml_core::types::Value;

let value = doc.block("service", "MyApp").property("port").value().unwrap();

// Supported conversions
let s: String    = value.try_into().unwrap();  // String, Secret, Path, Duration, Reference, RoleRef
let n: f64       = value.try_into().unwrap();  // Number
let i: i64       = value.try_into().unwrap();  // Number (truncated to integer)
let b: bool      = value.try_into().unwrap();  // Bool
let v: Vec<String> = value.try_into().unwrap(); // Array of strings
```

### Value Accessors

For quick access without `TryFrom`, use the accessor methods:

```rust
value.as_str();    // Option<&str> -- String, Path, Duration, Secret, Reference, RoleRef
value.as_f64();    // Option<f64>  -- Number
value.as_bool();   // Option<bool> -- Bool
value.as_array();  // Option<&[SpannedValue]> -- Array
```

## Schema Validation

Define models in `.model.nml` files and validate instances against them:

```nml
// schemas/service.model.nml
model service:
    host string
    port number
    debug bool?
```

```rust
use nml_core::model_extract;

let schema_source = std::fs::read_to_string("schemas/service.model.nml")?;
let schema_file = parse(&schema_source)?;
let schema = model_extract::extract(&schema_file);

let validator = nml_validate::schema::SchemaValidator::new(schema.models, schema.enums);
let diagnostics = validator.validate(&config_file);
for d in diagnostics {
    eprintln!("{}: {}", d.severity, d.message);
}
```

Or use the CLI:

```bash
nml check --schema schemas/ config.nml
```

## Project Configuration

Create an `nml-project.nml` at your workspace root to configure the NML tooling:

```nml
project MyProject:
    schema:
        - "schemas/service.model.nml"
        - "schemas/database.model.nml"
    templateNamespaces = ["env", "config", "args"]
    modifiers = ["allow", "deny", "readonly"]
    keywords = ["service", "database", "cache"]
```

This file is automatically detected by the NML language server and affects:

- **Schema validation**: Which `.model.nml` files to load
- **Template namespaces**: Which `{{namespace.key}}` prefixes are valid
- **Modifier names**: Which `|modifier = value` names are accepted
- **Keyword completions**: Which block keywords are suggested in the editor

## Custom Keywords

NML's parser is generic -- any identifier works as a block keyword:

```nml
database Primary:
    host = "localhost"
    port = 5432

cache Redis:
    host = "localhost"
    ttl = "30m"

pipeline DataSync:
    source = "Primary"
    schedule = "*/5 * * * *"
```

The VSCode extension highlights all block declarations, and the LSP
suggests keywords found in your workspace files.

## Editor Support

Install the NML VSCode extension for syntax highlighting, diagnostics,
completions, and hover information:

```bash
cd editors/vscode
just install-ext
```

The extension provides:

- Syntax highlighting for all NML constructs
- Real-time diagnostics (parse errors, schema validation)
- Autocomplete for keywords, types, and template expressions
- Hover information for properties and template variables
- Go-to-definition for references
