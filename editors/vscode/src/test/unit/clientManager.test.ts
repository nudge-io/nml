import { launchSandbox, providerWorkingDir } from "../../pathSecurity";
// MUST come first: routes `require("vscode")` to the stub before any module
// that (transitively) imports the real extension-host API is loaded.
import "../support/installVscodeStub";

import * as assert from "node:assert";
import type { ChildProcess } from "node:child_process";
import type { ExtensionContext, LogOutputChannel, Uri } from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  State,
  Trace,
} from "vscode-languageclient/node";
import {
  connectionLostMessage,
  lifecycleFailureMessage,
  startFailureMessage,
  NmlClientManager,
  NmlClientManagerDeps,
} from "../../clientManager";
import type { ServerResolution } from "../../serverResolution";
import type { LaunchedServer } from "../../processLaunch";
import type { WasmServer } from "../../serverAcquisition";
import type { NmlLogs } from "../../logging";
import {
  ExitInfo,
  ServerProcess,
  TerminationProfile,
  TerminationVerdict,
  DEACTIVATE_TERMINATION,
  INTERACTIVE_TERMINATION,
  terminationNote,
} from "../../serverProcess";
import { resetStubRecords, shownErrorMessages } from "../support/vscodeStub";
import { NML_SERVER_NAME } from "../../providerTrust";

/** The private working directory the manager asks `privateWorkingDir` for. */
const PRIVATE_CWD = "/fake/global-storage/server-cwd";
/** `ProcessServer.cwd` is branded, so even a fake resolution can only carry
 *  a directory `launchSandbox` minted — the same rule production obeys. */
const PRIVATE_SANDBOX_CWD = launchSandbox(PRIVATE_CWD).cwd;

/** A latch the test opens to release a blocked fake start(). */
class Gate {
  readonly promise: Promise<void>;
  private release!: () => void;
  constructor() {
    this.promise = new Promise((resolve) => {
      this.release = resolve;
    });
  }
  open(): void {
    this.release();
  }
}

/** A server process under the test's control, on the SAME interface the
 *  native launch and the WASM backend both present. */
class FakeServerProcess implements ServerProcess {
  readonly profiles: TerminationProfile[] = [];
  verdict: TerminationVerdict = "exited";
  pid: number | undefined = 4242;
  /** When set, `terminate()` does not resolve until the gate opens — a
   *  teardown the test can hold IN FLIGHT while it asks for something else. */
  gate: Gate | undefined;
  readonly exited: Promise<ExitInfo>;
  private settle!: (info: ExitInfo) => void;
  constructor(readonly label: string) {
    this.exited = new Promise((resolve) => {
      this.settle = resolve;
    });
  }
  /** When true, `terminate()` rejects. */
  throws = false;

  async terminate(profile: TerminationProfile): Promise<TerminationVerdict> {
    this.profiles.push(profile);
    // The real ladder is idempotent and the FIRST profile wins; the fake
    // records every caller's profile so a test can see which one was used.
    if (this.gate) await this.gate.promise;
    if (this.throws) throw new Error("fake teardown failure");
    this.settle({ code: 0, signal: null });
    return this.verdict;
  }
  get terminateCalls(): number {
    return this.profiles.length;
  }
}

class FakeLanguageClient {
  stopCalls = 0;
  listenerDisposals = 0;
  trace: Trace | undefined;
  state: State = State.Stopped;
  /** What the real client exposes after a successful `initialize`. */
  initializeResult: { serverInfo?: { name?: string; version?: string } } | undefined;

  constructor(
    private readonly id: number,
    private readonly events: string[],
    readonly serverOptions: ServerOptions,
    readonly clientOptions: LanguageClientOptions,
    private readonly behavior: (self: FakeLanguageClient) => Promise<void>
  ) {}

  private stateListener: ((e: { oldState: State; newState: State }) => void) | undefined;

  onDidChangeState(listener: (e: { oldState: State; newState: State }) => void): {
    dispose(): void;
  } {
    this.stateListener = listener;
    return {
      dispose: (): void => {
        this.listenerDisposals += 1;
        this.stateListener = undefined;
      },
    };
  }

  /** Mirrors the real client: a function `ServerOptions` is invoked on start. */
  async startServerIfFunction(): Promise<void> {
    const so = this.serverOptions;
    if (typeof so === "function") await so();
  }

  async start(): Promise<void> {
    this.events.push(`start:${this.id}`);
    this.state = State.Starting;
    await this.behavior(this);
    // MEASURED on `vscode-languageclient` 10.1.0: `start()` RESOLVING does
    // not mean the server started — when the connection closes during
    // `initialize` it resolves with `undefined` while the client sits in
    // `StartFailed`. The fake therefore carries a state, and the manager is
    // required to read it.
    if (this.state === State.Starting) this.state = State.Running;
    this.stateListener?.({ oldState: State.Starting, newState: this.state });
  }

  /** MEASURED against `vscode-languageclient` 10.1.0
   *  (`BaseLanguageClient.shutdown`): stopping needs an ACTIVE CONNECTION,
   *  which exists only once `initialize` has been answered — a client that
   *  is still `starting` throws `Client is not running and can't be
   *  stopped`, and so does `dispose()`. A fake that always resolved made the
   *  manager's "stop it" path untestable in the one case it exists for: a
   *  provider that never answers. */
  stop(): Promise<void> {
    this.stopCalls += 1;
    this.events.push(`stop:${this.id}`);
    if (this.state !== State.Running) {
      return Promise.reject(
        new Error(
          `Client is not running and can't be stopped. It's current state is: ${State[this.state]}`
        )
      );
    }
    this.state = State.Stopped;
    return Promise.resolve();
  }

  /** When true, `setTrace` rejects — what the real client does when the
   *  connection went away between the setting changing and the write. */
  traceRejects = false;

  setTrace(value: Trace): Promise<void> {
    if (this.traceRejects) return Promise.reject(new Error("connection is closed"));
    this.trace = value;
    return Promise.resolve();
  }

  /** Drive the library's stale callbacks by hand, exactly as the real library
   *  does when an ABANDONED client's process finally dies. */
  fireClosed(): unknown {
    return this.clientOptions.errorHandler?.closed();
  }
  fireInitializationFailed(error: unknown): unknown {
    return this.clientOptions.initializationFailedHandler?.(error as never);
  }
}

