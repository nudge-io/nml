# nml-validate

Schema validation for **NML**, a typed, indentation-based configuration
language. Validate config files against `model` definitions and get
span-accurate diagnostics with machine-applicable fixes.

```rust
use nml_validate::loader::load_schema;
use nml_validate::schema::SchemaValidator;

let (schema, schema_diags) = load_schema(&[("service.model.nml", schema_source)]);
let validator = SchemaValidator::new(schema.models, schema.enums, schema.oneofs);

let diagnostics = validator.validate(&config_file);
for d in diagnostics {
    eprintln!("{}: {}", d.severity, d.message);
    // d.suggestion carries a machine-applicable replacement (did-you-mean),
    // d.span points at the exact source range.
}
```

## What's inside

- **Model checking** — required fields, types, defaults, enums, `oneof`
  discriminated unions, typed arm maps `(K -> V)`, `set<T>` shape and
  duplicate rejection, reference targets, modifier values.
- **Lenient and strict modes** — lenient treats unknown properties as
  warnings; `strict()` promotes them to errors and flags unmodeled blocks.
- **Did-you-mean** — Levenshtein suggestions for enum typos, returned as
  structured `Suggestion { replacement, span }` quick-fixes (the NML language
  server applies them as code actions).
- **Schema loading** — `loader::load_schema` extracts models/enums/oneofs
  from `.model.nml` sources, detects duplicates and inheritance cycles, and
  resolves model inheritance, best-effort.
- **Schema packages** — `SchemaPackage` bundles a manifest
  (`<name>.package.nml`) with its schemas, content-addressed by blake3;
  `Store` is a per-user package store with atomic publish and current-slot
  resolution (the mechanism a tool uses to ship its config schemas to users'
  editors).
- **Membership semantics** — opt-in RBAC-style reference and cycle rules for
  languages that model roles and grants.

Parsing lives in [`nml-core`](https://crates.io/crates/nml-core); the
`nml check --schema` CLI wrapper is
[`nml-cli`](https://crates.io/crates/nml-cli).

## Documentation

- [Integration guide](https://github.com/nudge-io/nml/blob/main/docs/integration.md)
- [Language guide](https://github.com/nudge-io/nml/blob/main/docs/language-guide.md)

## License

MIT OR Apache-2.0
