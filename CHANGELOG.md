# Changelog

## [0.1.0] - Unreleased

### Added

- Core parser with indentation-aware lexer
- AST types for all NML constructs (blocks, arrays, properties, modifiers, shared properties)
- Money type with ISO 4217 currency table and minor-unit storage
- Reference resolver with duplicate detection
- CLI with `parse`, `validate`, `fmt`, and `check` subcommands
- Canonical formatter with round-trip fidelity and idempotency
- LSP server with diagnostics, completion, hover, and go-to-definition
- VS Code extension with TextMate grammar for syntax highlighting
- Language specification (syntax, types, models, access control)
- Test fixtures for valid and invalid NML files

- **Template expressions**: `{{namespace.key}}` syntax in strings for dynamic interpolation
- **Fallback values**: `$ENV.KEY | "default"` pipe-chained fallback resolution
- **Template declarations**: `template Name:` for named string values
- **Const declarations**: `const Name = value` for file-level constants

- **Serde bridge** (`nml_core::de`): deserialize NML blocks into Rust structs
  - `from_block` -- deserialize a struct from an NML block body
  - `from_value` -- deserialize from a single NML value
  - `from_body_resolved` -- resolve + apply shared properties + deserialize pipeline
  - Recursive deserialization of nested blocks into nested structs
  - Named list item deserialization with automatic `name` field injection
  - Support for `Option<T>`, `Vec<T>`, enums, `camelCase` renaming

- **Value resolution** (`nml_core::resolve`):
  - `ValueResolver` with pluggable lookup (env vars or custom function)
  - Resolves `Value::Secret` (`$ENV.X`) and `Value::Fallback` chains
  - `resolve_body` / `resolve_array_body` for recursive resolution
  - `apply_shared_properties` -- merge `.key:` defaults into list items
  - `apply_array_shared_properties` -- same for array declarations

- **Query API** (`nml_core::query`):
  - `Document` wrapper with fluent block/property/nested lookups
  - `const_value`, `template_value`, `blocks`, `declarations` queries
  - `BlockQuery` and `ValueQuery` with `as_str`, `as_f64`, `as_bool` accessors

- **Value type conversions** (`nml_core::types`):
  - `TryFrom<&Value>` for `String`, `f64`, `i64`, `bool`, `Vec<String>`
  - Handles `Reference`, `RoleRef`, `Path`, `Duration`, `Secret` as string
  - `Value::as_str()`, `as_f64()`, `as_bool()`, `as_array()` accessors

- **Project configuration** (`nml-project.nml`):
  - `ProjectConfig` for schema files, template namespaces, modifiers, keywords
  - Auto-detected by LSP for workspace-aware validation

- **Model extraction** (`nml_core::model_extract`):
  - Extract model, enum, and trait definitions from parsed AST

- **LSP enhancements**:
  - Template expression validation (invalid namespace warnings)
  - Step reference validation in workflow files
  - Go-to-definition for keywords, references, and model fields

### Fixed

- Money `format_display` now correctly handles negative fractional amounts
  (e.g. `-$0.50` was previously displayed as `$0.50`)
- Serde bridge now uses `format_display()` for money values instead of raw
  minor units (previously serialized `1999 USD` instead of `19.99 USD`)
- Money values can now be deserialized into `String` fields via serde

### Documentation

- Language guide with complete feature coverage
- Integration guide for using NML in Rust projects
- Formal language specification with PEG grammar
- Template expression, fallback value, and const declaration documentation
