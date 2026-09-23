// ─────────────────────────────────────────────────────────────────────────
// What the extension owns when it runs a language server, and how it ends one.
//
// Pure: no `vscode`, no `child_process`. `processLaunch.ts` supplies the
// POSIX/Windows implementation and `serverAcquisition.ts` the WASM adapter;
// everything a test needs to drive the ladder is in the [`ProcessControl`]
// seam, so the staging below is exercised with fakes AND with real processes
// that ignore signals.
//
// THE RULE THE LADDER EXISTS FOR: the verdict is OBSERVED, never assumed. The
// extension used to tell the operator a program had been stopped because a
// promise resolved; the program was still running. Every sentence about a
// server's fate below is derived from [`TerminationVerdict`], which is derived
// from the process's own `exit` event.
// ─────────────────────────────────────────────────────────────────────────

/** How a process ended, as its parent observed it. */
export interface ExitInfo {
  readonly code: number | null;
  readonly signal: string | null;
}

/** The OBSERVED outcome of [`ServerProcess.terminate`].
 *
 *  `exited`  — it ended on its own after being asked (stdin EOF, or the LSP
 *              `shutdown`/`exit` handshake). Nothing was signalled.
 *  `killed`  — it ended after a signal.
 *  `survived` — it is still running, and the editor has run out of levers. */
export type TerminationVerdict = "exited" | "killed" | "survived";

/** The budget of one teardown, stage by stage. Every stage is bounded, so the
 *  worst case is a sum this file can be asked for — see [`worstCaseMs`]. */
export interface TerminationProfile {
  /** For the log: which teardown this is. */
  readonly label: string;
  /** The LSP `shutdown`/`exit` round trip, when the client is running. */
  readonly stopMs: number;
  /** After the provider's stdin is closed. */
  readonly inputMs: number;
  /** After SIGTERM to the provider's process group. */
  readonly termMs: number;
  /** After SIGKILL to the provider's process group. */
  readonly killMs: number;
}

/** The whole teardown, worst case, in ms. */
export function worstCaseMs(profile: TerminationProfile): number {
  return profile.stopMs + profile.inputMs + profile.termMs + profile.killMs;
}

/** What VS Code gives `deactivate()`: `Promise.race([timeout(5000),
 *  deactivateAll()])` in `extHostExtensionService.ts`. Past it the extension
 *  host stops waiting and exits — every lever the extension still had is gone,
 *  so a teardown that does not fit inside this number is a teardown that does
 *  not happen. */
export const DEACTIVATE_BUDGET_MS = 5_000;

/** Stop / restart, asked for by a person who is watching. Generous: nothing is
 *  about to be torn down around us. */
export const INTERACTIVE_TERMINATION: TerminationProfile = {
  label: "stop",
  stopMs: 2_000,
  inputMs: 2_000,
  termMs: 2_000,
  killMs: 1_000,
};

/** The window is closing. Every stage is short on purpose: the whole ladder
 *  has to finish, SIGKILL included, well inside [`DEACTIVATE_BUDGET_MS`] —
 *  `deactivateWorstCaseMarginMs` is the proof, and a test reads it. */
export const DEACTIVATE_TERMINATION: TerminationProfile = {
  label: "deactivate",
  stopMs: 800,
  inputMs: 700,
  termMs: 700,
  killMs: 300,
};

/** What is left of VS Code's `deactivate` budget after the worst teardown.
 *  Positive by a wide margin, or the profile is wrong. */
export function deactivateWorstCaseMarginMs(): number {
  return DEACTIVATE_BUDGET_MS - worstCaseMs(DEACTIVATE_TERMINATION);
}

/** The levers the ladder pulls, and the one fact it reads. Implemented over a
 *  real child process, over the WASM process, and over fakes. */
export interface ProcessControl {
  /** The provider's pid where the platform has one — for the operator's
   *  message when nothing worked. */
  readonly pid: number | undefined;
  /** Resolves when the process is observed to end. Never rejects. */
  readonly exited: Promise<ExitInfo>;
  /** Whether the exit has ALREADY been observed. Read ONCE, before the first
   *  stage, so a process that ended before anyone asked is never signalled —
   *  that is how a pid the kernel has recycled gets killed.
   *
   *  Once, and not before every stage, because between a stage's wait
   *  expiring and the next signal there is no point at which it could change:
   *  `settledWithin` loses its race in a macrotask, the ladder resumes on the
   *  microtask that follows it, and an exit observed by a process event or a
   *  socket line is a macrotask that cannot run in between. A re-read there
   *  would be a guard against a window that JavaScript does not have — and a
   *  guard that can never fire is a guard nobody can test. */
  hasExited(): boolean;
  /** Stage 1: ask it to end. A stdio server exits on stdin EOF; the WASM
   *  backend has `terminate()` and nothing else. */
  requestExit(): void;
  /** Stages 2 and 3: SIGTERM, then SIGKILL, to the provider's process GROUP
   *  (a tree walk misses a double-forked worker; the group does not). */
  forceExit(hard: boolean): void;
}

