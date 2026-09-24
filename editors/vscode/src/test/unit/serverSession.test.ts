// MUST come first: routes `require("vscode")` to the stub before any module
// that (transitively) imports the real extension-host API is loaded.
import "../support/installVscodeStub";

import * as assert from "node:assert";
import type { ChildProcess } from "node:child_process";
import type { LogOutputChannel } from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  State,
} from "vscode-languageclient/node";
import type { NmlLogs } from "../../logging";
import type { LaunchedServer } from "../../processLaunch";
import { launchSandbox } from "../../pathSecurity";
import type { ServerResolution } from "../../serverResolution";
import { processServer } from "../../serverResolution";
import {
  ExitInfo,
  ServerProcess,
  TerminationProfile,
  TerminationVerdict,
  DEACTIVATE_TERMINATION,
  INTERACTIVE_TERMINATION,
} from "../../serverProcess";
import { ServerSession, SessionDeps, SessionHost } from "../../serverSession";

// ─────────────────────────────────────────────────────────────────────────
// THE SESSION, DRIVEN DIRECTLY.
//
// Everywhere else a session is reached through the manager, which is the
// right level for the lifecycle but hides the session's OWN contract: it
// promises to be retirable more than once, abortable before it starts, and
// to keep logging its process's exit after it has been retired. The manager
// happens never to exercise any of those — it retires each session exactly
// once, and clears `current` before it awaits — so a guarantee the class
// states in its doc comment had nothing testing it. (Measured: deleting the
// teardown memo left every other suite green.)
// ─────────────────────────────────────────────────────────────────────────

const logged: string[] = [];

const logs: NmlLogs = {
  client: undefined as unknown as LogOutputChannel,
  trace: undefined as unknown as LogOutputChannel,
  info: (m) => logged.push(`info: ${m}`),
  warn: (m) => logged.push(`warn: ${m}`),
  error: (m) => logged.push(`error: ${m}`),
  showClient: () => undefined,
  showTrace: () => undefined,
};

/** A server process whose teardown the test can count and hold open. */
class FakeProcess implements ServerProcess {
  readonly profiles: TerminationProfile[] = [];
  verdict: TerminationVerdict = "exited";
  readonly pid = 4242;
  readonly exited: Promise<ExitInfo>;
  private settle!: (info: ExitInfo) => void;
  /** Opened by the test to let a teardown finish. */
  release: (() => void) | undefined;

  constructor(readonly label: string) {
    this.exited = new Promise((resolve) => {
      this.settle = resolve;
    });
  }

  /** The process ends by itself, at a moment the test chooses. */
  endByItself(info: ExitInfo): void {
    this.settle(info);
  }

  async terminate(profile: TerminationProfile): Promise<TerminationVerdict> {
    this.profiles.push(profile);
    if (this.release) {
      await new Promise<void>((resolve) => {
        this.release = resolve;
      });
    }
    this.settle({ code: 0, signal: null });
    return this.verdict;
  }
}

class FakeClient {
  state: State = State.Stopped;
  stopCalls = 0;
  disposals = 0;
  initializeResult: unknown;
  constructor(
    readonly serverOptions: ServerOptions,
    readonly clientOptions: LanguageClientOptions
  ) {}
  onDidChangeState(): { dispose(): void } {
    return {
      dispose: (): void => {
        this.disposals += 1;
      },
    };
  }
  async start(): Promise<void> {
    const so = this.serverOptions;
    if (typeof so === "function") await so();
    this.state = State.Running;
  }
  stop(): Promise<void> {
    this.stopCalls += 1;
    if (this.state !== State.Running) return Promise.reject(new Error("not running"));
    this.state = State.Stopped;
    return Promise.resolve();
  }
  setTrace(): Promise<void> {
    return Promise.resolve();
  }
}

interface Rig {
  session: ServerSession;
  processes: FakeProcess[];
  clients: FakeClient[];
  connectionLosses: number;
}

/** The one constructor production uses, so the fake carries a branded
 *  sandbox directory like every real resolution — a plain string cannot be
 *  written here any more, which is the point of the brand. */
const RESOLUTION: ServerResolution = processServer(
  "/fake/nml-lsp",
  [],
  "fake process",
  "default",
  launchSandbox("/fake/private")
);

function rig(options: { budgetMs?: number } = {}): Rig {
  const processes: FakeProcess[] = [];
  const clients: FakeClient[] = [];
  const counters = { connectionLosses: 0 };
  const host: SessionHost = {
    logs,
    connectionLost: () => {
      counters.connectionLosses += 1;
    },
    stateChanged: () => undefined,
  };
  const deps: SessionDeps = {
    handshakeBudgetMs: options.budgetMs ?? 60_000,
    createWasmServer: () => {
      throw new Error("not used");
    },
    launchServerProcess: (res): LaunchedServer => {
      const proc = new FakeProcess(res.label);
      processes.push(proc);
      return { child: {} as ChildProcess, server: proc };
    },
    createLanguageClient: (_id, _name, serverOptions, clientOptions) => {
      const client = new FakeClient(serverOptions, clientOptions);
      clients.push(client);
      return client as unknown as LanguageClient;
    },
  };
  const session = new ServerSession(7, RESOLUTION, host, deps);
  return {
    session,
    processes,
    clients,
    get connectionLosses(): number {
      return counters.connectionLosses;
    },
  } as Rig;
}

