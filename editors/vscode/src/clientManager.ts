import { ExtensionContext, window, workspace } from "vscode";
import { Trace } from "vscode-languageclient/node";
import {
  CloseAction,
  ErrorAction,
  LanguageClient,
  LanguageClientOptions,
  RevealOutputChannelOn,
  ServerOptions,
  State,
} from "vscode-languageclient/node";
import { WasmProcess } from "@vscode/wasm-wasi/v1";
import { NmlLogs } from "./logging";
import { resolveServer } from "./providerDiscovery";
import { createWasmServer, wasmUriConverters } from "./serverAcquisition";
import { ClientLifecycleState } from "./contracts/lifecycle";

export class NmlClientManager {
  private client: LanguageClient | undefined;
  private wasmProcess: WasmProcess | undefined;
  private serverLabel = "";
  private clientState: State = State.Stopped;
  private connectionLost = false;
  private startFailureReported = false;
  private lifecycle: Promise<unknown> = Promise.resolve();

  constructor(
    private readonly context: ExtensionContext,
    private readonly logs: NmlLogs,
    private readonly onStateChange: () => void
  ) {}

  getClient(): LanguageClient | undefined {
    return this.client;
  }

  getServerLabel(): string {
    return this.serverLabel;
  }

  getClientState(): State {
    return this.clientState;
  }

  /** Single source of truth for status-bar lifecycle presentation. */
  getLifecycleState(): ClientLifecycleState {
    if (this.connectionLost) return "disconnected";
    const client = this.client;
    const state = this.clientState;
    if (!client) {
      if (state === State.Starting) return "starting";
      if (state === State.StartFailed) return "failed";
      return "absent";
    }
    if (state === State.Starting) return "starting";
    if (state === State.StartFailed) return "failed";
    if (state === State.Stopped) return "disconnected";
    return "running";
  }

  serialize(op: () => Promise<void>): Promise<void> {
    const next = this.lifecycle.then(op, op);
    this.lifecycle = next;
    return next;
  }

  async applyTraceSetting(): Promise<void> {
    const client = this.client;
    if (!client) return;
    const level = workspace
      .getConfiguration("nml")
      .get<string>("trace.server", "off");
    const trace =
      level === "verbose"
        ? Trace.Verbose
        : level === "messages"
          ? Trace.Messages
          : Trace.Off;
    await client.setTrace(trace);
  }

  async start(): Promise<void> {
    this.connectionLost = false;
    this.startFailureReported = false;

    const resolution = await resolveServer(this.context, this.logs);
    this.serverLabel = resolution.label;

    const serverOptions: ServerOptions =
      resolution.kind === "wasm"
        ? async () => {
            const server = await createWasmServer(resolution.module, this.logs);
            this.wasmProcess = server.process;
            return server.transports;
          }
        : { command: resolution.command, args: resolution.args };

    this.logs.info(`Starting NML LSP (${resolution.label})`);

    const clientOptions: LanguageClientOptions = {
      documentSelector: [{ scheme: "file", language: "nml" }],
      outputChannel: this.logs.client,
      traceOutputChannel: this.logs.trace,
      revealOutputChannelOn: RevealOutputChannelOn.Error,
      progressOnInitialization: true,
      initializationOptions: { explainCommand: "nml.explain" },
      initializationFailedHandler: (error) => {
        this.reportStartFailure(resolution.label, String(error));
        return false;
      },
      errorHandler: {
        error: (error) => {
          this.logs.error(`LSP connection error: ${error}`);
          return { action: ErrorAction.Continue };
        },
        closed: () => {
          this.logs.warn("Language server connection closed");
          void this.handleConnectionClosed();
          return { action: CloseAction.DoNotRestart };
        },
      },
      ...(resolution.kind === "wasm"
        ? {
            synchronize: {
              fileEvents: workspace.createFileSystemWatcher("**/*.nml"),
            },
            uriConverters: wasmUriConverters(),
          }
        : {}),
    };

    const client = new LanguageClient(
      "nml-lsp",
      "NML Language Server",
      serverOptions,
      clientOptions
    );

    client.onDidChangeState((event) => {
      this.clientState = event.newState;
      if (event.newState === State.Stopped && this.client === client) {
        this.connectionLost = true;
      }
      this.onStateChange();
    });

    this.client = client;
    this.clientState = State.Starting;
    this.onStateChange();

    try {
      await client.start();
      await this.applyTraceSetting();
    } catch (err) {
      this.reportStartFailure(resolution.label, String(err));
      this.client = undefined;
      this.clientState = State.StartFailed;
      this.onStateChange();
    }
  }

  private reportStartFailure(label: string, detail: string): void {
    if (this.startFailureReported) return;
    this.startFailureReported = true;
    this.logs.error(`Failed to start language server: ${detail}`);
    void window.showErrorMessage(
      `NML: failed to start the NML language server (${label}). ` +
        `Set nml.server.path to an nml-lsp binary, or install one.`
    );
  }

  private async handleConnectionClosed(): Promise<void> {
    this.connectionLost = true;
    const old = this.client;
    this.client = undefined;
    this.clientState = State.Stopped;
    if (old) await old.stop().catch(() => undefined);
    await this.terminateWasm();
    this.onStateChange();
  }

  private async terminateWasm(): Promise<void> {
    const proc = this.wasmProcess;
    this.wasmProcess = undefined;
    if (proc) await proc.terminate().catch(() => undefined);
  }

  async stop(): Promise<void> {
    this.connectionLost = false;
    const old = this.client;
    this.client = undefined;
    this.clientState = State.Stopped;
    if (old) await old.stop().catch(() => undefined);
    await this.terminateWasm();
    this.onStateChange();
  }

  async restart(): Promise<void> {
    await this.stop();
    await this.start();
  }
}
