import { Disposable, Uri, workspace } from "vscode";
import {
  CloseAction,
  ErrorAction,
  LanguageClient,
  LanguageClientOptions,
  RevealOutputChannelOn,
  ServerOptions,
  State,
} from "vscode-languageclient/node";
import { ClientLifecycleState, LspClientStateValue } from "./contracts/lifecycle";
import { resolveLifecycleState } from "./lifecycleState";
import { NmlLogs } from "./logging";
import type { LaunchLog, LaunchedServer } from "./processLaunch";
import { HandshakeOutcome, judgeServerIdentity } from "./providerTrust";
import { WasmServer, wasmUriConverters } from "./serverAcquisition";
import type {
  ProcessServer,
  ServerIdentityContract,
  ServerResolution,
} from "./serverResolution";
import {
  ServerProcess,
  TerminationProfile,
  TerminationVerdict,
  exitNote,
  settledWithin,
} from "./serverProcess";

// ─────────────────────────────────────────────────────────────────────────
// ONE OBJECT PER LAUNCH ATTEMPT.
//
// A session owns everything one attempt creates: the process, the
// `LanguageClient`, the state subscription, the label, and an `AbortController`
// that cancels it. When the manager moves on, it RETIRES the session — and from
// that moment every callback the session ever handed to the language client is
// INERT, because each one is a closure over this object and passes through
// [`live`].
//
// THE DEFECT THIS SHAPE EXISTS TO CLOSE (measured, twice): the language client
// calls back on an ABANDONED client long after the extension moved on — both
// `errorHandler.closed()` and `initializationFailedHandler` fire when a
// never-answering provider's process finally dies. Handlers that reached for
// `this.client` acted on whatever was current BY THEN, i.e. the healthy
// fallback: it was stopped, and a second, false "failed to start" toast was
// raised. Guarding each handler with `if (this.client === client)` is the same
// bug waiting for the next handler to be added. A retired session simply has
// nothing to act on.
// ─────────────────────────────────────────────────────────────────────────

/** How long ANY server gets to answer `initialize` before the editor stops
 *  waiting for it — the provider a repository declared, the bundled backend
 *  and an `nml.server.path` binary alike.
 *
 *  One number is safe because the handshake's cost does not grow with the
 *  workspace: `nml-lsp` answers `initialize` from its capabilities alone and
 *  indexes the workspace in `initialized`, after the answer (pinned by order
 *  in `crates/nml-lsp/tests/harness.rs`). What the budget bounds is the
 *  program starting and speaking at all, and what defeats it is a loaded
 *  host: this project has measured its own handshake missing a 20 s
 *  watchdog at load 131. A budget that expires on a busy machine produces
 *  a false accusation, and a false accusation is worse than no check — it
 *  teaches the operator that the check is noise. Fifteen seconds is long
 *  enough that expiry means "this is not answering", short enough that the
 *  editor is not silently dead, and a stop or restart asked for meanwhile
 *  does not wait for it (the attempt is cancelled). */
export const INITIALIZE_BUDGET_MS = 15_000;

/** What one attempt came to. */
export type SessionOutcome =
  | { readonly kind: "running" }
  /** The program could not be run, or never reached `Running`. */
  | { readonly kind: "failed"; readonly detail: string }
  /** A server with NO identity contract (the bundled backend, an
   *  `nml.server.path` binary) did not answer `initialize` inside the
   *  budget. There is no approval to judge and nothing to fall back to: it
   *  is a start failure, and the operator is told so. */
  | { readonly kind: "unanswered"; readonly budgetMs: number }
  /** A program launched on a repository's say-so ran, and the handshake it
   *  is held to did not identify it — judged against `identity`. */
  | {
      readonly kind: "handshake";
      readonly identity: ServerIdentityContract;
      readonly outcome: Exclude<HandshakeOutcome, { kind: "identified" }>;
    }
  /** A newer intent superseded this attempt; nothing is to be reported. */
  | { readonly kind: "aborted" };

/** What a session needs from the world around it. */
export interface SessionHost {
  readonly logs: NmlLogs;
  /** The server went away without being asked to. Called at most once, and
   *  never after the session is retired. */
  connectionLost(session: ServerSession): void;
  /** Anything the status bar would repaint for. */
  stateChanged(): void;
}

/** The seams a session is built over — production wiring in `clientManager`,
 *  fakes in the unit harness. */
export interface SessionDeps {
  readonly createLanguageClient: (
    id: string,
    name: string,
    serverOptions: ServerOptions,
    clientOptions: LanguageClientOptions
  ) => LanguageClient;
  readonly launchServerProcess: (
    resolution: ProcessServer,
    log: LaunchLog
  ) => LaunchedServer;
  readonly createWasmServer: (module: Uri, logs: NmlLogs) => Promise<WasmServer>;
  /** How long a program gets to answer `initialize` before the editor stops
   *  waiting for it. Injectable so the harness can expire it without sleeping. */
  readonly handshakeBudgetMs: number;
}

