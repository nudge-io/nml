import * as assert from "node:assert";
import * as vscode from "vscode";
import { waitForDiagnostics } from "./util";

// End-to-end tests against the real editor + the bundled WASM neutral server
// (RFC 0035). The headline is the CROSS-FILE FOCUS-HEAL: the whole point of the
// pull-diagnostics migration is that editing a schema and re-focusing a
// dependent instance re-pulls and heals it — with no server-side background
// sweep. These tests are the empirical check the design review deferred to a
// running editor.

const MODEL_NUMBER = "model server:\n    port number\n";
const MODEL_STRING = "model server:\n    port string\n";

function workspaceUri(): vscode.Uri {
  const folder = vscode.workspace.workspaceFolders?.[0];
  assert.ok(folder, "a fixture workspace folder must be open");
  return folder.uri;
}

/** Replace `core.model.nml`'s buffer (no save — the server tracks didChange). */
async function setModel(text: string): Promise<void> {
  const model = vscode.Uri.joinPath(workspaceUri(), "core.model.nml");
  const doc = await vscode.workspace.openTextDocument(model);
  const edit = new vscode.WorkspaceEdit();
  const whole = new vscode.Range(
    doc.positionAt(0),
    doc.positionAt(doc.getText().length)
  );
  edit.replace(model, whole, text);
  assert.ok(await vscode.workspace.applyEdit(edit), "schema edit must apply");
}

suite("nml pull diagnostics (E2E, WASM neutral server)", () => {
  // Keep the suite order-independent and re-runnable: restore the committed
  // schema after the mutating test (buffer only — disk is never written).
  suiteTeardown(async () => {
    await setModel(MODEL_NUMBER);
  });

  test("pulls a type-mismatch diagnostic for an instance file", async () => {
    const app = vscode.Uri.joinPath(workspaceUri(), "app.nml");
    await vscode.window.showTextDocument(await vscode.workspace.openTextDocument(app));
    const diags = await waitForDiagnostics(app, (d) => d.length > 0);
    assert.ok(
      diags.length > 0,
      `expected a diagnostic on app.nml (string for number), got: ${JSON.stringify(diags)}`
    );
  });

  test("cross-file heal of a NON-active dependent: schema edit re-pulls it automatically", async () => {
    const app = vscode.Uri.joinPath(workspaceUri(), "app.nml");
    const appDoc = await vscode.workspace.openTextDocument(app);
    await vscode.window.showTextDocument(appDoc);
    await waitForDiagnostics(app, (d) => d.length > 0);

    // Make the SCHEMA the active editor, so `app.nml` is an OPEN but NON-ACTIVE
    // dependent. Fixing the schema must heal app with NO app focus — the client
    // background-re-pulls open dependents under `inter_file_dependencies: true`.
    // This is the empirical proof that cross-file heal is not focus-gated.
    const model = vscode.Uri.joinPath(workspaceUri(), "core.model.nml");
    await vscode.window.showTextDocument(
      await vscode.workspace.openTextDocument(model)
    );
    await setModel(MODEL_STRING);

    // Note: app is never re-shown. It heals in the background.
    const healed = await waitForDiagnostics(app, (d) => d.length === 0);
    assert.strictEqual(
      healed.length,
      0,
      "a non-active open dependent must heal after the schema edit, without focus"
    );
  });
});
