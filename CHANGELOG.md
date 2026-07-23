# Changelog

## [0.1.0] - Unreleased

### Added

- **Traits are real (RFC 0011)**: `trait name:` declares a non-instantiable
  mixin — same field syntax as a model (defaults, markers, modifiers,
  directives all travel), composed with `is` by models and other traits,
  and never a block keyword, field type, or `oneof` arm target. Previously
  `trait` parsed but was silently ignored by schema extraction. Five new
  stable codes with error-index entries: `NML2020` unknown `is` target
  (with a machine-applicable did-you-mean at the target token — `is`
  targets were previously *silently* unresolved), `NML2021` non-composable
  `is` target, `NML2022` trait as a field type, `NML2023` trait as a
  `oneof` arm, `NML2024` trait instantiation (an error even in lenient
  mode). Strict-mode keyword suggestions and editor keyword completion
  exclude traits; `trait` joins the language-keyword completions.
- `nml validate` now runs the full schema-finder pipeline on files that
  declare models/traits/enums/oneofs (reserved and duplicate names,
  composition, oneof integrity, positional arity, cycles) — previously
  these surfaced only through `check --schema`. Warnings report without
  failing the file, matching `check`.

- **Unified diagnostics model** (`nml-core::diagnostic`, RFC 0008): one
  `Diagnostic` type — severity (now incl. `Info`), span, source, structured
  suggestion, and a **stable error code** (`NML0000`-style, never renumbered
  or reused once released) — shared by the parser's error list, symbols,
  the validator, the LSP, and the CLI. One renderer, one LSP converter
  (replacing three hand-rolled bridges); the CLI prints rustc-style
  `error[NML2000]:` prefixes.
- New did-you-mean hints from core diagnostics: unresolved references
  (`DefaultPrt` → `DefaultPort`), unknown currency codes (`USE` → `USD`,
  from the ISO 4217 table), and unknown template namespaces — each with a
  machine-applicable quick-fix span.
- **Structured parse errors (RFC 0009 foundations)**: syntax errors carry a
  payload kind from which message, code, and fix derive; **`NML0001`
  Replaced syntax** is live — writing `=>` now yields a machine-applicable
  `->` fix in the CLI and as an editor quick-fix, with the error index
  carrying the migration ledger.
- **`nml explain NML2007`**: the error index is embedded in the binary
  (canonical file: `crates/nml-core/assets/error-index.md`, exposed as
  `nml_core::diagnostic::explain`), so explanations work offline; the CLI
  prints a rustc-style "for more information" hint after coded diagnostics.
- The oneof umbrella code split by fix-pattern: `NML2012` is now
  arm-references-unknown-model only, with `NML2015`–`NML2019` covering
  duplicate discriminator, name collision, bad default, non-enum
  discriminator type, and non-exhaustive enum arms.
- **Error index** (`docs/errors/README.md`): every stable code has a
  documented section, most with CI-verified examples; a bidirectional
  docs-test guard means a new code cannot ship without its documentation.
- `nml check --strict`: unknown properties and unmodeled keywords become
  errors (CI posture).
- Schema diagnostics from `nml check --schema` now locate as
  `file:line:col` against the attributed schema source instead of printing
  raw byte spans.
- **Changed:** every public findings API now returns `Vec<Diagnostic>` —
  `cst::parse_to_ast_all`, `cst::extract_schema`, the `SymbolTable` finders,
  and the schema-integrity finders (`find_oneof_errors`,
  `find_shorthand_errors`, `find_model_cycles`, `find_extends_cycles`) —
  with codes assigned (NML2007–NML2014 incl. missing-required-field,
  type-mismatch, duplicate-definition, reserved-type-name, cycle classes)
  and severity carried at the source; `nml_validate::diagnostics`/`::suggest` moved to
  `nml_core::diagnostic`/`::suggest` (no re-export shims); the LSP now
  reports duplicate/unresolved findings at their true severity (error),
  matching the CLI.
- **Unified did-you-mean engine** (`nml-core::suggest`): one OSA
  (restricted Damerau-Levenshtein) metric behind every suggestion, so
  transpositions (`"wran"` → `"warn"`) are caught; case-insensitive exact
  match wins outright; deterministic tie-breaking. Replaces the two divergent
  per-site suggesters.
- Did-you-mean suggestions (with machine-applicable quick-fix spans) at
  previously uncovered sites: unknown properties, unknown modifiers, unknown
  `oneof` discriminator values, and strict-mode unknown block keywords.
- `Diagnostic::rendered_message()`: the hint is derived from the structured
  suggestion by one renderer shared by the CLI, `Display`, and the LSP —
  producers no longer bake hint prose into messages. The `nml` CLI now prints
  hints for all suggesting diagnostics.

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

- `nml check`/`nml validate` did not detect const/template reference cycles
  (the LSP did) — now reported as `NML1002`, caught by the error index's own
  verified example
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
- README rewritten around a CI-verified 30-second demo, an honest comparison
  table ("when NOT to use NML" included), and a Rust embed quickstart
- Docs verification harness (`just docs-test` + CI job): tagged ```` ```nml ````
  blocks in the Markdown docs run through the real CLI, including asserted
  error examples, so documentation examples cannot rot
- Per-crate READMEs, CONTRIBUTING (docs-required gate), SECURITY policy,
  PR template, RFC status index, and a stability & compatibility policy
  (`docs/stability.md`); workspace `rust-version` declared
- **Nine-chapter tutorial** (`docs/tutorial/`): one growing status-page
  config from first parse through schemas, composition, `oneof`, access
  control, the Rust embedding pipeline, a diff-driven reload classifier,
  and shipped schema packages. Every chapter's finished config is a
  CI-validated fixture; chapters 7–9 are workspace crates the docs harness
  compiles **and runs**, asserting the output printed on the page; full
  Rust listings on pages are CI-checked to be verbatim excerpts of the
  compiled programs (```` ```rust source=<file> ```` guard)
