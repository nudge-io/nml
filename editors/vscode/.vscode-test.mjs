import { defineConfig } from "@vscode/test-cli";

// End-to-end tests for the nml extension in a real (headless) VS Code, driven
// by @vscode/test-cli. These exercise the DEFAULT backend — the bundled WASM
// neutral server (RFC 0035) — in the actual editor: real pull-diagnostics
// round-trips, real cross-file focus-heal, and the WASI URI mapping. The native
// backend is a deferred performance tier (RFC 0035 §"defer native").
//
// Two launches (each config = its own VS Code instance / workspace):
//   • single-root — pull diagnostics + cross-file focus-heal.
//   • multi-root  — per-folder `/workspaces/<name>` mount + cross-folder schema.
//
// Prerequisites (CI): the WASM server must be bundled first
//   cargo build -p nml-lsp --target wasm32-wasip1 --release
//   npm run bundle:wasm && npm run compile
// The WASI host extension is auto-installed into each test instance below.
const wasi = { installExtensions: ["ms-vscode.wasm-wasi-core"], mocha: { ui: "tdd", timeout: 60_000 } };

export default defineConfig([
  {
    label: "single-root",
    files: "out/test/extension.test.js",
    workspaceFolder: "src/test/fixtures/ws",
    ...wasi,
  },
  {
    label: "multi-root",
    files: "out/test/multiroot.test.js",
    workspaceFolder: "src/test/fixtures/multi.code-workspace",
    ...wasi,
  },
]);
