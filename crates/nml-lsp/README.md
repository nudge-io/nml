# nml-lsp

Language server for **NML**, a typed, indentation-based configuration
language — and the crate that lets *your* CLI ship a full config-file editing
experience with one subcommand.

## Features

- Schema-driven completion (fields, enum values, discriminators, arm targets,
  directives), hover with doc comments, go-to-definition, references, rename,
  document symbols and highlights
- Formatting and on-type formatting (via `nml-fmt`, comment-preserving)
- Pull diagnostics (LSP 3.17) with machine-applicable did-you-mean quick-fix
  code actions
- Schema-package resolution: workspace manifests → per-user store →
  tool-embedded package → builtin, with a custom `nml/schemaInfo` method for
  editor status surfaces
- Builds natively **and** to `wasm32-wasip1` (the VS Code extension bundles
  the WASM server, so users need no separate install)

## Run standalone

```bash
cargo install nml-lsp
nml-lsp   # speaks LSP over stdio
```

## Embed in your tool

A tool that uses NML for its config files can expose the server itself, with
its own schemas built in — users who open your config files get completion
and validation for *your* models:

```rust
// the body of `<your-tool> lsp`
nml_lsp::serve(your_embedded_schema_package());
```

The NML VS Code extension discovers `<tool> lsp` providers declared in
`nml-project.nml` (behind workspace trust) and falls back to the neutral
bundled server otherwise.

Parsing lives in [`nml-core`](https://crates.io/crates/nml-core), validation
in [`nml-validate`](https://crates.io/crates/nml-validate).

## License

MIT OR Apache-2.0
