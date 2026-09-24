import * as assert from "node:assert";
import {
  DEACTIVATE_BUDGET_MS,
  DEACTIVATE_TERMINATION,
  ExitInfo,
  INTERACTIVE_TERMINATION,
  ProcessControl,
  TerminationProfile,
  deactivateWorstCaseMarginMs,
  exitNote,
  serverProcessOf,
  stagedTerminate,
  terminationNote,
  worstCaseMs,
} from "../../serverProcess";

/** A process whose every stage is under the test's control. */
class FakeControl implements ProcessControl {
  readonly calls: string[] = [];
  private info: ExitInfo | undefined;
  private settle!: (info: ExitInfo) => void;
  readonly exited: Promise<ExitInfo>;
  /** Which stage makes it go: 1 = stdin EOF, 2 = SIGTERM, 3 = SIGKILL, 0 = never. */
  constructor(
    readonly diesAt: 0 | 1 | 2 | 3,
    /** Not `readonly`: a supervised launch learns the provider's pid from
     *  the control pipe AFTER the process exists, and the ladder has to read
     *  it through the seam rather than having copied it. */
    public pid: number | undefined = 4242
  ) {
    this.exited = new Promise<ExitInfo>((resolve) => {
      this.settle = (info): void => {
        if (this.info) return;
        this.info = info;
        resolve(info);
      };
    });
  }
  hasExited(): boolean {
    return this.info !== undefined;
  }
  requestExit(): void {
    this.calls.push("requestExit");
    if (this.diesAt === 1) this.settle({ code: 0, signal: null });
  }
  forceExit(hard: boolean): void {
    this.calls.push(hard ? "kill" : "term");
    if (this.diesAt === 2 && !hard) this.settle({ code: null, signal: "SIGTERM" });
    if (this.diesAt === 3 && hard) this.settle({ code: null, signal: "SIGKILL" });
  }
  /** It went away on its own, before anyone asked. */
  exitByItself(info: ExitInfo = { code: 0, signal: null }): void {
    this.settle(info);
  }
}

/** Every stage expires at once — the ladder, not the clock, is under test. */
const IMPATIENT: TerminationProfile = {
  label: "test",
  stopMs: 1,
  inputMs: 5,
  termMs: 5,
  killMs: 5,
};

suite("serverProcess/the staged termination ladder", () => {
  test("a process that ends on stdin EOF is never signalled, and the verdict is `exited`", async () => {
    const control = new FakeControl(1);
    assert.strictEqual(await stagedTerminate(control, IMPATIENT), "exited");
    // The whole reason the first stage exists: a well-behaved server must not
    // be killed just because the editor is in a hurry.
    assert.deepStrictEqual(control.calls, ["requestExit"]);
  });

  test("a process that ignores stdin EOF is SIGTERMed, and the verdict is `killed`", async () => {
    const control = new FakeControl(2);
    assert.strictEqual(await stagedTerminate(control, IMPATIENT), "killed");
    assert.deepStrictEqual(control.calls, ["requestExit", "term"]);
  });

  test("a process that ignores SIGTERM is SIGKILLed, and the verdict is `killed`", async () => {
    const control = new FakeControl(3);
    assert.strictEqual(await stagedTerminate(control, IMPATIENT), "killed");
    assert.deepStrictEqual(control.calls, ["requestExit", "term", "kill"]);
  });

  test("a process that survives SIGKILL is reported as `survived`, not as stopped", async () => {
    // The defect this whole file exists for: the extension used to tell the
    // operator a program had been stopped because a promise resolved.
    const control = new FakeControl(0);
    assert.strictEqual(await stagedTerminate(control, IMPATIENT), "survived");
    assert.deepStrictEqual(control.calls, ["requestExit", "term", "kill"]);
  });

  test("a process that already exited is not signalled at all", async () => {
    // Signalling a pid that is gone is how an innocent process the kernel
    // handed the number to gets killed.
    const control = new FakeControl(0);
    control.exitByItself();
    assert.strictEqual(await stagedTerminate(control, IMPATIENT), "exited");
    assert.deepStrictEqual(control.calls, []);
  });

  test("terminate() runs the ladder ONCE however many callers ask", async () => {
    const control = new FakeControl(3);
    const server = serverProcessOf("fake", control);
    const [a, b] = await Promise.all([
      server.terminate(IMPATIENT),
      server.terminate(IMPATIENT),
    ]);
    assert.strictEqual(a, "killed");
    assert.strictEqual(b, "killed");
    assert.deepStrictEqual(control.calls, ["requestExit", "term", "kill"]);
    assert.strictEqual(await server.terminate(IMPATIENT), "killed");
    assert.deepStrictEqual(control.calls, ["requestExit", "term", "kill"]);
  });
});

