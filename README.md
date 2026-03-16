# NML

**NML** (Nudge Markup Language) is an indentation-based configuration language with
a built-in type system, model definitions, and composable traits.

## Features

- **7 primitive types** -- `string`, `number`, `money`, `bool`, `duration`, `path`, `secret`
- **Model definitions** -- define custom object types with typed fields and constraints
- **Traits** -- reusable field groups (mixins) across models
- **Enums** -- restricted sets of allowed values
- **Access control** -- built-in `|allow` and `|deny` modifiers
- **Money type** -- exact currency values with ISO 4217 codes, stored as integer minor units
- **Secret references** -- `$ENV.MY_SECRET` resolved at runtime, with fallback chains
- **Template expressions** -- `{{namespace.key}}` for dynamic value interpolation
- **Serde integration** -- deserialize NML blocks directly into Rust structs
- **Constants** -- `const Name = value` for reusable values across a file
- **Schema validation** -- validate config instances against model definitions

## Quick Example

```
const DefaultPort = 8080

service MyApp:
    host = "0.0.0.0"
    port = DefaultPort
    apiKey = $ENV.API_KEY | "dev-key"
    greeting = "Hello, {{args.name}}!"

    database:
        url = $ENV.DATABASE_URL
        pool_size = 10

model service:
    host string
    port number
    apiKey secret
    greeting string

    database:
        url secret
        pool_size number
```

## CLI

```bash
nml parse <file>       # Parse and dump AST as JSON
nml validate <file>    # Validate against model definitions
nml fmt <file>         # Format in canonical style
nml check <file>       # Parse + validate + report
```

## Documentation

| Document | Description |
|----------|-------------|
| [Language Guide](docs/language-guide.md) | Complete guide to NML syntax and features |
| [Integration Guide](docs/integration.md) | Using NML as a config language in Rust projects |
| [Language Specification](spec/README.md) | Formal syntax, type system, and grammar |

## Project Structure

```
crates/
  nml-core/       Core parsing, AST, serde, query, and resolution library
  nml-validate/   Schema validation layer
  nml-fmt/        Canonical formatter
  nml-lsp/        Language Server Protocol implementation
nml-cli/          CLI binary
editors/vscode/   VS Code extension with syntax highlighting
spec/             Language specification
docs/             Guides and integration documentation
```

## Building

```bash
cargo build
cargo test
```

## License

MIT OR Apache-2.0
