# nml-cli

Command-line tools for **NML**, a typed, indentation-based configuration
language. Installs the `nml` binary:

```bash
cargo install nml-cli
```

```bash
nml parse <file>                  # parse and dump the AST as JSON (reports ALL errors)
nml validate <file>               # duplicate declarations + unresolved references
nml fmt <file>                    # format in place, canonical style (atomic write)
nml check [--schema <dir>] <file> # CI-friendly: parse + validate + schema checks
```

`nml check --schema <dir>` loads every `*.model.nml` / `*.schema.nml` in the
directory and validates the file against them — exit code is non-zero on
errors, so it drops straight into CI:

```bash
nml check --schema schemas/ config.nml
```

The library APIs live in [`nml-core`](https://crates.io/crates/nml-core)
(parsing, query, serde) and
[`nml-validate`](https://crates.io/crates/nml-validate) (schema validation);
editor support is [`nml-lsp`](https://crates.io/crates/nml-lsp).

## Documentation

- [Language guide](https://github.com/nicknudge/nml/blob/main/docs/language-guide.md)
- [Integration guide](https://github.com/nicknudge/nml/blob/main/docs/integration.md)

## License

MIT OR Apache-2.0
