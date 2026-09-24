import { ExtensionContext, commands, window, workspace } from "vscode";
import { Trace } from "vscode-languageclient/node";
import { LanguageClient, LanguageClientOptions, ServerOptions } from "vscode-languageclient/node";
import { ClientLifecycleState } from "./contracts/lifecycle";
import { NmlLogs } from "./logging";
import { LaunchSandbox, launchSandbox } from "./pathSecurity";
import { launchServerProcess } from "./processLaunch";
import { resolveServer } from "./providerDiscovery";
import { indefiniteMessage, repudiatedMessage, unidentifiedMessage } from "./providerTrust";
import { createWasmServer, privateWorkingDir } from "./serverAcquisition";
import type { ServerResolution } from "./serverResolution";
import {
  DEACTIVATE_TERMINATION,
  INTERACTIVE_TERMINATION,
  TerminationProfile,
  TerminationVerdict,
  terminationNote,
} from "./serverProcess";
import {
  INITIALIZE_BUDGET_MS,
  ServerSession,
  SessionDeps,
  SessionHost,
  SessionOutcome,
} from "./serverSession";

/** The manager's outward seams, constructor-injectable so the unit harness can
 *  drive the lifecycle with a fake client/process (no extension host, no real
 *  `child_process`). Production always uses [`productionDeps`]. */
export interface NmlClientManagerDeps extends SessionDeps {
  readonly resolveServer: typeof resolveServer;
  readonly privateWorkingDir: typeof privateWorkingDir;
}

const SHOW_LOG = "Show Log";

/** The toast for a server that did not start: what failed, the remedy
 *  for THIS kind of server, and where the cause is. The bundled WASM
 *  server needs the WASI host extension; an `nml.server.path` binary
 *  needs to exist and run; a project's provider tool is chosen by
 *  `nml-project.nml`, which no setting overrides; the native fallback of
 *  a build that bundled no server needs installing. One sentence per
 *  kind — "set nml.server.path, or install one" fit at most one of them.
 *
 *  Dispatched on the resolution's KIND and [`ServerOrigin`], never on its
 *  display label: the label is what the status bar prints, and reading a
 *  remedy back out of it made rewording one a silent change of what the
 *  operator is told to do. The switch is exhaustive, so a fourth origin is
 *  a compile error here rather than a fall-through to the last sentence. */
export function startFailureMessage(resolution: ServerResolution): string {
  const lead = `NML: failed to start the NML language server (${resolution.label}).`;
  if (resolution.kind === "wasm") {
    return (
      `${lead} The bundled server runs on the WASI host extension ` +
      "(ms-vscode.wasm-wasi-core) — check that it is installed and enabled; " +
      "the log has the cause. To run a native server instead, set nml.server.path."
    );
  }
  switch (resolution.origin) {
    case "provider":
      return (
        `${lead} The project's tool (${resolution.command} lsp) could not be run; ` +
        "the log has the cause. Fix the tool, or remove the provider declaration " +
        "from nml-project.nml to use the bundled server."
      );
    case "setting":
      return (
        `${lead} Check that ${resolution.command} exists and is executable, or ` +
        "clear nml.server.path to use the bundled server; the log has the cause."
      );
    case "default":
      return (
        `${lead} ${resolution.command} could not be run and this build bundles no server; ` +
        "install one (cargo install --locked --git " +
        "https://github.com/nudge-io/nml nml-lsp) or set nml.server.path to a " +
        "binary outside the workspace. The log has the cause."
      );
  }
}

/** A running server went away without being asked to.
 *
 *  Said by the extension rather than by the language client: the client's own
 *  notice is "Connection to server got closed. Server will not be restarted.",
 *  which names neither the server nor the remedy — and it used to arrive
 *  ALONGSIDE whatever the extension said, so one event produced two
 *  notifications. The close handler now marks itself `handled`, and this is
 *  the one sentence. */
export function connectionLostMessage(label: string): string {
  return (
    `NML: the NML language server (${label}) stopped unexpectedly; NML files ` +
    "are no longer being checked. The log has its exit code. Restart it with " +
    "NML: Restart Language Server."
  );
}

/** The lifecycle itself threw on the way to a running server: the
 *  resolution, the private working directory, or a teardown that had to
 *  finish first. Nothing launched, so no server-kind remedy applies — the
 *  cause is in the log, and the one thing the operator can do is named.
 *
 *  It exists because the alternative was silence: the reconciler's own
 *  `catch` logged the exception, set the status bar to "failed" and said
 *  nothing, so a resolution that threw (a workspace-state write that failed,
 *  a modal raised while the host was closing) left an editor with no server
 *  and no sentence about why. */
