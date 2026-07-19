import * as vscode from "vscode";

/** Poll `getDiagnostics` (populated by the pull client applying server reports)
 *  until `predicate` holds or the timeout elapses. Generous timeout: the first
 *  pull waits on extension activation + WASM instantiation. */
export async function waitForDiagnostics(
  uri: vscode.Uri,
  predicate: (d: readonly vscode.Diagnostic[]) => boolean,
  timeoutMs = 40_000
): Promise<readonly vscode.Diagnostic[]> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const diags = vscode.languages.getDiagnostics(uri);
    if (predicate(diags)) return diags;
    if (Date.now() > deadline) {
      throw new Error(
        `diagnostics predicate not met for ${uri.fsPath} within ${timeoutMs}ms; last: ${JSON.stringify(diags)}`
      );
    }
    await new Promise((r) => setTimeout(r, 250));
  }
}
