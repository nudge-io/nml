import { commands, ExtensionContext, languages, window, workspace } from "vscode";
import { NmlClientManager } from "./clientManager";
import { registerExplain } from "./explain";
import { forgetProviderApprovals } from "./providerDiscovery";
import { createNmlLogs } from "./logging";
import { createStatusBar, NmlStatusBar } from "./statusBar";

let clientManager: NmlClientManager | undefined;
let statusBar: NmlStatusBar | undefined;

/** The extension's public API, returned from `activate`.
 *
 *  Deliberately one accessor. Its reason to exist is the E2E gate: the
 *  real-editor suite runs the SAME assertions over the wasm backend and the
 *  native one, and without a way to read back which one is running, the
 *  native lane would pass identically if `nml.server.path` were ignored —
 *  a gate that cannot tell the two apart is not testing either. */
export interface NmlExtensionApi {
  /** The running server's label, as the status bar shows it. */
  getServerLabel(): string;
}

export async function activate(context: ExtensionContext): Promise<NmlExtensionApi> {
  const logs = createNmlLogs(context);
  const bar = createStatusBar(context);
  statusBar = bar;

  const manager = new NmlClientManager(context, logs, () => {
    void bar.refresh(manager);
  });
  clientManager = manager;

  registerExplain(context, () => manager.getClient());

  context.subscriptions.push(
    commands.registerCommand("nml.restartServer", () => manager.restart()),
    // Revoking has to be as easy as granting: one command, and it takes
    // effect immediately rather than at the next window reload.
    commands.registerCommand("nml.forgetProviderApprovals", async () => {
      const forgotten = await forgetProviderApprovals(context);
      await manager.restart();
      void window.showInformationMessage(
        forgotten === 0
          ? "NML: this workspace had no remembered language-server decision. " +
              "If a project declares one, you will be asked before it runs."
          : "NML: forgot this workspace's language-server decision. You will " +
              "be asked again the next time the project asks to use its own."
      );
    }),
    commands.registerCommand("nml.showServerLog", () => logs.showClient()),
    commands.registerCommand("nml.showServerTrace", () => logs.showTrace()),
    workspace.onDidGrantWorkspaceTrust(() => manager.restart()),
    workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("nml.trace.server")) {
        void manager.applyTraceSetting();
      }
      // A changed server path only takes effect on a fresh spawn. A burst
      // of configuration events COALESCES into one restart: the manager
      // holds an intent, not a queue of operations.
      if (e.affectsConfiguration("nml.server.path")) {
        void manager.restart();
      }
    }),
    window.onDidChangeActiveTextEditor(() => void bar.refresh(manager)),
    languages.onDidChangeDiagnostics((e) => {
      const active = window.activeTextEditor?.document.uri.toString();
      if (active && e.uris.some((u) => u.toString() === active)) {
        bar.scheduleRefresh(manager);
      }
    })
  );

  await manager.start();

  return { getServerLabel: () => manager.getServerLabel() };
}

export function deactivate(): Thenable<void> | undefined {
  statusBar?.dispose();
  const manager = clientManager;
  clientManager = undefined;
  statusBar = undefined;
  // VS Code gives this 5000 ms (`Promise.race([timeout(5000), deactivateAll])`
  // in extHostExtensionService.ts) and then stops waiting. `deactivate()`
  // therefore CANCELS an attempt in flight rather than queueing behind its
  // 15 s handshake budget, and tears down on a profile whose worst case is
  // asserted to fit inside the 5000 — see serverProcess.ts.
  return manager?.deactivate();
}