export function lifecycleFailureMessage(): string {
  return (
    "NML: the NML language server could not be started; NML files are not " +
    "being checked. The log has the cause. Try again with " +
    "NML: Restart Language Server."
  );
}

/** A server with no handshake to fail did not finish starting. It was ended
 *  (the verdict is appended by the caller), nothing replaced it, and the one
 *  thing the operator can do is named. */
export function unansweredMessage(label: string, budgetMs: number): string {
  return (
    `NML: the NML language server (${label}) did not finish starting within ` +
    `${Math.round(budgetMs / 1000)} seconds and was stopped; NML files are not ` +
    "being checked. The log has what it printed. Restart it with " +
    "NML: Restart Language Server."
  );
}

const productionDeps: NmlClientManagerDeps = {
  resolveServer,
  createWasmServer,
  privateWorkingDir,
  launchServerProcess,
  createLanguageClient: (
    id: string,
    name: string,
    serverOptions: ServerOptions,
    clientOptions: LanguageClientOptions
  ) => new LanguageClient(id, name, serverOptions, clientOptions),
  handshakeBudgetMs: INITIALIZE_BUDGET_MS,
};

/** What the editor currently WANTS, and since when.
 *
 *  Level-triggered, not edge-triggered. Five call sites ask for a restart (the
 *  command, `nml.server.path` changing, a trace-setting change, workspace trust
 *  being granted, approvals being forgotten) and they arrive in bursts — a
 *  settings edit fires several configuration events. Queued as OPERATIONS that
 *  is N restarts, each able to sit behind a 15 s handshake budget; as an
 *  INTENT it is one number, and every attempt whose generation is no longer
 *  the current one is superseded before it can do any work. */
interface Intent {
  readonly kind: "running" | "stopped";
  readonly generation: number;
  /** How hard to try, and how fast — a window that is closing has 5000 ms. */
  readonly profile: TerminationProfile;
}

/** One fallback per intent: a handshake that fails re-resolves ONCE, and the
 *  re-resolution has already been told to skip the launch that failed, so the
 *  ladder cannot cycle. A BOUND, not a latch: the loop below cannot run more
 *  times than this whatever the resolutions do. */
const MAX_ATTEMPTS = 2;

export class NmlClientManager {
  /** The one attempt that is live. Everything else about a server lives on it. */
  private current: ServerSession | undefined;
  private desired: Intent = { kind: "stopped", generation: 0, profile: INTERACTIVE_TERMINATION };
  private generation = 0;
  /** The reconciler, while it is running. */
  private loop: Promise<void> | undefined;
  private sandbox: LaunchSandbox | undefined;
  /** What the status bar reads when there is no session — the outcome of the
   *  last one. */
  private idle: ClientLifecycleState = "absent";
  /** The last server we named to the operator; a failed attempt must not blank
   *  the status bar's label. */
  private lastLabel = "";

  private readonly host: SessionHost;

  constructor(
    private readonly context: ExtensionContext,
    private readonly logs: NmlLogs,
    private readonly onStateChange: () => void,
    private readonly deps: NmlClientManagerDeps = productionDeps
  ) {
    this.host = {
      logs,
      connectionLost: (session) => this.serverDied(session),
      stateChanged: () => this.onStateChange(),
    };
  }

  getClient(): LanguageClient | undefined {
    return this.current?.getClient();
  }

  getServerLabel(): string {
    return this.current?.label ?? this.lastLabel;
  }

  getLifecycleState(): ClientLifecycleState {
    return this.current?.lifecycle() ?? this.idle;
  }

  /** Apply `nml.trace.server` to the running client. NEVER REJECTS.
   *
   *  Both call sites need that, for different reasons. The configuration
   *  listener is fire-and-forget (`void manager.applyTraceSetting()`), where
   *  a rejection becomes an unhandled rejection in the extension host — the
   *  library's `setTrace` writes to the connection, and a connection that
   *  closed between the setting changing and the write rejects. And the
   *  launch must not fail a server that started because its tracing could
   *  not be turned on. One `catch`, in the one place that knows what to say,
   *  rather than a rule every caller has to remember. */
  async applyTraceSetting(): Promise<void> {
    const client = this.getClient();
    if (!client) return;
    const level = workspace.getConfiguration("nml").get<string>("trace.server", "off");
    const trace =
      level === "verbose"
        ? Trace.Verbose
        : level === "messages"
          ? Trace.Messages
          : Trace.Off;
    try {
      await client.setTrace(trace);
    } catch (err: unknown) {
      this.logs.warn(`Could not apply the trace setting: ${err}`);
    }
  }

  // ── intents ────────────────────────────────────────────────────────────

  /** Run a server. Idempotent: asking again while one is wanted does not
   *  restart it. */
  start(): Promise<void> {
    if (this.desired.kind === "running") return this.kick();
    return this.intend("running", INTERACTIVE_TERMINATION);
  }