export class ServerSession {
  private readonly controller = new AbortController();
  private retired = false;
  private client: LanguageClient | undefined;
  private process: ServerProcess | undefined;
  private stateListener: Disposable | undefined;
  private clientState: State = State.Stopped;
  private connectionLost = false;
  private retirement: Promise<TerminationVerdict> | undefined;

  constructor(
    /** The intent this attempt serves. The manager retires a session whose
     *  generation is no longer the one being asked for. */
    readonly generation: number,
    readonly resolution: ServerResolution,
    private readonly host: SessionHost,
    private readonly deps: SessionDeps
  ) {}

  get label(): string {
    return this.resolution.label;
  }

  get signal(): AbortSignal {
    return this.controller.signal;
  }

  getClient(): LanguageClient | undefined {
    return this.client;
  }

  /** Where this session's server is, in the terms the status bar speaks. */
  lifecycle(): ClientLifecycleState {
    return resolveLifecycleState({
      connectionLost: this.connectionLost,
      hasClient: this.client !== undefined,
      clientState: this.clientState as LspClientStateValue,
    });
  }

  /** Cancel an attempt in flight. Idempotent, and safe at any point: what it
   *  cancels is the WAIT, not the teardown — [`retire`] still runs. */
  abort(): void {
    this.controller.abort();
  }

  /** Nothing this session hands to the language client does anything after
   *  this returns. */
  private live<A extends unknown[]>(fn: (...args: A) => void): (...args: A) => void {
    return (...args: A): void => {
      if (this.retired) return;
      fn(...args);
    };
  }

  /** Run one attempt to its verdict. NEVER THROWS — by construction here,
   *  rather than by every line of [`attempt`] remembering to.
   *
   *  The manager adopts a session (`current`) BEFORE it awaits this, so an
   *  exception escaping reached the reconciler's own `catch`: which says a
   *  sentence and records a status, but retires nothing. The session stayed
   *  `current` for good — the status bar read ITS state instead of the
   *  failure just recorded, and `start()` became a permanent no-op, because
   *  the reconciler sees a live session of the current generation and
   *  returns. Only a restart, which bumps the generation, could get a server
   *  back.
   *
   *  What can throw is the wiring, not the protocol: the language client's
   *  constructor, and on the wasm branch the file-system watcher built for
   *  its `synchronize` options. Both do while the extension host is shutting
   *  down. Answering with the verdict the caller already knows how to report
   *  means the teardown, the message and the status are the ones any other
   *  failed start gets. */
  async start(): Promise<SessionOutcome> {
    try {
      return await this.attempt();
    } catch (err: unknown) {
      return { kind: "failed", detail: String(err) };
    }
  }

