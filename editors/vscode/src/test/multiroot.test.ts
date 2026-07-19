import * as assert from "node:assert";
import * as vscode from "vscode";
import { waitForDiagnostics } from "./util";

// Multi-root E2E (RFC 0035): proves the WASM backend's per-folder WASI mount +
// `wasmUriConverters` are correct across MORE THAN ONE folder — the case that
// exercises the `/workspaces/<name>` mount naming (single-folder uses the
// simpler `/workspace`). The schema lives in folder A, the instance in folder
// B: a diagnostic can only appear if BOTH folders are mounted and read, and
// each folder's URIs map to the right mount. Fixture: `multi.code-workspace`.

function folderByName(name: string): vscode.WorkspaceFolder {
  const f = vscode.workspace.workspaceFolders?.find((w) => w.name === name);
  assert.ok(f, `workspace folder '${name}' must be open`);
  return f;
}

suite("nml pull diagnostics (E2E, WASM neutral server, MULTI-ROOT)", () => {
  test("resolves a cross-folder schema (per-folder /workspaces/<name> mount)", async () => {
    // The whole point — confirm this really opened as a multi-root workspace.
    assert.strictEqual(
      vscode.workspace.workspaceFolders?.length,
      2,
      "fixture must open as a 2-folder multi-root workspace"
    );
    // `app.nml` is in folder B; its `server` model is in folder A. A diagnostic
    // proves both folders are mounted at `/workspaces/<name>` and read, and that
    // each folder's URIs are converted to the correct mount.
    const app = vscode.Uri.joinPath(folderByName("multi-b").uri, "app.nml");
    await vscode.window.showTextDocument(await vscode.workspace.openTextDocument(app));
    const diags = await waitForDiagnostics(app, (d) => d.length > 0);
    assert.ok(
      diags.length > 0,
      `expected a cross-folder type diagnostic on app.nml, got: ${JSON.stringify(diags)}`
    );
  });
});