suite("serverProcess/one ladder, whoever asks", () => {
  /** Long enough that a test can tell it apart from the impatient one by
   *  the clock, short enough to spend three of them. */
  const PATIENT: TerminationProfile = {
    label: "patient",
    stopMs: 1,
    inputMs: 120,
    termMs: 120,
    killMs: 120,
  };

  test("a second terminate() on a SHORTER profile does not shorten the ladder in flight", async () => {
    // The shape in production: the operator restarts a hung server (the
    // INTERACTIVE profile, 7000 ms worst case) and closes the window before
    // it has finished (the DEACTIVATE profile, 2500 ms, because VS Code
    // stops waiting at 5000). `terminate` is idempotent and the FIRST
    // profile wins, so the second caller waits out the first one's stages.
    // That is the design — a ladder cannot un-schedule a wait it is already
    // inside — and this is the pin that says so out loud, because the
    // comment at the call site reads as though the newer profile applied.
    const control = new FakeControl(0);
    const server = serverProcessOf("fake", control);
    const began = Date.now();
    const slow = server.terminate(PATIENT);
    const fast = server.terminate(IMPATIENT);
    assert.strictEqual(fast, slow, "two callers, one ladder, one promise");
    assert.strictEqual(await fast, "survived");
    assert.ok(
      Date.now() - began >= 300,
      `the ladder ran the impatient profile's stages instead (${Date.now() - began} ms)`
    );
    assert.deepStrictEqual(control.calls, ["requestExit", "term", "kill"], "one pass, not two");
  });

  test("a terminate() after the ladder finished returns the verdict, it does not re-run", async () => {
    // A pid that has been reused is killed by a second ladder as surely as
    // by a first one: the memo is the mechanism that stops it.
    const control = new FakeControl(1);
    const server = serverProcessOf("fake", control);
    assert.strictEqual(await server.terminate(IMPATIENT), "exited");
    assert.strictEqual(await server.terminate(IMPATIENT), "exited");
    assert.deepStrictEqual(control.calls, ["requestExit"]);
  });

  test("the ladder never spends `stopMs`: that stage belongs to the client", async () => {
    // `worstCaseMs` sums all four stages because the SESSION spends `stopMs`
    // on the LSP `shutdown`/`exit` round trip before it hands the process to
    // this ladder. A reader who took the ladder to own all four would budget
    // the deactivate profile wrong — and so would a change that "tidied" the
    // stop into `stagedTerminate`.
    const control = new FakeControl(1);
    const began = Date.now();
    assert.strictEqual(
      await stagedTerminate(control, { ...IMPATIENT, stopMs: 30_000 }),
      "exited"
    );
    assert.ok(Date.now() - began < 1_000, "the ladder waited on the client's stage");
  });

  test("the pid is read THROUGH the control, so a pid learned late still reaches the message", async () => {
    // The supervised launch does not know the provider's pid at spawn: it
    // arrives on the control pipe a moment later. A `ServerProcess` that had
    // copied the pid at construction would name `undefined` in the one
    // message that exists to name a process.
    const control = new FakeControl(0);
    // A default parameter cannot express "no pid yet": `undefined` is what
    // selects the default. It is set here, as the control pipe sets it.
    control.pid = undefined;
    const server = serverProcessOf("fake", control);
    assert.strictEqual(server.pid, undefined);
    control.pid = 777;
    assert.strictEqual(server.pid, 777);
    assert.match(terminationNote("survived", server.pid, "darwin"), /process 777/);
  });
});

suite("serverProcess/what the operator is told, written from the verdict", () => {
  test("`exited` and `killed` read the same: nothing extra is said", () => {
    for (const platform of ["darwin", "linux", "win32"]) {
      assert.strictEqual(terminationNote("exited", 99, platform), "");
      assert.strictEqual(terminationNote("killed", 99, platform), "");
    }
  });

  test("`survived` names the process, because the operator has to end it", () => {
    for (const platform of ["darwin", "linux"]) {
      const note = terminationNote("survived", 4242, platform);
      assert.match(note, /still running as process 4242/);
      assert.match(note, /kill -9 4242/);
    }
  });

  test("`survived` on Windows names a command Windows has", () => {
    // The forced stage there is `taskkill /T /F` (processLaunch.ts): there is
    // no `kill` on the operator's PATH, so the one sentence that hands the
    // job back to a person must not be written in POSIX.
    const note = terminationNote("survived", 4242, "win32");
    assert.match(note, /still running as process 4242/);
    assert.match(note, /taskkill \/F \/PID 4242/);
    assert.doesNotMatch(note, /kill -9/);
  });

  test("`survived` with no pid says so rather than inventing one", () => {
    for (const platform of ["darwin", "win32"]) {
      const note = terminationNote("survived", undefined, platform);
      assert.match(note, /may still be running/);
      assert.doesNotMatch(note, /process undefined/);
    }
  });

  test("an exit is logged with its code or its signal", () => {
    assert.strictEqual(exitNote({ code: 0, signal: null }), "exited with code 0");
    assert.strictEqual(exitNote({ code: 101, signal: null }), "exited with code 101");
    assert.strictEqual(exitNote({ code: null, signal: "SIGKILL" }), "ended by SIGKILL");
    assert.strictEqual(exitNote({ code: null, signal: null }), "ended for an unknown reason");
  });
});

suite("serverProcess/the deactivate budget", () => {
  test("the worst case of the deactivate ladder fits inside VS Code's 5000 ms", () => {
    // VS Code: `Promise.race([timeout(5000), deactivateAll()])`
    // (extHostExtensionService.ts). Past it the host stops waiting, so a
    // teardown that does not fit is a teardown that does not happen.
    const worst = worstCaseMs(DEACTIVATE_TERMINATION);
    assert.ok(
      worst < DEACTIVATE_BUDGET_MS,
      `the deactivate profile's worst case is ${worst} ms, VS Code allows ${DEACTIVATE_BUDGET_MS}`
    );
    // Not merely "inside": the margin has to absorb a loaded machine, the
    // resolution work and every other extension deactivating at once.
    assert.ok(
      deactivateWorstCaseMarginMs() >= 2_000,
      `only ${deactivateWorstCaseMarginMs()} ms of margin`
    );
  });

  test("the interactive profile is the generous one, and every stage is bounded", () => {
    assert.ok(worstCaseMs(INTERACTIVE_TERMINATION) > worstCaseMs(DEACTIVATE_TERMINATION));
    for (const profile of [INTERACTIVE_TERMINATION, DEACTIVATE_TERMINATION]) {
      for (const stage of [profile.stopMs, profile.inputMs, profile.termMs, profile.killMs]) {
        assert.ok(stage > 0 && Number.isFinite(stage), `${profile.label}: unbounded stage`);
      }
    }
  });
});