  private async attempt(): Promise<SessionOutcome> {
    const resolution = this.resolution;
    this.host.logs.info(`Starting NML LSP (${resolution.label})`);

    const serverOptions: ServerOptions =
      resolution.kind === "wasm"
        ? async (): Promise<WasmServer["transports"]> => {
            const server = await this.deps.createWasmServer(resolution.module, this.host.logs);
            this.process = server.server;
            this.watchExit(server.server);
            return server.transports;
          }
        : async (): Promise<{ process: LaunchedServer["child"]; detached: true }> => {
            const launched = this.deps.launchServerProcess(resolution, (m) =>
              this.host.logs.info(m)
            );
            this.process = launched.server;
            this.watchExit(launched.server);
            // `{ process, detached: true }` — MEASURED (`lib/node/main.js`):
            // the library pipes the process's stderr to the log channel and
            // builds the message streams, and records NO `_serverProcess`, so
            // it never runs its own delayed `pgrep -P` tree walk with
            // `kill -9` behind the extension's back. The lifecycle is wholly
            // ours, which is the only way the verdict can be honest.
            return { process: launched.child, detached: true };
          };

    const clientOptions: LanguageClientOptions = {
      documentSelector: [{ scheme: "file", language: "nml" }],
      outputChannel: this.host.logs.client,
      traceOutputChannel: this.host.logs.trace,
      revealOutputChannelOn: RevealOutputChannelOn.Error,
      progressOnInitialization: true,
      initializationOptions: { explainCommand: "nml.explain" },
      initializationFailedHandler: (error): boolean => {
        // Recorded, never acted on from here: this fires on an ABANDONED
        // client too (measured), and `live` is what stops that from
        // reaching the session that replaced it.
        this.live(() => this.host.logs.error(`initialize failed: ${error}`))();
        return false;
      },
      errorHandler: {
        error: (error) => {
          this.live(() => this.host.logs.error(`LSP connection error: ${error}`))();
          return { action: ErrorAction.Continue };
        },
        closed: () => {
          this.live(() => {
            this.connectionLost = true;
            this.host.connectionLost(this);
          })();
          // `handled: true`: the extension says what happened, with the
          // server's exit code in it. Without this the library ALSO raises
          // its own "Connection to server got closed" toast (measured), so
          // one event became two notifications saying different things.
          return { action: CloseAction.DoNotRestart, handled: true };
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
    this.client = client;
    this.clientState = State.Starting;
    this.stateListener = client.onDidChangeState(
      this.live((event: { newState: State }) => {
        this.clientState = event.newState;
        this.host.stateChanged();
      })
    );
    this.host.stateChanged();

    // `start()` is awaited through a promise that NEVER rejects: the loser of
    // the race below would otherwise become an unhandled rejection in the
    // extension host minutes after the operator moved on, and the failure
    // detail is wanted for the message rather than as a thrown exception.
    let startError: unknown;
    const started = client.start().catch((err: unknown) => {
      startError = err;
    });
    const identity = resolution.kind === "process" ? resolution.identity : undefined;
    // A language client that never answers `initialize` leaves `start()`
    // pending forever — there is no timeout in `vscode-languageclient` — so
    // without a budget a program that accepted the spawn and then sat there
    // leaves the editor in "starting" with no way out. The signal is what
    // stops a `stop`, `restart` or window close from queueing behind it.
    const race = await settledWithin(started, this.deps.handshakeBudgetMs, this.signal);
    if (race === "aborted") return { kind: "aborted" };
    if (race === "expired") {
      // The same silence means two things. Held to a handshake, it is the
      // `indefinite` outcome — the approval is left alone and the ladder
      // falls back. With no handshake to hold it to, it is simply a server
      // that did not start, and saying nothing (which is what routing it
      // through the handshake path did) left the operator with an empty
      // status bar and no sentence about why.
      return identity
        ? { kind: "handshake", identity, outcome: { kind: "indefinite" } }
        : { kind: "unanswered", budgetMs: this.deps.handshakeBudgetMs };
    }

    // `start()` RESOLVING is not evidence that the server started. MEASURED
    // on `vscode-languageclient` 10.1.0: when the connection closes during
    // `initialize`, `handleConnectionClosed` clears `_onStart` FIRST, so
    // `start()`'s trailing `return this._onStart` resolves with `undefined`
    // while the client sits in `StartFailed` — and the real rejection escapes
    // as an unhandled rejection. The state is the fact; the promise is not.
    if (client.state !== State.Running) {
      return {
        kind: "failed",
        detail:
          startError === undefined
            ? `the client reached ${State[client.state]}`
            : String(startError),
      };
    }

    if (identity) {
      const outcome = judgeServerIdentity(client.initializeResult?.serverInfo, identity.expect);
      if (outcome.kind !== "identified") return { kind: "handshake", identity, outcome };
      this.host.logs.info(
        `Handshake verified: ${resolution.label} identifies as ` +
          `${identity.expect}${outcome.version ? ` ${outcome.version}` : ""}.`
      );
    }
    return { kind: "running" };
  }

  /** Say what the server's exit was, once, when it happens — the language
   *  client stopped recording it the moment the extension took the process,
   *  and an exit code is the first thing anyone wants when a server
   *  disappears. NOT gated on `live`: this is a fact about THIS session's own
   *  process, which stays worth logging after the session is retired. */
  private watchExit(server: ServerProcess): void {
    void server.exited.then((info) => {
      this.host.logs.info(`${this.label} ${exitNote(info)}.`);
    });
  }

  /** End this session: it is retired FIRST (so every callback is inert from
   *  here on, including the ones the teardown itself provokes), then the
   *  client is unwound, then the process is ended on the staged ladder.
   *
   *  Idempotent: concurrent callers share one teardown and one verdict. */
  retire(profile: TerminationProfile): Promise<TerminationVerdict> {
    if (!this.retirement) this.retirement = this.tearDown(profile);
    return this.retirement;
  }

  private async tearDown(profile: TerminationProfile): Promise<TerminationVerdict> {
    this.retired = true;
    this.controller.abort();
    this.stateListener?.dispose();
    this.stateListener = undefined;
    const client = this.client;
    this.client = undefined;
    this.clientState = State.Stopped;

    // The LSP `shutdown`/`exit` handshake, and ONLY in the one state the
    // library allows it: `stop()` on a client that is still `starting` throws
    // (`Client is not running and can't be stopped`), and so does `dispose()`.
    // Calling it anyway and swallowing the rejection is how the extension used
    // to tell the operator it had stopped a program it had not.
    if (client && client.state === State.Running) {
      const stopped = client.stop().catch((err) => {
        this.host.logs.warn(`The language server did not shut down cleanly: ${err}`);
      });
      // The outcome is deliberately not read: whether the handshake
      // completed is not evidence about the PROCESS, and the ladder below
      // observes that for itself.
      await settledWithin(stopped, profile.stopMs);
    }

    // The process handle is KEPT: the operator's message names the pid of a
    // program that could not be ended, and that pid is read after this.
    const process = this.process;
    if (!process) return "exited";
    const verdict = await process.terminate(profile);
    if (verdict === "survived") {
      this.host.logs.error(
        `${this.label} could not be ended` +
          (process.pid === undefined ? "." : ` (process ${process.pid}).`)
      );
    }
    return verdict;
  }

  /** The pid of the server this session owned, for the operator's message. */
  get pid(): number | undefined {
    return this.process?.pid;
  }
}