interface HarnessOptions {
  kind: "wasm" | "process";
  /** When present, the FIRST client's start() blocks until the gate opens. */
  firstStartGate?: Gate;
  /** When true, the FIRST client's start() rejects (after wiring the server). */
  failFirstStart?: boolean;
  /** When set, the Nth client's start() rejects (1-based) — a SECOND
   *  attempt failing, which `failFirstStart` cannot express. */
  failStartOnCall?: number;
  /** When true, the FIRST client's start() RESOLVES but the client is left in
   *  StartFailed — the library's measured behaviour on a connection that
   *  closes during `initialize`. */
  firstStartResolvesFailed?: boolean;
  /** When true, the process resolution carries an identity contract — i.e.
   *  it is a program a REPOSITORY asked for, so the handshake applies. */
  identity?: boolean;
  /** `serverInfo.name` the first client answers `initialize` with. `null`
   *  means the answer carried no `serverInfo` at all. */
  serverInfoName?: string | null;
  /** When true, the FIRST client's start() never settles. */
  hangFirstStart?: boolean;
  /** When true, the FIRST client fires the library's `closed()` callback
   *  from INSIDE start() and then leaves itself in StartFailed — the server
   *  dying during its own handshake, which is the order the abandoned-client
   *  probes measured. */
  closeDuringFirstStart?: boolean;
  /** When set, `resolveServer` throws on the Nth call (1-based): the
   *  lifecycle failing BEFORE any session exists. */
  resolveThrowsOnCall?: number;
  /** When set, `createLanguageClient` throws on the Nth call (1-based) —
   *  the session's own wiring failing AFTER the manager has adopted it.
   *  The real seam is `new LanguageClient(...)` and, on the wasm branch,
   *  `workspace.createFileSystemWatcher`, both of which throw while the
   *  extension host is shutting down. */
  createClientThrowsOnCall?: number;
  /** When present, every identity consequence (`repudiate`/`standDown`)
   *  waits on it — the ladder held BETWEEN its rungs, which is where an
   *  intent can arrive and the second rung must not be spent. */
  identityGate?: Gate;
  /** When true, every client's `setTrace` rejects. */
  traceRejects?: boolean;
  /** Every resolution carries an identity contract AND every server answers
   *  the handshake as something else: the whole ladder fails its handshake. */
  impostorEverywhere?: boolean;
  /** Handed to every fake process, so a teardown can be held in flight. */
  terminateGate?: Gate;
  /** When true, every fake process's `terminate()` throws — the teardown
   *  itself failing, which the reconciler can only report. */
  terminateThrows?: boolean;
  handshakeBudgetMs?: number;
  /** The verdict every fake process reports from `terminate()`. */
  verdict?: TerminationVerdict;
}

interface IdentityCalls {
  repudiated: number;
  stoodDown: number;
}

interface Harness {
  manager: NmlClientManager;
  events: string[];
  clients: FakeLanguageClient[];
  processes: FakeServerProcess[];
  launches: ServerResolution[];
  stateChanges: { count: number };
  identityCalls: IdentityCalls;
}

function makeHarness(options: HarnessOptions): Harness {
  const events: string[] = [];
  const clients: FakeLanguageClient[] = [];
  const processes: FakeServerProcess[] = [];
  const launches: ServerResolution[] = [];
  const stateChanges = { count: 0 };

  const identityCalls: IdentityCalls = { repudiated: 0, stoodDown: 0 };

  const neutral: ServerResolution = {
    kind: "process",
    command: "/fake/neutral-nml-lsp",
    args: [],
    cwd: PRIVATE_SANDBOX_CWD,
    env: { LD_PRELOAD: undefined },
    label: "fake neutral",
    origin: "default",
  };

  const resolution: ServerResolution =
    options.kind === "wasm"
      ? {
          kind: "wasm",
          module: undefined as unknown as Uri,
          label: "fake wasm",
        }
      : {
          kind: "process",
          command: "/fake/nml-lsp",
          args: [],
          cwd: PRIVATE_SANDBOX_CWD,
          env: { LD_PRELOAD: undefined, DYLD_INSERT_LIBRARIES: undefined },
          label: "fake process",
          origin: options.identity || options.impostorEverywhere ? "provider" : "default",
          ...(options.identity || options.impostorEverywhere
            ? {
                identity: {
                  expect: NML_SERVER_NAME,
                  tool: "nudge",
                  repudiate: async (): Promise<void> => {
                    identityCalls.repudiated += 1;
                    if (options.identityGate) await options.identityGate.promise;
                  },
                  standDown: async (): Promise<void> => {
                    identityCalls.stoodDown += 1;
                    if (options.identityGate) await options.identityGate.promise;
                  },
                },
              }
            : {}),
        };
  let resolveCalls = 0;
  let createClientCalls = 0;

  const logs: NmlLogs = {
    client: undefined as unknown as LogOutputChannel,
    trace: undefined as unknown as LogOutputChannel,
    info: () => undefined,
    warn: () => undefined,
    error: () => undefined,
    showClient: () => undefined,
    showTrace: () => undefined,
  };

  function newProcess(label: string): FakeServerProcess {
    const proc = new FakeServerProcess(label);
    if (options.verdict) proc.verdict = options.verdict;
    if (options.terminateGate) proc.gate = options.terminateGate;
    if (options.terminateThrows) proc.throws = true;
    processes.push(proc);
    return proc;
  }

  const deps: NmlClientManagerDeps = {
    privateWorkingDir: async () => {
      events.push("private-cwd");
      return PRIVATE_CWD;
    },
    handshakeBudgetMs: options.handshakeBudgetMs ?? 60_000,
    resolveServer: async () => {
      events.push("resolve");
      resolveCalls += 1;
      if (options.resolveThrowsOnCall === resolveCalls) {
        throw new Error("fake resolution failure");
      }
      // Every rung of the ladder is the same declared provider: the
      // fallback has nowhere better to go.
      if (options.impostorEverywhere) return resolution;
      // A stood-down provider is skipped on re-resolution — the ladder's
      // own behaviour, so the fallback lands on a working server rather
      // than back on the launch that just failed.
      return resolveCalls === 1 || !options.identity ? resolution : neutral;
    },
    launchServerProcess: (res): LaunchedServer => {
      launches.push(res);
      events.push("launch");
      return {
        child: {} as ChildProcess,
        server: newProcess(res.label),
      };
    },
    createWasmServer: async () => {
      events.push("wasm-create");
      return {
        transports: undefined,
        server: newProcess("fake wasm"),
      } as unknown as WasmServer;
    },
    createLanguageClient: (_id, _name, serverOptions, clientOptions) => {
      // Counted separately from `clients`: a construction that THREW pushed
      // nothing, so `clients.length` would name the same call for ever.
      createClientCalls += 1;
      if (options.createClientThrowsOnCall === createClientCalls) {
        throw new Error("fake language-client construction failure");
      }
      const n = clients.length + 1;
      const behavior = async (self: FakeLanguageClient): Promise<void> => {
        await self.startServerIfFunction();
        if (n === 1 && options.firstStartGate) await options.firstStartGate.promise;
        if (n === 1 && options.hangFirstStart) await new Promise<void>(() => undefined);
        if (n === 1 && options.closeDuringFirstStart) {
          // The connection dies while `initialize` is still outstanding:
          // the library calls `closed()` and then resolves `start()` with
          // the client in StartFailed (measured on 10.1.0).
          self.fireClosed();
          self.state = State.StartFailed;
          return;
        }
        if ((n === 1 && options.failFirstStart) || options.failStartOnCall === n) {
          throw new Error("fake start failure");
        }
        if (n === 1 && options.firstStartResolvesFailed) {
          self.state = State.StartFailed;
          return;
        }
        const name = options.impostorEverywhere
          ? "sh"
          : n === 1 && options.serverInfoName !== undefined
            ? options.serverInfoName
            : NML_SERVER_NAME;
        self.initializeResult =
          name === null ? {} : { serverInfo: { name, version: "0.1.0" } };
      };
      const client = new FakeLanguageClient(n, events, serverOptions, clientOptions, behavior);
      if (options.traceRejects) client.traceRejects = true;
      clients.push(client);
      events.push(`create:${n}`);
      return client as unknown as LanguageClient;
    },
  };

  const manager = new NmlClientManager(
    undefined as unknown as ExtensionContext,
    logs,
    () => {
      stateChanges.count += 1;
    },
    deps
  );
  return { manager, events, clients, processes, launches, stateChanges, identityCalls };
}