  /** Run a FRESH server, cancelling whatever attempt is in flight. */
  restart(): Promise<void> {
    return this.intend("running", INTERACTIVE_TERMINATION);
  }

  /** Stop, cancelling whatever attempt is in flight. */
  stop(): Promise<void> {
    return this.intend("stopped", INTERACTIVE_TERMINATION);
  }

  /** Stop, on the budget VS Code gives a closing window. */
  deactivate(): Promise<void> {
    return this.intend("stopped", DEACTIVATE_TERMINATION);
  }

  private intend(kind: Intent["kind"], profile: TerminationProfile): Promise<void> {
    this.generation += 1;
    this.desired = { kind, generation: this.generation, profile };
    // Immediately, not through the queue: the whole point is that a stop
    // asked for during a 15 s handshake wait does not wait for it.
    this.current?.abort();
    return this.kick();
  }

  // ── the reconciler ─────────────────────────────────────────────────────

  /** Drive the world toward [`desired`], and resolve when it is there.
   *
   *  One loop, one step at a time: two sessions never start concurrently, and
   *  one session's teardown always completes before the next one begins —
   *  the single guarantee the old operation queue actually provided. */
  private kick(): Promise<void> {
    if (!this.loop) {
      this.loop = this.reconcile()
        // No intent may reject. `start()` is awaited by `activate()`, where a
        // rejection fails the whole extension, and `deactivate()` is awaited
        // by a host that is 5000 ms from exiting. Whatever went wrong, the
        // editor is told and the lifecycle stays usable.
        .catch((err: unknown) => {
          this.logs.error(`The language-server lifecycle failed: ${err}`);
          this.idle = "failed";
          // Said, not merely logged. Everything that throws in here happens
          // BEFORE a session exists — resolution, consent, the private
          // working directory — so none of the per-server messages apply and
          // none of them would be reached: without this the editor showed
          // "server failed" and explained nothing. A failed STOP is not the
          // operator's problem to act on, so only an intent to run speaks.
          if (this.desired.kind === "running") this.tell(lifecycleFailureMessage());
          this.onStateChange();
        })
        .finally(() => {
          this.loop = undefined;
        });
    }
    return this.loop;
  }

  private async reconcile(): Promise<void> {
    for (;;) {
      const want = this.desired;
      const live = this.current;
      if (live && live.generation !== want.generation) {
        await this.retire(live, want.profile);
        continue;
      }
      if (want.kind === "stopped") {
        if (live) {
          await this.retire(live, want.profile);
          continue;
        }
        if (this.desired !== want) continue;
        return;
      }
      if (live) {
        if (this.desired !== want) continue;
        return;
      }
      await this.launch(want);
      if (this.desired !== want) continue;
      return;
    }
  }

  private async retire(
    session: ServerSession,
    profile: TerminationProfile
  ): Promise<TerminationVerdict> {
    if (this.current === session) this.current = undefined;
    const verdict = await session.retire(profile);
    this.idle = "absent";
    this.onStateChange();
    return verdict;
  }

