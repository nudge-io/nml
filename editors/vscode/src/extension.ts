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

let client: LanguageClient | undefined;

const DEFAULT_SERVER_PATH =
  (process.env.HOME || process.env.USERPROFILE || "") + "/.cargo/bin/nml-lsp";

export function activate(context: ExtensionContext) {
  const configuredPath = workspace
    .getConfiguration("nml")
    .get<string>("server.path", "");
  const serverCommand = configuredPath || DEFAULT_SERVER_PATH;

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