/** Drain the microtask chains behind fire-and-forget lifecycle handlers. */
function settled(): Promise<void> {
  return new Promise((resolve) => setImmediate(resolve));
}

suite("clientManager/NmlClientManager lifecycle", () => {
  setup(() => {
    resetStubRecords();
  });

  test("start() wires the wasm server and stop() ends its process", async () => {
    const h = makeHarness({ kind: "wasm" });
    await h.manager.start();

    assert.strictEqual(h.processes.length, 1);
    assert.strictEqual(h.manager.getServerLabel(), "fake wasm");
    assert.strictEqual(h.manager.getLifecycleState(), "running");
    // A server started while `nml.trace.server` is on must trace from its
    // first message, so the setting is applied at start, not only when the
    // configuration next changes.
    assert.strictEqual(h.clients[0].trace, Trace.Off);

    await h.manager.stop();
    assert.strictEqual(h.processes[0].terminateCalls, 1);
    assert.strictEqual(h.clients[0].stopCalls, 1);
    // The per-start state subscription must die with its session.
    assert.strictEqual(h.clients[0].listenerDisposals, 1);
    assert.strictEqual(h.manager.getClient(), undefined);
    assert.strictEqual(h.manager.getLifecycleState(), "absent");
  });

  test("a process server is launched with the resolution's sandbox, and the client is given the process", async () => {
    const h = makeHarness({ kind: "process" });
    await h.manager.start();
    assert.strictEqual(h.launches.length, 1);
    const launched = h.launches[0];
    assert.ok(launched.kind === "process");
    // `vscode-languageclient` spawns in the first workspace folder when
    // `options.cwd` is absent — the hole a repository's `lsp` file rode.
    assert.strictEqual(launched.cwd, PRIVATE_CWD);
    // …and the scrub overlay must reach the spawn, or the loader-injection
    // variables are simply inherited.
    assert.deepStrictEqual(launched.env, {
      LD_PRELOAD: undefined,
      DYLD_INSERT_LIBRARIES: undefined,
    });
    // The client is handed `{ process, detached: true }` — the shape for
    // which the library records NO `_serverProcess` and therefore never runs
    // its own delayed tree-walk kill behind the extension's back.
    const so = h.clients[0].serverOptions;
    assert.ok(typeof so === "function", "a function ServerOptions, not an Executable");
    const wired = (await so()) as { process: unknown; detached: boolean };
    assert.strictEqual(wired.detached, true);
    assert.ok("process" in wired);
  });

  test("a verified provider handshake keeps the client and says nothing to the operator", async () => {
    const h = makeHarness({ kind: "process", identity: true });
    await h.manager.start();
    assert.strictEqual(h.clients.length, 1, "no fallback was needed");
    assert.strictEqual(h.identityCalls.repudiated, 0);
    assert.strictEqual(h.identityCalls.stoodDown, 0);
    assert.deepStrictEqual(shownErrorMessages, []);
    assert.ok(h.manager.getClient(), "the verified client is still running");
  });

  test("a provider that is not an NML server is ended, its approval withdrawn, and the operator told", async () => {
    const h = makeHarness({ kind: "process", identity: true, serverInfoName: "sh" });
    await h.manager.start();

    assert.strictEqual(h.processes[0].terminateCalls, 1, "the impostor's process is ended");
    assert.strictEqual(h.identityCalls.repudiated, 1, "the approval is withdrawn");
    assert.strictEqual(h.identityCalls.stoodDown, 0);
    assert.strictEqual(shownErrorMessages.length, 1);
    assert.match(shownErrorMessages[0], /answered the editor's handshake as "sh"/);
    assert.match(shownErrorMessages[0], /approval has been removed/);
    // The operator is left with a working editor, not a dead one.
    assert.strictEqual(h.clients.length, 2, "one fallback start");
    assert.strictEqual(h.manager.getServerLabel(), "fake neutral");
  });

  test("a provider answering with NO serverInfo is stopped for the session and KEEPS its approval", async () => {
    const h = makeHarness({ kind: "process", identity: true, serverInfoName: null });
    await h.manager.start();
    assert.strictEqual(h.processes[0].terminateCalls, 1, "it does not keep running unidentified");
    assert.strictEqual(h.identityCalls.repudiated, 0, "the approval is untouched");
    assert.strictEqual(h.identityCalls.stoodDown, 1, "and it is not retried this session");
    assert.strictEqual(shownErrorMessages.length, 1);
    assert.match(shownErrorMessages[0], /without naming itself/);
    assert.match(shownErrorMessages[0], /approval is unchanged/);
    assert.strictEqual(h.manager.getServerLabel(), "fake neutral", "one fallback start");
    // The operator is told the likely cause (a provider built before the
    // handshake existed) and the action, not only the accusation.
    assert.match(shownErrorMessages[0], /rebuild it against a current nml-lsp/);
  });

  test("a provider that never answers stands down WITHOUT withdrawing the approval", async () => {
    const h = makeHarness({
      kind: "process",
      identity: true,
      hangFirstStart: true,
      handshakeBudgetMs: 10,
    });
    await h.manager.start();

    // An unanswered handshake is not evidence: a loaded machine looks
    // exactly like this, and a false accusation trains the operator to
    // click through the real one.
    assert.strictEqual(h.identityCalls.repudiated, 0, "the approval is untouched");
    assert.strictEqual(h.identityCalls.stoodDown, 1);
    assert.strictEqual(shownErrorMessages.length, 1);
    assert.match(shownErrorMessages[0], /did not finish starting within 0 seconds/);
    assert.match(shownErrorMessages[0], /approval is unchanged/);
    assert.strictEqual(h.clients.length, 2, "one fallback start");
  });

  test("a NEUTRAL server that never answers initialize is stopped, the operator told, and nothing hidden", async () => {
    // The bundled backend and an `nml.server.path` binary have no handshake
    // to fail and nothing to fall back to. Routed through the handshake
    // path, their silence was retired in silence: no message, an empty
    // status bar, and no way for the operator to know the server had been
    // ended.
    const h = makeHarness({ kind: "wasm", hangFirstStart: true, handshakeBudgetMs: 10 });
    await h.manager.start();
    assert.strictEqual(h.clients.length, 1, "nothing to fall back to");
    assert.strictEqual(h.processes[0].terminateCalls, 1, "the hung server was ended");
    assert.strictEqual(shownErrorMessages.length, 1, shownErrorMessages.join(" | "));
    assert.match(shownErrorMessages[0], /did not finish starting within 0 seconds/);
    assert.match(shownErrorMessages[0], /NML: Restart Language Server/);
    assert.strictEqual(h.manager.getLifecycleState(), "failed");
  });

  test("the neutral server is never held to a handshake", async () => {
    // No identity contract ⇒ an operator's own binary (nml.server.path, the
    // native default) is not made to prove itself: they chose it, and a fork
    // of the server answering a different name is their business.
    const h = makeHarness({ kind: "process", serverInfoName: "something-else" });
    await h.manager.start();
    assert.deepStrictEqual(shownErrorMessages, []);
    assert.strictEqual(h.clients.length, 1);
  });

  test("an unexpected close ends the session, says so ONCE, and reports disconnected", async () => {
    const h = makeHarness({ kind: "wasm" });
    await h.manager.start();

    h.clients[0].fireClosed();
    await settled();
    await settled();
    await settled();

    assert.strictEqual(h.processes[0].terminateCalls, 1);
    assert.strictEqual(h.clients[0].listenerDisposals, 1);
    assert.strictEqual(h.manager.getClient(), undefined);
    assert.strictEqual(h.manager.getLifecycleState(), "disconnected");
    assert.strictEqual(shownErrorMessages.length, 1, "one event, one notification");
    assert.match(shownErrorMessages[0], /stopped unexpectedly/);
    assert.match(shownErrorMessages[0], /fake wasm/);
  });

  test("the close handler tells the library the extension has handled it", () => {
    // Without `handled: true` the library ALSO raises its own generic
    // "Connection to server got closed" toast (measured), so one event became
    // two notifications saying different things.
    const h = makeHarness({ kind: "wasm" });
    return h.manager.start().then(() => {
      const result = h.clients[0].fireClosed() as { handled?: boolean };
      assert.strictEqual(result.handled, true);
    });
  });

  test("a start that fails cleans up the client, the process, and the listener", async () => {
    const h = makeHarness({ kind: "wasm", failFirstStart: true });
    await h.manager.start();

    assert.strictEqual(h.manager.getClient(), undefined);
    assert.strictEqual(h.manager.getLifecycleState(), "failed");
    assert.strictEqual(h.clients[0].listenerDisposals, 1);
    assert.strictEqual(h.processes[0].terminateCalls, 1);
    assert.strictEqual(shownErrorMessages.length, 1);
    assert.match(shownErrorMessages[0], /failed to start the NML language server/);
    // The bundled server's remedy is the WASI host, not "install a binary".
    assert.match(shownErrorMessages[0], /ms-vscode\.wasm-wasi-core/);
    assert.doesNotMatch(shownErrorMessages[0], /or install one/);
  });

  test("the LSP shutdown handshake is attempted ONLY on a running client", async () => {
    // `stop()` on a client that is still `starting` throws (measured), and so
    // does `dispose()`. Calling it anyway and swallowing the rejection is how
    // the extension used to report a stop that never happened — and it is
    // also two seconds of the library's own delayed kill timer for nothing.
    // The process is still ended: by us, on the ladder.
    const h = makeHarness({
      kind: "process",
      identity: true,
      hangFirstStart: true,
      handshakeBudgetMs: 10,
    });
    await h.manager.start();
    assert.strictEqual(h.clients[0].stopCalls, 0, "a shutdown was sent to a client that had none");
    assert.strictEqual(h.processes[0].terminateCalls, 1, "…but the process was still ended");
  });

  test("a start() that RESOLVES while the client is StartFailed is a failure, not a success", async () => {
    // MEASURED on the real library: when the connection closes during
    // `initialize`, `handleConnectionClosed` clears `_onStart` before
    // `start()`'s trailing `return this._onStart`, so the promise resolves
    // with `undefined` and the real rejection escapes unhandled. Treating a
    // resolved promise as "it started" leaves the editor claiming a running
    // server that is not there.
    const h = makeHarness({ kind: "wasm", firstStartResolvesFailed: true });
    await h.manager.start();
    assert.strictEqual(h.manager.getLifecycleState(), "failed");
    assert.strictEqual(h.processes[0].terminateCalls, 1, "the process is still ended");
    assert.strictEqual(shownErrorMessages.length, 1);
    assert.match(shownErrorMessages[0], /failed to start/);
  });
});

suite("clientManager/a retired session is inert", () => {
  setup(() => {
    resetStubRecords();
  });

  // The first of the two defects this design exists to close, ported from
  // the probe that found it. The abandoned provider's process is killed
  // (by us, or by the library's own delayed tree walk) LONG after the
  // fallback started; the library then calls `closed()` on the ABANDONED
  // client, and a handler that reached for "the current client" stopped the
  // healthy fallback instead.
  test("the abandoned provider's LATE close must not tear down the healthy fallback", async () => {
    const h = makeHarness({
      kind: "process",
      identity: true,
      hangFirstStart: true,
      handshakeBudgetMs: 10,
    });
    await h.manager.start();
    assert.strictEqual(h.clients.length, 2, "the fallback started");
    const fallback = h.clients[1];
    const fallbackProcess = h.processes[1];
    const stopsBefore = fallback.stopCalls;
    const terminationsBefore = fallbackProcess.terminateCalls;

    h.clients[0].fireClosed();
    await settled();
    await settled();
    await settled();

    assert.strictEqual(fallback.stopCalls, stopsBefore, "the healthy fallback was stopped");
    assert.strictEqual(
      fallbackProcess.terminateCalls,
      terminationsBefore,
      "the healthy fallback's process was ended"
    );
    assert.ok(h.manager.getClient(), "the editor still has a language server");
    assert.strictEqual(h.manager.getLifecycleState(), "running");
  });

  // The second defect, same shape, different callback.
  test("the abandoned provider's LATE initialize failure must not tear down the fallback", async () => {
    const h = makeHarness({
      kind: "process",
      identity: true,
      hangFirstStart: true,
      handshakeBudgetMs: 10,
    });
    await h.manager.start();
    const toastsAfterHandshake = shownErrorMessages.length;
    assert.strictEqual(toastsAfterHandshake, 1, "exactly one message so far");

    h.clients[0].fireInitializationFailed(new Error("late initialize failure"));
    await settled();
    await settled();
    await settled();

    assert.strictEqual(
      shownErrorMessages.length,
      toastsAfterHandshake,
      `a second, false start-failure toast: ${shownErrorMessages[1]}`
    );
    assert.ok(h.manager.getClient(), "the editor still has a language server");
  });
});

suite("clientManager/intents supersede, they do not queue", () => {
  setup(() => {
    resetStubRecords();
  });

  test("N restarts asked for in one tick produce exactly ONE new session", async () => {
    const h = makeHarness({ kind: "process" });
    await h.manager.start();
    assert.strictEqual(h.clients.length, 1);

    const burst = [
      h.manager.restart(),
      h.manager.restart(),
      h.manager.restart(),
      h.manager.restart(),
      h.manager.restart(),
    ];
    await Promise.all(burst);

    assert.strictEqual(h.clients.length, 2, "five intents, one new session");
    assert.strictEqual(h.processes.length, 2);
    // …and the superseded session was torn down exactly once.
    assert.strictEqual(h.processes[0].terminateCalls, 1);
  });

  test("a restart that arrives before the first session exists supersedes it, not queues behind it", async () => {
    const gate = new Gate();
    const h = makeHarness({ kind: "process", firstStartGate: gate });

    const first = h.manager.start();
    const second = h.manager.restart();
    await settled();
    // The superseded attempt got as far as re-resolving and then returned —
    // it never created a client, so there is exactly one session for two
    // intents. (The old operation queue ran BOTH.)
    assert.deepStrictEqual(h.events, [
      "private-cwd",
      "resolve",
      "resolve",
      "create:1",
      // The process is launched by the client, when it starts — a function
      // `ServerOptions`, so nothing is spawned until the client asks.
      "start:1",
      "launch",
    ]);

    gate.open();
    await first;
    await second;
    assert.strictEqual(h.clients.length, 1, "two intents, one session");
  });


  test("a stop asked for before the first session exists launches nothing, and says nothing", async () => {
    // The stop-shaped twin of the cell above, and a different outcome: the
    // restart re-resolves and runs a server, this one must run NOTHING. The
    // window is real — `activate()` awaits `start()`, and a window closed
    // while the private working directory is still being made arrives
    // exactly here. What makes it a cell worth stating is the SILENCE: the
    // superseded attempt is not a failure, so nothing is shown and the
    // status stays `absent` rather than becoming `failed`.
    const h = makeHarness({ kind: "process" });
    const starting = h.manager.start();
    const stopping = h.manager.stop();
    await starting;
    await stopping;

    assert.deepStrictEqual(
      h.events,
      ["private-cwd", "resolve"],
      "the superseded attempt got past its own resolution"
    );
    assert.strictEqual(h.clients.length, 0, "a server was started for an intent that was gone");
    assert.strictEqual(h.processes.length, 0);
    assert.deepStrictEqual(shownErrorMessages, []);
    assert.strictEqual(h.manager.getLifecycleState(), "absent");
  });

  test("two sessions never start concurrently: teardown completes before the next start", async () => {
    const h = makeHarness({ kind: "process" });
    await h.manager.start();
    await h.manager.restart();

    assert.deepStrictEqual(h.events, [
      "private-cwd",
      "resolve",
      "create:1",
      "start:1",
      "launch",
      "stop:1",
      "resolve",
      "create:2",
      "start:2",
      "launch",
    ]);
    assert.strictEqual(h.clients[0].listenerDisposals, 1);
    assert.strictEqual(h.processes[0].terminateCalls, 1);
  });

  test("a restart asked for during a hung handshake takes effect at once, not after the budget", async () => {
    // The budget is 60 s here. If the restart queued behind it, this test
    // would take a minute; the assertion is that it does not.
    const h = makeHarness({ kind: "process", hangFirstStart: true, handshakeBudgetMs: 60_000 });
    const hung = h.manager.start();
    await settled();
    assert.strictEqual(h.clients.length, 1);

    const began = Date.now();
    await h.manager.restart();
    const took = Date.now() - began;

    assert.ok(took < 2_000, `the restart waited ${took} ms for the handshake budget`);
    assert.strictEqual(h.clients.length, 2, "a fresh session");
    assert.strictEqual(h.processes[0].terminateCalls, 1, "the hung attempt's process was ended");
    // The superseded attempt reports nothing: it was not a failure, it was
    // replaced.
    assert.deepStrictEqual(shownErrorMessages, []);
    await hung;
  });

  test("deactivate during a hung handshake completes inside its own budget", async () => {
    const h = makeHarness({ kind: "process", hangFirstStart: true, handshakeBudgetMs: 60_000 });
    const hung = h.manager.start();
    await settled();

    const began = Date.now();
    await h.manager.deactivate();
    const took = Date.now() - began;

    assert.ok(took < 2_000, `deactivate waited ${took} ms`);
    assert.strictEqual(h.manager.getClient(), undefined);
    // …and it tore down on the DEACTIVATE profile, not the generous one.
    assert.deepStrictEqual(h.processes[0].profiles, [DEACTIVATE_TERMINATION]);
    await hung;
  });

  test("start() is idempotent: asking again while a server is wanted does not restart it", async () => {
    const h = makeHarness({ kind: "process" });
    await h.manager.start();
    await h.manager.start();
    await h.manager.start();
    assert.strictEqual(h.clients.length, 1);
  });
});

suite("clientManager/the operator's message is written from the verdict", () => {
  setup(() => {
    resetStubRecords();
  });

  test("a program that could NOT be ended names the process the operator must kill", async () => {
    const h = makeHarness({
      kind: "process",
      identity: true,
      serverInfoName: "sh",
      verdict: "survived",
    });
    await h.manager.start();
    assert.strictEqual(shownErrorMessages.length, 1);
    assert.match(shownErrorMessages[0], /still running as process 4242/);
    // …in a command THIS operator's shell has. The manager passes the real
    // platform, so the windows-latest leg of the extension workflow asserts
    // the Windows sentence and the POSIX legs assert the POSIX one.
    assert.match(
      shownErrorMessages[0],
      process.platform === "win32" ? /taskkill \/F \/PID 4242/ : /kill -9 4242/
    );
  });

  test("a program that DID end is not accused of still running", async () => {
    const h = makeHarness({ kind: "process", identity: true, serverInfoName: "sh" });
    await h.manager.start();
    assert.strictEqual(shownErrorMessages.length, 1);
    assert.doesNotMatch(shownErrorMessages[0], /still running/);
    assert.doesNotMatch(shownErrorMessages[0], /Reload Window/);
  });

  test("one that had to be killed reads the same as one that exited", async () => {
    const h = makeHarness({
      kind: "process",
      identity: true,
      serverInfoName: "sh",
      verdict: "killed",
    });
    await h.manager.start();
    assert.doesNotMatch(shownErrorMessages[0], /still running/);
    assert.doesNotMatch(shownErrorMessages[0], /killed/);
  });
});

suite("startFailureMessage/one remedy per server kind", () => {
  const HOME_CWD = launchSandbox(providerWorkingDir("/home/u", "/")).cwd;

  test("the bundled server names the WASI host and the native alternative", () => {
    const m = startFailureMessage({ kind: "wasm", module: {} as never, label: "neutral nml-lsp (wasm)" });
    assert.match(m, /^NML: failed to start the NML language server \(neutral nml-lsp \(wasm\)\)\./);
    assert.match(m, /ms-vscode\.wasm-wasi-core/);
    assert.match(m, /set nml\.server\.path/);
  });

  test("an nml.server.path binary is told to check the path or clear the setting", () => {
    const m = startFailureMessage({ kind: "process", command: "/x/nml-lsp", args: [], label: "neutral (nml.server.path)", origin: "setting", cwd: HOME_CWD, env: {} });
    assert.match(m, /Check that \/x\/nml-lsp exists and is executable/);
    assert.match(m, /clear nml\.server\.path/);
  });

  test("a provider tool is the project's declaration, which no setting overrides", () => {
    const m = startFailureMessage({ kind: "process", command: "/usr/local/bin/nudge", args: ["lsp"], label: "nudge (in-binary)", origin: "provider", cwd: HOME_CWD, env: {} });
    assert.match(m, /\/usr\/local\/bin\/nudge lsp/);
    assert.match(m, /nml-project\.nml/);
    assert.doesNotMatch(m, /clear nml\.server\.path/);
  });

  test("the native fallback of a build with no bundled server says to install one", () => {
    const m = startFailureMessage({ kind: "process", command: "/home/u/.cargo/bin/nml-lsp", args: [], label: "neutral nml-lsp", origin: "default", cwd: HOME_CWD, env: {} });
    assert.match(m, /bundles no server/);
    assert.match(
      m,
      /cargo install --locked --git https:\/\/github\.com\/nudge-io\/nml nml-lsp/
    );
  });

  test("an unexpected close names the server and the way back", () => {
    const m = connectionLostMessage("neutral nml-lsp (wasm)");
    assert.match(m, /neutral nml-lsp \(wasm\)/);
    assert.match(m, /stopped unexpectedly/);
    assert.match(m, /NML: Restart Language Server/);
  });
});

// ─────────────────────────────────────────────────────────────────────────
// THE CELLS OF THE INTENT × SESSION-STATE MATRIX THE SUITE ABOVE DOES NOT
// REACH. Each one is a state the reconciler can be in when the next intent
// arrives: a teardown in flight, a handshake in flight, a resolution that
// never returned a resolution at all.
// ─────────────────────────────────────────────────────────────────────────

suite("clientManager/an intent that arrives while a teardown is in flight", () => {
  setup(() => {
    resetStubRecords();
  });

  test("start() during a stop's teardown runs a new server AFTER it, never beside it", async () => {
    // The cell: desired=stopped, a session being retired, and the operator
    // asking for a server back (the `nml.server.path` setting being cleared
    // is exactly this — a stop-shaped intent followed by a start-shaped
    // one). The one guarantee the old operation queue gave, and the one
    // this reconciler has to keep, is that the two do not overlap.
    const gate = new Gate();
    const h = makeHarness({ kind: "process", terminateGate: gate });
    await h.manager.start();

    const stopping = h.manager.stop();
    await settled();
    assert.strictEqual(h.processes[0].terminateCalls, 1, "the teardown is in flight");

    const starting = h.manager.start();
    await settled();
    await settled();
    assert.strictEqual(
      h.clients.length,
      1,
      "a second session was created while the first was still being torn down"
    );

    gate.open();
    await stopping;
    await starting;

    assert.strictEqual(h.clients.length, 2, "the new server started once the old one was gone");
    assert.strictEqual(h.processes[0].terminateCalls, 1, "…and the old one was torn down once");
    assert.strictEqual(h.manager.getLifecycleState(), "running");
    assert.deepStrictEqual(shownErrorMessages, [], "nothing here is a failure");
  });

  test("a window closing during an interactive teardown does NOT re-budget it", async () => {
    // KNOWN, DELIBERATE, AND THE REASON THIS TEST EXISTS. `retire()` is
    // idempotent and the FIRST profile wins, so a teardown already climbing
    // the interactive ladder (worst case 7000 ms) keeps that budget when the
    // window closes — and VS Code stops waiting at 5000. The fallback that
    // makes it survivable is outside the extension: the editor host exits,
    // the supervisor's control pipe reaches EOF, and the provider's group is
    // killed. Pinned so that the day someone shortens the interactive
    // profile, or teaches the ladder to be tightened in flight, they change
    // this test on purpose rather than discovering the coupling.
    const gate = new Gate();
    const h = makeHarness({ kind: "process", terminateGate: gate });
    await h.manager.start();

    const stopping = h.manager.stop();
    await settled();
    const closing = h.manager.deactivate();
    await settled();

    gate.open();
    await stopping;
    await closing;

    assert.deepStrictEqual(
      h.processes[0].profiles,
      [INTERACTIVE_TERMINATION],
      "the deactivate profile reached the process, which this design does not do"
    );
    assert.strictEqual(h.manager.getClient(), undefined);
  });

  test("a teardown that throws is not reported as a server that failed to START", async () => {
    // The reconciler's own `catch` is the last line for anything that throws
    // on the way to the desired state. It must not tell the operator their
    // server "could not be started" when what they asked for was a stop:
    // the sentence carries a remedy (restart it), and the remedy for a stop
    // that failed is not to start it again.
    const h = makeHarness({ kind: "process", terminateThrows: true });
    await h.manager.start();
    resetStubRecords();

    await h.manager.stop();

    assert.deepStrictEqual(
      shownErrorMessages,
      [],
      `a stop that failed produced a start-failure message: ${shownErrorMessages.join(" | ")}`
    );
    assert.strictEqual(h.manager.getLifecycleState(), "failed");
  });
});

suite("clientManager/the server dies during its own handshake", () => {
  setup(() => {
    resetStubRecords();
  });

  test("one message, no false start-failure, and the session ends disconnected", async () => {
    // The cell nothing reached: `closed()` arrives while `start()` is still
    // outstanding, i.e. the server accepted the spawn and then died during
    // `initialize`. The close is an INTENT, so it aborts the attempt it
    // interrupts — and the attempt must then report NOTHING, because the
    // close already said what happened. Without the abort the attempt
    // outlives its own obituary and adds a second, contradictory toast
    // ("failed to start") to the first ("stopped unexpectedly").
    const h = makeHarness({ kind: "wasm", closeDuringFirstStart: true });
    await h.manager.start();
    await settled();
    await settled();
    await settled();

    assert.strictEqual(
      shownErrorMessages.length,
      1,
      `one event, one notification: ${shownErrorMessages.join(" | ")}`
    );
    assert.match(shownErrorMessages[0], /stopped unexpectedly/);
    assert.strictEqual(h.clients.length, 1, "a dropped connection is not a handshake to retry");
    assert.strictEqual(h.processes[0].terminateCalls, 1, "the process was still ended");
    assert.strictEqual(
      h.clients[0].stopCalls,
      0,
      "an LSP shutdown was sent to a client that never ran"
    );
    assert.strictEqual(h.clients[0].listenerDisposals, 1);
    assert.strictEqual(h.manager.getClient(), undefined);
    assert.strictEqual(h.manager.getLifecycleState(), "disconnected");
  });
});

suite("clientManager/the lifecycle itself fails", () => {
  setup(() => {
    resetStubRecords();
  });

  test("a resolution that throws is SAID, not only logged", async () => {
    // Everything that can throw on the way to a server throws BEFORE a
    // session exists — the resolution, the consent modal, the workspace-state
    // write behind a decline, the private working directory. None of the
    // per-server messages can be reached from there, so without this the
    // operator got a status bar reading "server failed" and no sentence
    // anywhere but a log channel they had no reason to open.
    const h = makeHarness({ kind: "process", resolveThrowsOnCall: 1 });
    await h.manager.start();

    assert.strictEqual(h.clients.length, 0, "nothing launched");
    assert.strictEqual(h.manager.getLifecycleState(), "failed");
    assert.deepStrictEqual(shownErrorMessages, [lifecycleFailureMessage()]);
    assert.match(shownErrorMessages[0], /could not be started/);
    assert.match(shownErrorMessages[0], /NML: Restart Language Server/);
  });

  test("…and the editor can still be asked again", async () => {
    // The bound is on ONE intent's ladder, not on the manager: a lifecycle
    // that threw must leave a manager that still works, or the remedy the
    // message names is a lie.
    const h = makeHarness({ kind: "process", resolveThrowsOnCall: 1 });
    await h.manager.start();
    assert.strictEqual(h.manager.getLifecycleState(), "failed");

    await h.manager.restart();
    assert.strictEqual(h.clients.length, 1, "the second attempt ran");
    assert.strictEqual(h.manager.getLifecycleState(), "running");
  });
});

suite("clientManager/the ladder's bound", () => {
  setup(() => {
    resetStubRecords();
  });

  test("every rung failing its handshake ends in `failed`, not in `absent`", async () => {
    // MAX_ATTEMPTS is a BOUND, and the cell nothing reached is the one where
    // it is actually spent: a repository whose declared provider is an
    // impostor, re-resolved to the same impostor. Two messages, two ended
    // processes, no third attempt — and a status that says the editor TRIED
    // and has no server, rather than "no server", which reads as if nothing
    // had been attempted.
    const h = makeHarness({ kind: "process", impostorEverywhere: true });
    await h.manager.start();

    assert.strictEqual(h.clients.length, 2, "MAX_ATTEMPTS launches, and no more");
    assert.strictEqual(h.launches.length, 2);
    assert.strictEqual(
      h.events.filter((e) => e === "resolve").length,
      2,
      "one resolution per attempt"
    );
    assert.deepStrictEqual(h.processes.map((p) => p.terminateCalls), [1, 1]);
    assert.strictEqual(h.identityCalls.repudiated, 2, "each impostor's approval is withdrawn");
    assert.strictEqual(shownErrorMessages.length, 2, shownErrorMessages.join(" | "));
    assert.strictEqual(h.manager.getClient(), undefined);
    assert.strictEqual(h.manager.getLifecycleState(), "failed");
    // The label of the last thing tried survives, so the status bar's
    // tooltip names a server rather than an empty parenthesis.
    assert.strictEqual(h.manager.getServerLabel(), "fake process");
  });
});

suite("clientManager/the trace setting never fails a server", () => {
  setup(() => {
    resetStubRecords();
  });

  test("a setTrace that rejects neither fails the start nor escapes as a rejection", async () => {
    // Two call sites, one rule. The configuration listener is
    // `void manager.applyTraceSetting()` — a rejection there is an unhandled
    // rejection in the extension host, minutes after the operator changed a
    // setting — and the launch must not lose a server that started because
    // its tracing could not be turned on. The library's `setTrace` writes to
    // the connection, which is exactly the thing that may have gone away.
    const h = makeHarness({ kind: "process", traceRejects: true });
    await h.manager.start();

    assert.strictEqual(h.manager.getLifecycleState(), "running", "the server started");
    assert.deepStrictEqual(shownErrorMessages, [], "and nothing was blamed on it");

    await assert.doesNotReject(
      () => h.manager.applyTraceSetting(),
      "the configuration listener's fire-and-forget call can reject"
    );
  });

  test("there is nothing to apply when no server is running", async () => {
    const h = makeHarness({ kind: "process", traceRejects: true });
    await assert.doesNotReject(() => h.manager.applyTraceSetting());
    assert.strictEqual(h.clients.length, 0);
  });
});


suite("clientManager/the ladder between its rungs", () => {
  setup(() => {
    resetStubRecords();
  });

  test("an intent that arrives between the rungs spends no further rung", async () => {
    // The ladder's OTHER supersession point. `launch()` re-reads the intent
    // at the top of every attempt and after every resolution, and only the
    // top-of-loop read can fire on the SECOND rung — the first is entered
    // straight from the reconciler, which has just read the same intent.
    // Reachable because the rung's last act is asynchronous: withdrawing an
    // approval is a `workspaceState.update`, and an operator who sees the
    // first impostor's message and stops the server lands inside it.
    //
    // What must NOT happen is a second program being launched on behalf of
    // an intent nobody holds any more — and the status must read `absent`,
    // the outcome of the stop, not the `failed` an exhausted ladder leaves.
    const gate = new Gate();
    const h = makeHarness({ kind: "process", impostorEverywhere: true, identityGate: gate });
    const starting = h.manager.start();
    await settled();
    await settled();
    assert.strictEqual(h.clients.length, 1, "the first rung has not reached its consequence");
    assert.strictEqual(h.identityCalls.repudiated, 1);

    const stopping = h.manager.stop();
    gate.open();
    await starting;
    await stopping;

    assert.strictEqual(h.clients.length, 1, "the second rung ran for an intent that was gone");
    assert.strictEqual(h.launches.length, 1);
    assert.strictEqual(
      h.events.filter((e) => e === "resolve").length,
      1,
      "the ladder re-resolved after it had been superseded"
    );
    assert.strictEqual(h.manager.getLifecycleState(), "absent");
    assert.strictEqual(shownErrorMessages.length, 1, shownErrorMessages.join(" | "));
  });
});

suite("clientManager/a session that could not be wired at all", () => {
  setup(() => {
    resetStubRecords();
  });

  test("a language client that could not be constructed is a start failure, not a wedged manager", async () => {
    // `ServerSession.start()` says it never throws, and everything inside it
    // is written to that rule — but the rule was not ENFORCED, and the two
    // seams that can break it are the library's constructor and (on the wasm
    // branch) `workspace.createFileSystemWatcher`, both of which throw while
    // the extension host is shutting down.
    //
    // What a throw cost, measured before this was closed: the manager had
    // already adopted the session (`this.current = session`) when the
    // exception escaped to the reconciler's `catch`, which sets `idle` and
    // says a sentence but does not retire anything. The session then stayed
    // `current` forever — so the status bar read the zombie's `absent`
    // instead of the `failed` that was just recorded, and `start()` became a
    // PERMANENT no-op: the reconciler sees a live session of the current
    // generation and returns. Only a restart (a new generation) could get a
    // server back.
    const h = makeHarness({ kind: "process", createClientThrowsOnCall: 1 });
    await h.manager.start();

    assert.strictEqual(h.clients.length, 0, "no client was constructed");
    assert.strictEqual(h.manager.getClient(), undefined);
    assert.strictEqual(
      h.manager.getLifecycleState(),
      "failed",
      "the manager is still holding the session that never started"
    );
    assert.strictEqual(shownErrorMessages.length, 1, shownErrorMessages.join(" | "));
    assert.match(shownErrorMessages[0], /failed to start the NML language server/);

    // The remedy the operator is offered is `start`-shaped (activation, the
    // walkthrough's button), so it has to work without a generation bump.
    await h.manager.start();
    assert.strictEqual(h.clients.length, 1, "asking again did nothing at all");
    assert.strictEqual(h.manager.getLifecycleState(), "running");
  });
});

suite("clientManager/a teardown that could not end the server", () => {
  setup(() => {
    resetStubRecords();
  });

  test("a plain stop that left the program running says nothing, and reports `absent`", async () => {
    // PINNED AS IT IS, not as it should be. Three paths append
    // `terminationNote(verdict, …)` to what the operator reads — a start
    // failure, an unanswered neutral server, a failed handshake — and the
    // ORDINARY stop is not one of them: `reconcile` discards the verdict
    // `retire()` returns, so a server that outlived SIGKILL leaves a status
    // bar reading "no server", one line in a log channel, and a process
    // still holding the workspace. A restart is the same path, and there it
    // also starts a SECOND server beside the survivor.
    const h = makeHarness({ kind: "process", verdict: "survived" });
    await h.manager.start();
    resetStubRecords();

    await h.manager.stop();

    assert.strictEqual(h.processes[0].terminateCalls, 1);
    assert.deepStrictEqual(shownErrorMessages, [], "this is the cell — if it speaks, re-pin it");
    assert.strictEqual(h.manager.getLifecycleState(), "absent");
    // …and the sentence it does not say is one this module can write: the
    // gap is that nobody asks for it here, not that there is nothing to say.
    assert.notStrictEqual(terminationNote("survived", 4242, process.platform), "");
  });
});


suite("clientManager/a close and a restart, in that order", () => {
  setup(() => {
    resetStubRecords();
  });

  test("a restart asked for while the dead server is being torn down keeps the RESTART's outcome", async () => {
    // The cell: `serverDied` is an INTENT, and its "and now the bar reads
    // disconnected" runs when the LOOP settles — but the loop that serves
    // the close also serves anything asked for while its teardown is in
    // flight. The toast the close raises says "Restart it with NML: Restart
    // Language Server", so an operator doing exactly that lands inside the
    // window, and a server that died because it crashes is a server whose
    // restart fails.
    //
    // Both states carry the same remedy, so what is at stake is the
    // SENTENCE: "the connection closed unexpectedly" describes the event
    // before last, while the editor has just failed to start a replacement.
    const gate = new Gate();
    const h = makeHarness({ kind: "wasm", terminateGate: gate, failStartOnCall: 2 });
    await h.manager.start();

    h.clients[0].fireClosed();
    await settled();
    assert.strictEqual(h.processes[0].terminateCalls, 1, "the close's teardown is in flight");

    const restarting = h.manager.restart();
    gate.open();
    await restarting;
    await settled();

    assert.strictEqual(h.clients.length, 2, "the restart never ran");
    assert.strictEqual(h.manager.getClient(), undefined, "the restart was supposed to fail");
    assert.strictEqual(
      h.manager.getLifecycleState(),
      "failed",
      "the close's outcome overwrote the restart's"
    );
  });
});

suite("clientManager/restarts across ticks", () => {
  setup(() => {
    resetStubRecords();
  });

  test("two restarts in two ticks are two sessions, each superseded one torn down once", async () => {
    // The companion to the one-tick burst: coalescing must not swallow an
    // intent that arrives after the previous one has been satisfied.
    const h = makeHarness({ kind: "process" });
    await h.manager.start();
    await h.manager.restart();
    await h.manager.restart();

    assert.strictEqual(h.clients.length, 3);
    assert.deepStrictEqual(
      h.processes.map((p) => p.terminateCalls),
      [1, 1, 0],
      "each replaced server ended once; the live one was left alone"
    );
    assert.strictEqual(h.manager.getLifecycleState(), "running");
    assert.deepStrictEqual(shownErrorMessages, []);
  });
});
