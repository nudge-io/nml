import { Disposable, ExtensionContext, window, workspace } from "vscode";
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
import { ClientLifecycleState, LspClientStateValue } from "./contracts/lifecycle";
import { resolveLifecycleState } from "./lifecycleState";
import { NmlLogs } from "./logging";
import { resolveServer } from "./providerDiscovery";
import { createWasmServer, wasmUriConverters } from "./serverAcquisition";

/** The manager's outward seams, constructor-injectable so the unit harness can
 *  drive the lifecycle with a fake client/process (no extension host).
 *  Production always uses [`productionDeps`]. */
export interface NmlClientManagerDeps {
  readonly resolveServer: typeof resolveServer;
  readonly createWasmServer: typeof createWasmServer;
  readonly createLanguageClient: (
    id: string,
    name: string,
    serverOptions: ServerOptions,
    clientOptions: LanguageClientOptions
  ) => LanguageClient;
}

const productionDeps: NmlClientManagerDeps = {
  resolveServer,
  createWasmServer,
  createLanguageClient: (id, name, serverOptions, clientOptions) =>
    new LanguageClient(id, name, serverOptions, clientOptions),
};

export class NmlClientManager {
  private client: LanguageClient | undefined;
  private wasmProcess: WasmProcess | undefined;
  private stateListener: Disposable | undefined;
  private serverLabel = "";
  private clientState: State = State.Stopped;
  private connectionLost = false;
  private startFailureReported = false;
  private lifecycle: Promise<unknown> = Promise.resolve();

  constructor(
    private readonly context: ExtensionContext,
    private readonly logs: NmlLogs,
    private readonly onStateChange: () => void,
    private readonly deps: NmlClientManagerDeps = productionDeps
  ) {}

  getClient(): LanguageClient | undefined {
    return this.client;
  }

  getServerLabel(): string {
    return this.serverLabel;
  }

  getLifecycleState(): ClientLifecycleState {
    return resolveLifecycleState({
      connectionLost: this.connectionLost,
      hasClient: this.client !== undefined,
      clientState: this.clientState as LspClientStateValue,
    });
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

    const resolution = await this.deps.resolveServer(this.context, this.logs);
    this.serverLabel = resolution.label;

    const serverOptions: ServerOptions =
      resolution.kind === "wasm"
        ? async () => {
            const server = await this.deps.createWasmServer(
              resolution.module,
              this.logs
            );
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
        void this.failStart(resolution.label, String(error));
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

    const client = this.deps.createLanguageClient(
      "nml-lsp",
      "NML Language Server",
      serverOptions,
      clientOptions
    );

    this.stateListener?.dispose();
    this.stateListener = client.onDidChangeState((event) => {
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
      await this.failStart(resolution.label, String(err), client);
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

  private async failStart(
    label: string,
    detail: string,
    client?: LanguageClient
  ): Promise<void> {
    this.reportStartFailure(label, detail);
    const old = client ?? this.client;
    this.client = undefined;
    this.releaseStateListener();
    this.clientState = State.StartFailed;
    if (old) await old.stop().catch(() => undefined);
    await this.terminateWasm();
    this.onStateChange();
  }

  private async handleConnectionClosed(): Promise<void> {
    this.connectionLost = true;
    const old = this.client;
    this.client = undefined;
    this.releaseStateListener();
    this.clientState = State.Stopped;
    if (old) await old.stop().catch(() => undefined);
    await this.terminateWasm();
    this.onStateChange();
  }

  // The per-start subscription must not outlive its client: a leaked listener
  // keeps reporting a dead client's transitions into the manager's state.
  private releaseStateListener(): void {
    this.stateListener?.dispose();
    this.stateListener = undefined;
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
    this.releaseStateListener();
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
