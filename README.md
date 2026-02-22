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
- **Secret references** -- `$ENV.MY_SECRET` resolved at runtime

## Quick Example

```
// Define a model
model service (accessControlled):
    localMount path
    resources []resource
    endpoints []endpoint

// Use it
service MyService:
    |allow:
        - @role/admin
        - @public
    localMount = "/"
    resources = myResources
```

## CLI

```bash
nml parse <file>       # Parse and dump AST as JSON
nml validate <file>    # Validate against model definitions
nml fmt <file>         # Format in canonical style
nml check <file>       # Parse + validate + report
```

## Project Structure

```
crates/
  nml-core/       Core parsing and AST library
  nml-validate/   Schema validation layer
  nml-fmt/        Canonical formatter
  nml-lsp/        Language Server Protocol implementation
nml-cli/          CLI binary
editors/vscode/   VS Code extension with syntax highlighting
spec/             Language specification
```

## Building

```bash
cargo build
cargo test
```

## License

MIT OR Apache-2.0
