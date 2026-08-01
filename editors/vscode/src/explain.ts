import {
  commands,
  ExtensionContext,
  QuickPickItem,
  TextDocumentContentProvider,
  Uri,
  window,
  workspace,
} from "vscode";
import { LanguageClient } from "vscode-languageclient/node";
import { isRecord, readString } from "./contracts/wire";

// ─────────────────────────────────────────────────────────────────────────
// RFC 0010 tier 2 — full error explanations in-editor.
//
// One command (`nml.explain`) serves both entry points: the server's
// "Explain NML0000" code action invokes it with a code (negotiated — the
// server only emits that action because activation declared
// `initializationOptions.explainCommand`); the command palette invokes it
// bare, which lists every code with its summary (searchable by either) and
// opens the chosen one. Entries render as virtual markdown documents on the
// `nml-explain:` scheme, fetched from the RUNNING language server
// (`nml/explain`) — so the explanation always comes from the exact binary
// that produced the diagnostic (no version skew, offline, any backend). The
// scheme structurally cannot read files: its only content source is the
// server's embedded compile-time index, and both sides validate the code
// shape before it goes anywhere.
// ─────────────────────────────────────────────────────────────────────────

const SCHEME = "nml-explain";
/** The only shape either side accepts; anything else never reaches a request. */
const CODE = /^NML\d{4}$/i;

interface IndexEntry {
  code: string;
  summary: string;
}

export function registerExplain(
  context: ExtensionContext,
  getClient: () => LanguageClient | undefined
): void {
  const provider: TextDocumentContentProvider = {
    provideTextDocumentContent: (uri) => explanationMarkdown(uri, getClient),
  };
  context.subscriptions.push(
    workspace.registerTextDocumentContentProvider(SCHEME, provider),
    commands.registerCommand("nml.explain", (code?: unknown) =>
      explain(code, getClient)
    )
  );
}

async function explain(
  code: unknown,
  getClient: () => LanguageClient | undefined
): Promise<void> {
  const direct =
    typeof code === "string" && CODE.test(code) ? code.toUpperCase() : undefined;
  const chosen = direct ?? (await pickCode(getClient));
  if (!chosen) {
    return; // dismissed, or no server — already surfaced to the user
  }
  const uri = Uri.parse(`${SCHEME}:${chosen}.md`);
  try {
    // Rendered beside the code — the entry is documentation, not source.
    await commands.executeCommand("markdown.showPreviewToSide", uri);
  } catch {
    // No markdown preview available: raw markdown still beats nothing.
    await window.showTextDocument(await workspace.openTextDocument(uri));
  }
}

/** The palette path: every code with its summary, searchable by both. */
async function pickCode(
  getClient: () => LanguageClient | undefined
): Promise<string | undefined> {
  const client = getClient();
  if (!client) {
    void window.showWarningMessage(
      "NML: the language server is not running, so the code index is unavailable."
    );
    return undefined;
  }
  let index: IndexEntry[] | undefined;
  try {
    const result = await client.sendRequest<unknown>("nml/explainIndex", {});
    // Trust nothing off the wire, even our own server's: anything but the
    // contracted array degrades to the same readable warning, never a throw.
    index = Array.isArray(result)
      ? result
          .filter(
            (e): e is Record<string, unknown> =>
              isRecord(e) &&
              readString(e, "code") !== undefined &&
              readString(e, "summary") !== undefined
          )
          .map((e) => ({
            code: readString(e, "code")!,
            summary: readString(e, "summary")!,
          }))
      : undefined;
  } catch {
    index = undefined;
  }
  if (!index || index.length === 0) {
    void window.showWarningMessage(
      "NML: the running language server does not provide the code index " +
        "(update it, or run `nml explain --list` in a terminal)."
    );
    return undefined;
  }
  const items: QuickPickItem[] = index.map((entry) => ({
    label: entry.code,
    description: entry.summary,
  }));
  const picked = await window.showQuickPick(items, {
    matchOnDescription: true,
    placeHolder: "Explain a diagnostic code (search by code or summary)",
  });
  return picked?.label;
}

/** Content for `nml-explain:NML0000.md` — the server's full index entry.
 *  Every failure is a readable markdown document, never a thrown error: the
 *  user asked to READ something, so degradation must stay readable. */
async function explanationMarkdown(
  uri: Uri,
  getClient: () => LanguageClient | undefined
): Promise<string> {
  const code = uri.path.replace(/\.md$/, "");
  if (!CODE.test(code)) {
    return "# Not an NML diagnostic code\n\nCodes look like `NML2007`.";
  }
  const canonical = code.toUpperCase();
  const client = getClient();
  if (!client) {
    return (
      `# ${canonical}\n\nThe NML language server is not running, so this ` +
      "entry cannot be fetched. Start it (open an `.nml` file), or run " +
      `\`nml explain ${canonical}\` in a terminal.`
    );
  }
  try {
    const result = await client.sendRequest<unknown>("nml/explain", {
      code: canonical,
    });
    if (isRecord(result) && typeof result.markdown === "string") {
      return result.markdown;
    }
    return `# ${canonical}\n\nNo such diagnostic code. See \`nml explain --list\`.`;
  } catch {
    return (
      `# ${canonical}\n\nThe running NML language server does not provide ` +
      "explanations (update it), or the request failed. Run " +
      `\`nml explain ${canonical}\` in a terminal for the same entry.`
    );
  }
}
