import * as assert from "node:assert";
import * as vscode from "vscode";
import {
  assertConfiguredBackend,
  configuredBackend,
  diagnosticCode,
  suiteTimeoutMs,
  waitForDiagnostics,
} from "./util";

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

suite(`nml pull diagnostics (E2E, ${configuredBackend()} neutral server)`, function () {
  // The cross-file heal below holds TWO diagnostics waits; the launch config's
  // 60 000 ms bounds the whole test, so the second could never reach its own
  // deadline and say what it was waiting for.
  this.timeout(suiteTimeoutMs(2));

  // Keep the suite order-independent and re-runnable: restore the committed
  // schema after the mutating test (buffer only — disk is never written).
  suiteTeardown(async () => {
    await setModel(MODEL_NUMBER);
  });

  test("the backend under test is the one this launch configured", async () => {
    // First, and on purpose: every assertion after this one would pass on
    // EITHER backend, so this is what stops the native lane from being a
    // second wasm run wearing a different label.
    const label = await assertConfiguredBackend();
    assert.ok(label.length > 0, "the extension must name the server it started");
  });

  test("pulls a type-mismatch diagnostic for an instance file", async () => {
    const app = vscode.Uri.joinPath(workspaceUri(), "app.nml");
    await vscode.window.showTextDocument(await vscode.workspace.openTextDocument(app));
    const diags = await waitForDiagnostics(app, (d) => d.length > 0);
    // The CODE, not merely "something arrived". `d.length > 0` is satisfied
    // by ANY diagnostic this server can produce — a parse error, a schema it
    // could not read, a universe that came back open — and every one of
    // those would mean the cross-file type check this test is named for
    // never ran. NML2008 is what `port = "x"` is against `port number`,
    // MEASURED against this fixture on both backends.
    assert.deepStrictEqual(
      diags.map(diagnosticCode),
      ["NML2008"],
      `expected exactly the type mismatch on app.nml, got: ${JSON.stringify(diags)}`
    );
    assert.deepStrictEqual(
      diags.map((d) => d.source),
      ["nml"],
      "a diagnostic from somewhere other than the NML server"
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
// provider fetches the full entry from the running server, and a real
// diagnostic surfaces the negotiated "Explain …" code action wired to
// `nml.explain`. (The suite above restores the committed schema in its
// teardown, so `app.nml`'s type mismatch is live again here.)
//
// Both tests take the code from the LIVE diagnostic rather than naming one.
// A hardcoded code made this a round trip to the server and back with the
// diagnostic left out of it: the fixture produces NML2008 and the test asked
// for NML0013, so a server that answered the wrong entry — or a code action
// that carried a code no one could explain — read exactly the same here.
suite(`nml explanations (E2E, ${configuredBackend()} neutral server)`, function () {
  this.timeout(suiteTimeoutMs(1));

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

  test("the nml-explain provider serves the full entry for the diagnostic on screen", async () => {
    const { diags } = await openAppWithDiagnostic();
    const code = diagnosticCode(diags[0]);
    const doc = await vscode.workspace.openTextDocument(
      vscode.Uri.parse(`nml-explain:${code}.md`)
    );
    const text = doc.getText();
    assert.ok(
      text.startsWith(`# ${code}\n`),
      `the entry for the code on screen was expected, got: ${text.slice(0, 120)}`
    );
    // The FULL entry, not the one-line headline the explain INDEX carries
    // for the same code — which is the other thing this provider could
    // plausibly have served.
    assert.match(
      text,
      /\*\*Fix:\*\*/,
      `the full entry body was expected, got: ${text.slice(0, 300)}`
    );
  });

  test("a diagnostic offers the negotiated Explain code action", async () => {
    const { app, diags } = await openAppWithDiagnostic();
    const actions = await vscode.commands.executeCommand<vscode.CodeAction[]>(
      "vscode.executeCodeActionProvider",
      app,
      diags[0].range
    );
    const expected = diagnosticCode(diags[0]);
    const explain = (actions ?? []).find((a) => a.title.startsWith("Explain NML"));
    assert.ok(
      explain,
      `Explain action expected, got: ${JSON.stringify((actions ?? []).map((a) => a.title))}`
    );
    assert.strictEqual(explain!.title, `Explain ${expected}`);
    assert.strictEqual(explain!.command?.command, "nml.explain");
    // The DIAGNOSTIC's code, not merely a well-shaped one: `/^NML\d{4}$/`
    // is satisfied by any code the server cares to send, including one that
    // explains something else entirely.
    assert.strictEqual(explain!.command?.arguments?.[0], expected);
  });
});
