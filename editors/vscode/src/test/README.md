# nml extension — end-to-end tests

Runs the **bundled** extension (`dist/extension.js`) in a real (headless) VS
Code via
[`@vscode/test-cli`](https://code.visualstudio.com/api/working-with-extensions/testing-extension).
`.vscode-test.mjs` defines two launches (each its own VS Code / workspace):

- **single-root** (`fixtures/ws`) — pull diagnostics + the **cross-file
  focus-heal** (edit a schema, a dependent instance re-pulls and clears, with no
  server-side background sweep).
- **multi-root** (`fixtures/multi.code-workspace`) — the per-folder
  `/workspaces/<name>` WASI mount + cross-folder schema resolution.

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

## Debugging the extension (F5)

The repo gitignores `.vscode/`, so editor launch config is each contributor's
own. To debug interactively: build the bundle (`npm run bundle:js`, and once,
the WASM server per the Prerequisites above — without it the neutral server
falls back to native `~/.cargo/bin/nml-lsp`), then add a personal
`editors/vscode/.vscode/launch.json`:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "name": "Run NML Extension",
      "type": "extensionHost",
      "request": "launch",
      "args": ["--extensionDevelopmentPath=${workspaceFolder}"],
      "outFiles": ["${workspaceFolder}/dist/**/*.js"]
    }
  ]
}
```
