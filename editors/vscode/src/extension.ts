import { commands, ExtensionContext, languages, window, workspace } from "vscode";
import { NmlClientManager } from "./clientManager";
import { registerExplain } from "./explain";
import { createNmlLogs } from "./logging";
import { createStatusBar, NmlStatusBar } from "./statusBar";

let clientManager: NmlClientManager | undefined;
let statusBar: NmlStatusBar | undefined;

export async function activate(context: ExtensionContext): Promise<void> {
  const logs = createNmlLogs(context);
  const bar = createStatusBar(context);
  statusBar = bar;

  const manager = new NmlClientManager(context, logs, () => {
    void bar.refresh(manager);
  });
  clientManager = manager;

  registerExplain(context, () => manager.getClient());

  context.subscriptions.push(
    commands.registerCommand("nml.restartServer", () =>
      manager.serialize(() => manager.restart())
    ),
    commands.registerCommand("nml.showServerLog", () => logs.showClient()),
    commands.registerCommand("nml.showServerTrace", () => logs.showTrace()),
    workspace.onDidGrantWorkspaceTrust(() =>
      manager.serialize(() => manager.restart())
    ),
    workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("nml.trace.server")) {
        void manager.applyTraceSetting();
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

  await manager.serialize(() => manager.start());
}

export function deactivate(): Thenable<void> | undefined {
  statusBar?.dispose();
  const stop = clientManager?.stop();
  clientManager = undefined;
  statusBar = undefined;
  return stop;
}
