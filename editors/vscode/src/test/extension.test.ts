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

// RFC 0010 tier 2, end-to-end in the real editor: the `nml-explain:` content
// provider fetches the full entry from the running WASM server, and a real
// diagnostic surfaces the negotiated "Explain …" code action wired to
// `nml.explain`. (The suite above restores the committed schema in its
// teardown, so `app.nml`'s type mismatch is live again here.)
suite("nml explanations (E2E, WASM neutral server)", () => {
  /** Open app.nml and wait for its diagnostic — activates the extension and
   *  guarantees a coded diagnostic to hang assertions on. */
  async function openAppWithDiagnostic(): Promise<{
    app: vscode.Uri;
    diags: readonly vscode.Diagnostic[];
  }> {
    const app = vscode.Uri.joinPath(workspaceUri(), "app.nml");
    await vscode.window.showTextDocument(
      await vscode.workspace.openTextDocument(app)
    );
    const diags = await waitForDiagnostics(app, (d) => d.length > 0);
    return { app, diags };
  }

  test("the nml-explain provider serves the full entry from the running server", async () => {
    await openAppWithDiagnostic();
    const doc = await vscode.workspace.openTextDocument(
      vscode.Uri.parse("nml-explain:NML0013.md")
    );
    const text = doc.getText();
    assert.ok(
      text.startsWith("# NML0013"),
      `canonical heading expected, got: ${text.slice(0, 120)}`
    );
    assert.ok(
      text.includes("Invalid number"),
      `full entry body expected, got: ${text.slice(0, 200)}`
    );
  });

  test("a diagnostic offers the negotiated Explain code action", async () => {
    const { app, diags } = await openAppWithDiagnostic();
    const actions = await vscode.commands.executeCommand<vscode.CodeAction[]>(
      "vscode.executeCodeActionProvider",
      app,
      diags[0].range
    );
    const explain = (actions ?? []).find((a) => a.title.startsWith("Explain NML"));
    assert.ok(
      explain,
      `Explain action expected, got: ${JSON.stringify((actions ?? []).map((a) => a.title))}`
    );
    assert.strictEqual(explain!.command?.command, "nml.explain");
    const code = explain!.command?.arguments?.[0];
    assert.ok(
      typeof code === "string" && /^NML\d{4}$/.test(code),
      `canonical code argument expected, got: ${JSON.stringify(code)}`
    );
  });
});