suite("serverSession/retiring is idempotent", () => {
  test("concurrent callers share ONE teardown and ONE verdict", async () => {
    // A pid the kernel has recycled is killed by a second ladder as surely
    // as by a first one. The manager happens to retire each session once, so
    // nothing else in the suite can tell a memo from no memo — this is the
    // test that can.
    const r = rig();
    assert.strictEqual((await r.session.start()).kind, "running");
    const proc = r.processes[0];
    proc.release = (): void => undefined; // hold the teardown open

    const first = r.session.retire(INTERACTIVE_TERMINATION);
    const second = r.session.retire(DEACTIVATE_TERMINATION);
    assert.strictEqual(first, second, "two callers, one teardown promise");
    await new Promise((resolve) => setImmediate(resolve));
    assert.strictEqual(proc.profiles.length, 1, "the ladder was climbed twice");

    r.processes[0].release?.();
    assert.strictEqual(await first, "exited");
    assert.strictEqual(await second, "exited");
    assert.deepStrictEqual(
      proc.profiles,
      [INTERACTIVE_TERMINATION],
      "the FIRST profile owns the teardown; a later, shorter one does not restart it"
    );
    assert.strictEqual(r.clients[0].stopCalls, 1, "one LSP shutdown, not two");
    assert.strictEqual(r.clients[0].disposals, 1, "one listener, disposed once");
  });

  test("a sequential second retire returns the verdict, it does not run again", async () => {
    const r = rig();
    await r.session.start();
    assert.strictEqual(await r.session.retire(INTERACTIVE_TERMINATION), "exited");
    assert.strictEqual(await r.session.retire(DEACTIVATE_TERMINATION), "exited");
    assert.strictEqual(r.processes[0].profiles.length, 1);
    assert.strictEqual(r.clients[0].stopCalls, 1);
  });

  test("a session with no process at all retires cleanly", async () => {
    // `start()` was never called: there is no client, no listener and no
    // process. Retiring must still answer, because the reconciler retires
    // whatever is current without asking how far it got.
    const r = rig();
    assert.strictEqual(await r.session.retire(INTERACTIVE_TERMINATION), "exited");
    assert.strictEqual(r.processes.length, 0);
    assert.strictEqual(r.session.pid, undefined);
  });
});

suite("serverSession/an attempt that was cancelled before it began", () => {
  test("abort() before start() is `aborted`, and the process it started is still ownable", async () => {
    // `settledWithin` checks an ALREADY-aborted signal before it subscribes,
    // because `addEventListener("abort", …)` on a signal that has already
    // fired never runs — so without that check a cancelled attempt reports
    // itself RUNNING and the manager keeps a server nobody asked for. The
    // manager cannot reach this today (it sets `current` and awaits
    // `start()` with no suspension in between), so the branch is pinned here
    // or nowhere.
    //
    // What is NOT claimed: that nothing was launched. The language client
    // starts the process, and by the time the outcome is read it exists —
    // which is exactly why an aborted attempt keeps its session: the
    // reconciler retires it, and the process goes with it. An outcome that
    // said "aborted" while the session dropped the handle would leak a
    // server on every superseded intent.
    const r = rig();
    r.session.abort();
    const outcome = await r.session.start();
    assert.strictEqual(outcome.kind, "aborted");
    assert.strictEqual(r.processes.length, 1, "the client started the server before we got here");
    assert.strictEqual(r.session.pid, 4242, "…and the session owns it");

    assert.strictEqual(await r.session.retire(INTERACTIVE_TERMINATION), "exited");
    assert.deepStrictEqual(r.processes[0].profiles, [INTERACTIVE_TERMINATION]);
  });
});

suite("serverSession/the exit of this session's own process", () => {
  test("is logged after the session is retired, because it is still a fact about it", async () => {
    // Deliberately NOT gated on `live`: every callback the session hands to
    // the language client is inert once retired, but the process's exit code
    // is the first thing anyone wants when a server disappears, and it
    // usually ARRIVES after the teardown that caused it.
    const r = rig();
    await r.session.start();
    const proc = r.processes[0];
    logged.length = 0;
    await r.session.retire(INTERACTIVE_TERMINATION);
    proc.endByItself({ code: null, signal: "SIGKILL" });
    await proc.exited;
    await new Promise((resolve) => setImmediate(resolve));
    assert.ok(
      logged.some((line) => line.includes("fake process") && line.includes("exited with code 0")),
      `the exit was not logged: ${logged.join(" | ")}`
    );
  });
});
