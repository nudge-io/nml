# NML Language Support

Schema-aware editing for the **NML** configuration language: diagnostics,
completions, hovers, go-to-definition, and duration-aware semantic
highlighting, validated against your project's schema.

## How it works

The extension ships a **neutral language server compiled to WebAssembly** and
runs it sandboxed via the WASI host — no separate binary to install, works
offline, on every desktop platform VS Code runs on. It validates:

- **committed schema** — drop a `<name>.package.nml` (+ its `*.model.nml`
  sources) in your repo and it is discovered automatically, and
- **your tool's schema** — a project may opt in (in `nml-project.nml`) to its
  build tool's own language server (`<tool> lsp`, e.g. `nudge lsp`), launched
  only in a **trusted** workspace, only from `PATH`, and only after you approve
  it once.

The status bar (bottom-right) names the active schema and where it came from.

## Settings

- `nml.server.path` — absolute path to a native `nml-lsp` binary to use instead
  of the bundled WASM server (machine-scoped: a repository cannot set it).
- `nml.trace.server` — log LSP protocol traffic to the trace output channel.

Host compatibility (`engines.vscode` vs `@types/vscode`) is documented in
[VSCODE-API.md](./VSCODE-API.md).

## Commands

- **NML: Restart Language Server**

## Security

NML is data, not code, so the server reads committed schema even in an untrusted
workspace. Launching a project's own tool as a language server is the only
trust-gated action, and it is refused for any workspace-resident binary.

Licensed MIT OR Apache-2.0.
