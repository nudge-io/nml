import {
  ExtensionContext,
  StatusBarAlignment,
  StatusBarItem,
  ThemeColor,
  window,
} from "vscode";
import { parseSchemaInfoResult } from "./contracts/schemaInfo";
import { NmlClientManager } from "./clientManager";
import {
  buildStatusPresentation,
  SchemaInfoLookup,
} from "./statusPresentation";

export interface NmlStatusBar {
  refresh(manager: NmlClientManager): Promise<void>;
  scheduleRefresh(manager: NmlClientManager): void;
  dispose(): void;
}

export function createStatusBar(context: ExtensionContext): NmlStatusBar {
  const item: StatusBarItem = window.createStatusBarItem(
    StatusBarAlignment.Right,
    100
  );
  item.command = "nml.restartServer";
  context.subscriptions.push(item);

  let timer: ReturnType<typeof setTimeout> | undefined;

  async function refresh(manager: NmlClientManager): Promise<void> {
    const editor = window.activeTextEditor;
    const hasNml = Boolean(editor && editor.document.languageId === "nml");
    const lifecycle = manager.getLifecycleState();
    const serverLabel = manager.getServerLabel();

    let schemaLookup: SchemaInfoLookup = { kind: "skipped" };
    const client = manager.getClient();
    if (hasNml && client && lifecycle === "running") {
      try {
        const raw = await client.sendRequest<unknown>("nml/schemaInfo", {
          uri: editor!.document.uri.toString(),
        });
        schemaLookup = parseSchemaInfoResult(raw);
      } catch {
        schemaLookup = { kind: "unavailable" };
      }
    }

    const presentation = buildStatusPresentation(
      lifecycle,
      serverLabel,
      schemaLookup,
      hasNml
    );

    if (presentation.hidden) {
      item.hide();
      return;
    }

    item.text = presentation.text;
    item.tooltip = presentation.tooltip;
    item.backgroundColor = presentation.backgroundColorId
      ? new ThemeColor(presentation.backgroundColorId)
      : undefined;
    item.show();
  }

  return {
    refresh,
    scheduleRefresh(manager: NmlClientManager): void {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = undefined;
        void refresh(manager);
      }, 250);
    },
    dispose(): void {
      if (timer) clearTimeout(timer);
    },
  };
}
