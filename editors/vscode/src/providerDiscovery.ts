import { constants as fsConstants, promises as fsp } from "fs";
import * as path from "path";
import { ExtensionContext, Uri, window, workspace } from "vscode";
import { isValidToolName, parseProviderTool } from "./contracts/providerProject";
import { isPathInsideWorkspace } from "./pathSecurity";
import { NmlLogs } from "./logging";
import { NeutralServer, resolveNeutralServer } from "./serverAcquisition";

export type ServerResolution = NeutralServer;

async function declaredProviderTool(): Promise<string | undefined> {
  const folders = workspace.workspaceFolders ?? [];
  const found = new Set<string>();
  for (const folder of folders) {
    let text: string;
    try {
      const bytes = await workspace.fs.readFile(
        Uri.joinPath(folder.uri, "nml-project.nml")
      );
      text = new TextDecoder().decode(bytes);
    } catch {
      continue;
    }
    const tool = parseProviderTool(text);
    if (tool && isValidToolName(tool)) found.add(tool);
  }
  return found.size === 1 ? [...found][0] : undefined;
}

async function resolveOnPath(tool: string): Promise<string | undefined> {
  const exts =
    process.platform === "win32"
      ? (process.env.PATHEXT ?? ".EXE;.CMD;.BAT").split(";")
      : [""];
  for (const dir of (process.env.PATH ?? "").split(path.delimiter)) {
    if (!dir) continue;
    for (const ext of exts) {
      const candidate = path.join(dir, tool + ext);
      try {
        await fsp.access(candidate, fsConstants.X_OK);
        return candidate;
      } catch {
        /* keep looking */
      }
    }
  }
  return undefined;
}

function workspaceRoots(): string[] {
  return (workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath);
}

/** The discovery ladder (RFC 0035). */
export async function resolveServer(
  context: ExtensionContext,
  logs: NmlLogs
): Promise<ServerResolution> {
  const tool = await declaredProviderTool();
  if (!tool) return resolveNeutralServer(context, logs);

  if (!workspace.isTrusted) return resolveNeutralServer(context, logs);

  const command = await resolveOnPath(tool);
  if (!command || (await isPathInsideWorkspace(command, workspaceRoots()))) {
    return resolveNeutralServer(context, logs);
  }

  const key = `nml.approvedProvider.${tool}`;
  const remembered = context.workspaceState.get<string>(key);
  if (remembered !== command) {
    const choice = await window.showInformationMessage(
      `This project asks to use "${tool}" as its NML language server ` +
        `(resolved to ${command}). Use it?`,
      "Use it",
      "Use neutral server"
    );
    if (choice !== "Use it") return resolveNeutralServer(context, logs);
    await context.workspaceState.update(key, command);
  }

  return {
    kind: "process",
    command,
    args: ["lsp"],
    label: `${tool} (in-binary)`,
  };
}