/** Await `promise`, or give up after `ms`, or stop the moment `signal`
 *  aborts — whichever happens first.
 *
 *  THE ONE BOUNDED WAIT. Every wait in this extension's server lifecycle is
 *  this function: each stage of the ladder below, the LSP `shutdown` round
 *  trip, and the `initialize` budget. One place clears the timer, one place
 *  drops the abort listener, and one place decides what "it did not happen"
 *  means — three near-copies of this used to answer in three different
 *  types, which is how a fourth gets written.
 *
 *  The loser of the race is NOT abandoned but it is also not cancelled: a
 *  `promise` that can reject must already carry its own `catch` before it
 *  gets here, or it becomes an unhandled rejection in the extension host
 *  minutes after the operator moved on.
 *
 *  Without the SIGNAL, a `stop`, `restart` or window close asked for during
 *  a long wait would queue behind it — and a closing window has only
 *  [`DEACTIVATE_BUDGET_MS`]. */
export function settledWithin(
  promise: Promise<unknown>,
  ms: number,
  signal?: AbortSignal
): Promise<"settled" | "expired" | "aborted"> {
  if (signal?.aborted) return Promise.resolve("aborted");
  let timer: ReturnType<typeof setTimeout> | undefined;
  let onAbort: (() => void) | undefined;
  const racing: Promise<"settled" | "expired" | "aborted">[] = [
    promise.then((): "settled" => "settled"),
    new Promise<"expired">((resolve) => {
      timer = setTimeout(() => resolve("expired"), ms);
    }),
  ];
  if (signal) {
    racing.push(
      new Promise<"aborted">((resolve) => {
        onAbort = (): void => resolve("aborted");
        signal.addEventListener("abort", onAbort, { once: true });
      })
    );
  }
  return Promise.race(racing).finally(() => {
    if (timer) clearTimeout(timer);
    if (signal && onAbort) signal.removeEventListener("abort", onAbort);
  });
}

/** Await the OBSERVED exit, or give up after `ms`. A stage with no budget
 *  reads the fact the control already has rather than scheduling a timer. */
async function exitObserved(control: ProcessControl, ms: number): Promise<boolean> {
  if (ms <= 0) return control.hasExited();
  return (await settledWithin(control.exited, ms)) === "settled";
}

/** End a server process, and say what actually happened.
 *
 *  Ask (stdin EOF) → SIGTERM the group → SIGKILL the group, each stage
 *  bounded, each one skipped the moment the exit is observed. The return value
 *  is the observation, not the intent. */
export async function stagedTerminate(
  control: ProcessControl,
  profile: TerminationProfile
): Promise<TerminationVerdict> {
  if (control.hasExited()) return "exited";
  control.requestExit();
  if (await exitObserved(control, profile.inputMs)) return "exited";
  control.forceExit(false);
  if (await exitObserved(control, profile.termMs)) return "killed";
  control.forceExit(true);
  if (await exitObserved(control, profile.killMs)) return "killed";
  return "survived";
}

/** A running server the extension owns: one object, one teardown path, for the
 *  native child process and the WASM backend alike. */
export interface ServerProcess {
  /** What the operator calls this server (the resolution's label). */
  readonly label: string;
  /** The provider's pid, once known; `undefined` for the WASM backend, which
   *  has no process of its own on the host. */
  readonly pid: number | undefined;
  readonly exited: Promise<ExitInfo>;
  terminate(profile: TerminationProfile): Promise<TerminationVerdict>;
}

/** Build a [`ServerProcess`] from a [`ProcessControl`] — the single teardown
 *  path both backends go through. `terminate` is idempotent and safe to call
 *  concurrently: the ladder runs once, and every caller gets the same verdict. */
export function serverProcessOf(label: string, control: ProcessControl): ServerProcess {
  let running: Promise<TerminationVerdict> | undefined;
  return {
    label,
    get pid(): number | undefined {
      return control.pid;
    },
    exited: control.exited,
    terminate(profile: TerminationProfile): Promise<TerminationVerdict> {
      if (!running) running = stagedTerminate(control, profile);
      return running;
    },
  };
}

/** The sentence the operator reads about a server that was told to stop.
 *
 *  `exited` and `killed` read the SAME — whether a signal was needed is the
 *  editor's business, and a message that said "killed" would invite the
 *  question "should I worry?" for which the honest answer is no. `survived` is
 *  the one the operator must act on, so it names the process they will need.
 *
 *  And it names it in a command THIS operator's shell has. The forced stage
 *  is already two mechanisms — a POSIX group signal, `taskkill /T /F` on
 *  Windows (`processLaunch.ts`) — so the one sentence that hands the job back
 *  to a person cannot be written in one of them: `kill -9` is not a program on
 *  Windows, and a remedy that cannot be run is worse than none, because the
 *  operator spends their next minutes on the instruction rather than on the
 *  process. `platform` is a PARAMETER, as [`classifyResolutionDirectory`]'s
 *  is, so both sentences are rendered by a unit test on either host. */
export function terminationNote(
  verdict: TerminationVerdict,
  pid: number | undefined,
  platform: string
): string {
  if (verdict !== "survived") return "";
  if (pid === undefined) return "It could not be ended from here and may still be running.";
  const byHand =
    platform === "win32" ? `taskkill /F /PID ${pid}` : `kill -9 ${pid}`;
  return (
    `It could not be ended from here and is still running as process ${pid}; ` +
    `end it from a terminal (${byHand}).`
  );
}

/** How a server ended, for the log. The language client stopped recording
 *  this the moment the extension took ownership of the process, and an exit
 *  code is the first thing anyone wants when a server disappears. */
export function exitNote(info: ExitInfo): string {
  if (info.signal) return `ended by ${info.signal}`;
  if (info.code === null) return "ended for an unknown reason";
  return `exited with code ${info.code}`;
}
