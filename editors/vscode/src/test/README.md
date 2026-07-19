# nml extension — end-to-end tests

Runs the extension in a real (headless) VS Code via
[`@vscode/test-cli`](https://code.visualstudio.com/api/working-with-extensions/testing-extension),
against the fixture workspace in `fixtures/ws`. These are the empirical check
for the RFC 0035 **pull-diagnostics** model — most importantly the **cross-file
focus-heal**: edit a schema, re-focus a dependent instance, and its diagnostics
re-pull and clear with no server-side background sweep.

## Prerequisites

The default suite exercises the **bundled WASM neutral server**, so the module
must be built and bundled first:

```sh
# from the repo root
cargo build -p nml-lsp --target wasm32-wasip1 --release
# from editors/vscode
npm install
npm test          # pretest bundles the .wasm and compiles; vscode-test runs it
```

`@vscode/test-cli` downloads a throwaway VS Code into `.vscode-test/` and
auto-installs `ms-vscode.wasm-wasi-core` (the WASI host the WASM backend needs).

## Native backend (deferred tier)

Native is a deferred performance tier (RFC 0035 §"defer native"), not a launch
gate, so it is not wired into the default config. To exercise it, build the
native `nml-lsp`, point `nml.server.path` (a `machine`-scoped user setting) at
it via a prepared `--user-data-dir`, and re-run — the same assertions hold, the
only difference is transport.