  /** The resolution ladder for ONE intent: at most [`MAX_ATTEMPTS`] launches,
   *  each its own session. A handshake failure falls back to the next
   *  resolution; a program that could not be run, or a neutral server that
   *  never answered, does not (the operator is told what to fix, and a
   *  fallback would hide it). */
  private async launch(want: Intent): Promise<void> {
    for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt += 1) {
      if (this.desired !== want) return;
      const resolution = await this.deps.resolveServer(
        this.context,
        this.logs,
        await this.launchSandbox()
      );
      if (this.desired !== want) return;

      const session = new ServerSession(want.generation, resolution, this.host, this.deps);
      this.current = session;
      this.lastLabel = resolution.label;
      const outcome = await session.start();
      if (outcome.kind === "running") {
        // `nml.trace.server` is read here rather than by the session: it is a
        // setting, and the session does not know about settings. A server
        // started with tracing already on must trace from its first message,
        // so this cannot wait for the next configuration change.
        await this.applyTraceSetting();
        return;
      }
      if (outcome.kind === "aborted") {
        // Superseded. The session stays `current` so the reconciler retires
        // it on the profile of the intent that superseded it — a window that
        // is closing has 5000 ms, and the intent it replaced does not get to
        // spend them.
        return;
      }

      const verdict = await this.retire(session, want.profile);
      if (outcome.kind === "failed") {
        this.reportStartFailure(resolution, outcome.detail);
        this.idle = "failed";
        this.onStateChange();
        return;
      }
      if (outcome.kind === "unanswered") {
        this.logs.error(
          `${resolution.label} did not answer initialize within ${outcome.budgetMs} ms. Stopped it.`
        );
        const note = terminationNote(verdict, session.pid, process.platform);
        const message = unansweredMessage(resolution.label, outcome.budgetMs);
        this.tell(note ? `${message} ${note}` : message);
        this.idle = "failed";
        this.onStateChange();
        return;
      }
      await this.handshakeFailed(session, resolution, outcome.identity, outcome.outcome, verdict);
    }
    // Every attempt in the ladder failed its handshake. The operator has been
    // told about each one; what is left is the STATE, and `retire` left it
    // "absent" — which reads as "not running", i.e. as if nothing had been
    // tried. The editor tried twice and has no server: that is `failed`, and
    // it is the status that carries the remedy (show the log, restart).
    this.idle = "failed";
    this.onStateChange();
  }

  /** A server we were running went away by itself. Today's contract is
   *  DoNotRestart: the editor says so and stops, rather than looping a
   *  crashing program. Expressed as an INTENT so a restart the operator asks
   *  for a moment later is ordered against it. */
  private serverDied(session: ServerSession): void {
    // No `if (this.current === session)` here, deliberately. A session that is
    // no longer current has been RETIRED, and a retired session's callbacks do
    // not run — that is the one mechanism, and a second guard beside it means
    // the first is never exercised and the next handler someone adds is
    // unguarded again. (Proven: with the identity check here, breaking
    // `ServerSession.live` left every test green.)
    this.logs.warn(`Language server connection closed (${session.label}).`);
    const label = session.label;
    const settling = this.intend("stopped", INTERACTIVE_TERMINATION);
    // The generation this close OWNS, read after `intend` has bumped it.
    // The teardown can take the whole interactive ladder, the toast above
    // tells the operator to restart, and a restart asked for meanwhile is
    // served by the SAME loop — so this callback runs after it. Without the
    // guard it overwrote that attempt's outcome with this one's: a
    // replacement that failed to start read as "the connection closed
    // unexpectedly", which is the event before last.
    const mine = this.generation;
    void settling.then(() => {
      if (this.generation !== mine) return;
      this.idle = "disconnected";
      this.onStateChange();
    });
    this.tell(connectionLostMessage(label));
  }

  /** The spawn sandbox, minted once per manager: an empty private working
   *  directory plus the scrubbed-environment overlay. */
  private async launchSandbox(): Promise<LaunchSandbox> {
    if (!this.sandbox) {
      this.sandbox = launchSandbox(await this.deps.privateWorkingDir(this.context, this.logs));
    }
    return this.sandbox;
  }

  // ── what the operator is told ──────────────────────────────────────────

  private reportStartFailure(resolution: ServerResolution, detail: string): void {
    this.logs.error(`Failed to start language server: ${detail}`);
    this.tell(startFailureMessage(resolution));
  }

  /** A program the operator approved did not turn out to be an NML language
   *  server, or never answered. Its session is already retired and its process
   *  already ended — this says what happened and records the consequence; the
   *  caller then falls back to the next resolution. */
  private async handshakeFailed(
    session: ServerSession,
    resolution: ServerResolution,
    identity: Extract<SessionOutcome, { kind: "handshake" }>["identity"],
    outcome: Extract<SessionOutcome, { kind: "handshake" }>["outcome"],
    verdict: TerminationVerdict
  ): Promise<void> {
    // One row per outcome: what the log says, what happens to the approval,
    // and what the operator reads. Only a definite negative touches the
    // approval.
    const failure = ((): {
      detail: string;
      consequence: () => Promise<void>;
      message: string;
    } => {
      switch (outcome.kind) {
        case "repudiated":
          return {
            detail: `serverInfo.name was "${outcome.saw}"`,
            consequence: identity.repudiate,
            message: repudiatedMessage(identity.tool, outcome.saw),
          };
        case "unidentified":
          return {
            detail: "serverInfo.name was absent",
            consequence: identity.standDown,
            message: unidentifiedMessage(identity.tool),
          };
        case "indefinite":
          return {
            detail: `no initialize answer within ${this.deps.handshakeBudgetMs} ms`,
            consequence: identity.standDown,
            message: indefiniteMessage(identity.tool, this.deps.handshakeBudgetMs),
          };
      }
    })();
    this.logs.error(
      `Provider handshake failed for ${resolution.label}: ${failure.detail}. Stopping it.`
    );
    await failure.consequence();
    // The sentence about the program's FATE is written from what was OBSERVED
    // when it was ended, never from a promise resolving.
    const note = terminationNote(verdict, session.pid, process.platform);
    this.tell(note ? `${failure.message} ${note}` : failure.message);
  }

  private tell(message: string): void {
    void window.showErrorMessage(message, SHOW_LOG).then((choice) => {
      if (choice === SHOW_LOG) void commands.executeCommand("nml.showServerLog");
    });
  }
}
