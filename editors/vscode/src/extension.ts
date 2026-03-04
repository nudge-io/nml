import {
  ExtensionContext,
  window,
  workspace,
} from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";
import * as path from "path";

let client: LanguageClient | undefined;

const DEFAULT_SERVER_PATH =
  (process.env.HOME || process.env.USERPROFILE || "") + "/.cargo/bin/nml-lsp";

function resolveServerPath(): string | undefined {
  const config = workspace.getConfiguration("nml");
  const configuredPath = config.get<string>("server.path", "");
  const serverPath = configuredPath || DEFAULT_SERVER_PATH;

  const inspected = config.inspect<string>("server.path");
  if (
    inspected?.workspaceValue &&
    !inspected?.globalValue
  ) {
    const basename = path.basename(serverPath);
    if (basename !== "nml-lsp" && basename !== "nml-lsp.exe") {
      window.showWarningMessage(
        `NML: ignoring workspace-level nml.server.path ("${serverPath}") ` +
          `because it does not point to an nml-lsp binary. ` +
          `Set the path in your User settings instead.`
      );
      return DEFAULT_SERVER_PATH;
    }
  }

  return serverPath;
}

export function activate(context: ExtensionContext) {
  const serverCommand = resolveServerPath();
  if (!serverCommand) {
    return;
  }

  const serverOptions: ServerOptions = {
    command: serverCommand,
    args: [],
  };

  const outputChannel = window.createOutputChannel("NML Language Server");
  outputChannel.appendLine(`Starting NML LSP: ${serverCommand}`);

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "nml" }],
    outputChannel,
  };

  client = new LanguageClient(
    "nml-lsp",
    "NML Language Server",
    serverOptions,
    clientOptions
  );

  client.start().catch((err) => {
    console.error(
      `NML: failed to start language server (${serverCommand}):`,
      err
    );
  });
}

export function deactivate(): Thenable<void> | undefined {
  if (client) {
    return client.stop();
  }
  return undefined;
}
