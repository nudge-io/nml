// MUST come first: routes `require("vscode")` to the stub before any module
// that (transitively) imports the real extension-host API is loaded.
import "../support/installVscodeStub";

import * as assert from "node:assert";
import type { ExtensionContext, LogOutputChannel, Uri } from "vscode";
import type { WasmProcess } from "@vscode/wasm-wasi/v1";
import {
  CloseAction,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  Trace,
} from "vscode-languageclient/node";
import { NmlClientManager, NmlClientManagerDeps } from "../../clientManager";
import type { ServerResolution } from "../../providerDiscovery";
import type { WasmServer } from "../../serverAcquisition";
import type { NmlLogs } from "../../logging";
import { resetStubRecords, shownErrorMessages } from "../support/vscodeStub";

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

class FakeWasmProcess {
  terminateCalls = 0;
  terminate(): Promise<number> {
    this.terminateCalls += 1;
    return Promise.resolve(0);
  }
}

class FakeLanguageClient {
  stopCalls = 0;
  listenerDisposals = 0;
  trace: Trace | undefined;

  constructor(
    private readonly id: number,
    private readonly events: string[],
    readonly serverOptions: ServerOptions,
    readonly clientOptions: LanguageClientOptions,
    private readonly behavior: (self: FakeLanguageClient) => Promise<void>
  ) {}

  onDidChangeState(): { dispose(): void } {
    return {
      dispose: (): void => {
        this.listenerDisposals += 1;
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
    await this.behavior(this);
  }

  stop(): Promise<void> {
    this.stopCalls += 1;
    this.events.push(`stop:${this.id}`);
    return Promise.resolve();
  }

  setTrace(value: Trace): Promise<void> {
    this.trace = value;
    return Promise.resolve();
  }
}

interface HarnessOptions {
  kind: "wasm" | "process";
  /** When present, the FIRST client's start() blocks until the gate opens. */
  firstStartGate?: Gate;
  /** When true, the FIRST client's start() rejects (after wiring the server). */
  failFirstStart?: boolean;
}

interface Harness {
  manager: NmlClientManager;
  events: string[];
  clients: FakeLanguageClient[];
  wasmProcesses: FakeWasmProcess[];
  stateChanges: { count: number };
}

function makeHarness(options: HarnessOptions): Harness {
  const events: string[] = [];
  const clients: FakeLanguageClient[] = [];
  const wasmProcesses: FakeWasmProcess[] = [];
  const stateChanges = { count: 0 };

  const resolution: ServerResolution =
    options.kind === "wasm"
      ? {
          kind: "wasm",
          module: undefined as unknown as Uri,
          label: "fake wasm",
        }
      : { kind: "process", command: "/fake/nml-lsp", args: [], label: "fake process" };

  const logs: NmlLogs = {
    client: undefined as unknown as LogOutputChannel,
    trace: undefined as unknown as LogOutputChannel,
    info: () => undefined,
    warn: () => undefined,
    error: () => undefined,
    showClient: () => undefined,
    showTrace: () => undefined,
  };

  const deps: NmlClientManagerDeps = {
    resolveServer: async () => {
      events.push("resolve");
      return resolution;
    },
    createWasmServer: async () => {
      const proc = new FakeWasmProcess();
      wasmProcesses.push(proc);
      events.push("wasm-create");
      return {
        transports: undefined,
        process: proc as unknown as WasmProcess,
      } as unknown as WasmServer;
    },
    createLanguageClient: (_id, _name, serverOptions, clientOptions) => {
      const n = clients.length + 1;
      const behavior = async (self: FakeLanguageClient): Promise<void> => {
        await self.startServerIfFunction();
        if (n === 1 && options.firstStartGate) await options.firstStartGate.promise;
        if (n === 1 && options.failFirstStart) throw new Error("fake start failure");
      };
      const client = new FakeLanguageClient(n, events, serverOptions, clientOptions, behavior);
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
  return { manager, events, clients, wasmProcesses, stateChanges };
}

/** Drain the microtask chains behind fire-and-forget lifecycle handlers. */
function settled(): Promise<void> {
  return new Promise((resolve) => setImmediate(resolve));
}

suite("clientManager/NmlClientManager lifecycle", () => {
  setup(() => {
    resetStubRecords();
  });

  test("start() wires the wasm server and stop() terminates its process", async () => {
    const h = makeHarness({ kind: "wasm" });
    await h.manager.serialize(() => h.manager.start());

    assert.strictEqual(h.wasmProcesses.length, 1);
    assert.strictEqual(h.manager.getServerLabel(), "fake wasm");
    assert.strictEqual(h.manager.getLifecycleState(), "starting");
    assert.strictEqual(h.clients[0].trace, Trace.Off);

    await h.manager.serialize(() => h.manager.stop());
    assert.strictEqual(h.wasmProcesses[0].terminateCalls, 1);
    assert.strictEqual(h.clients[0].stopCalls, 1);
    // The per-start state subscription must die with its client.
    assert.strictEqual(h.clients[0].listenerDisposals, 1);
    assert.strictEqual(h.manager.getClient(), undefined);
    assert.strictEqual(h.manager.getLifecycleState(), "absent");
  });

  test("restart() serializes after an in-flight start", async () => {
    const gate = new Gate();
    const h = makeHarness({ kind: "process", firstStartGate: gate });

    const first = h.manager.serialize(() => h.manager.start());
    const second = h.manager.serialize(() => h.manager.restart());

    await settled();
    // The queued restart must not have begun while start #1 is in flight.
    assert.deepStrictEqual(h.events, ["resolve", "create:1", "start:1"]);

    gate.open();
    await first;
    await second;
    assert.deepStrictEqual(h.events, [
      "resolve",
      "create:1",
      "start:1",
      "stop:1",
      "resolve",
      "create:2",
      "start:2",
    ]);
    assert.strictEqual(h.clients[0].listenerDisposals, 1);
  });

  test("connection close terminates the wasm process and reports disconnected", async () => {
    const h = makeHarness({ kind: "wasm" });
    await h.manager.serialize(() => h.manager.start());

    const closed = h.clients[0].clientOptions.errorHandler?.closed();
    assert.deepStrictEqual(closed, { action: CloseAction.DoNotRestart });
    await settled();
    await settled();

    assert.strictEqual(h.wasmProcesses[0].terminateCalls, 1);
    assert.strictEqual(h.clients[0].stopCalls, 1);
    assert.strictEqual(h.clients[0].listenerDisposals, 1);
    assert.strictEqual(h.manager.getClient(), undefined);
    assert.strictEqual(h.manager.getLifecycleState(), "disconnected");
  });

  test("failStart cleans up the client, wasm process, and listener", async () => {
    const h = makeHarness({ kind: "wasm", failFirstStart: true });
    await h.manager.serialize(() => h.manager.start());

    assert.strictEqual(h.manager.getClient(), undefined);
    assert.strictEqual(h.manager.getLifecycleState(), "failed");
    assert.strictEqual(h.clients[0].stopCalls, 1);
    assert.strictEqual(h.clients[0].listenerDisposals, 1);
    assert.strictEqual(h.wasmProcesses[0].terminateCalls, 1);
    assert.strictEqual(shownErrorMessages.length, 1);
    assert.match(shownErrorMessages[0], /failed to start the NML language server/);
  });
});
