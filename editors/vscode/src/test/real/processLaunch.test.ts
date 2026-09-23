import * as assert from "node:assert";
import { execFileSync, spawn } from "node:child_process";
import { chmodSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { LaunchedServer } from "../../processLaunch";
import { launchServerProcess, supervisorPath } from "../../processLaunch";
import { launchSandbox } from "../../pathSecurity";
import type { ProcessServer } from "../../serverResolution";
import { processServer } from "../../serverResolution";
import { TerminationProfile, TerminationVerdict } from "../../serverProcess";

// ─────────────────────────────────────────────────────────────────────────
// REAL PROCESSES. Not a fake, not a mock: a program that ignores SIGTERM,
// SIGHUP and SIGINT, never answers a byte of LSP, and double-forks a worker
// out of its own process TREE. Everything this design claims about ending a
// server is a claim about the kernel, and only the kernel can settle it.
//
// NO TEST MAY LEAVE A PROCESS BEHIND. Every suite's teardown asserts that
// nothing matching its private temp directory is still running — a probe that
// leaks a `sleep` loop poisons every measurement taken after it.
// ─────────────────────────────────────────────────────────────────────────

const POSIX = process.platform !== "win32";

/** Why this case is not running here, where it can be READ.
 *
 *  `this.skip()` marks a case pending and prints its TITLE and nothing else —
 *  no reporter has a field for a reason. Half of this file is about a
 *  mechanism the other platform does not have, so on each leg of the 3-OS
 *  matrix the reason is the result: without it, `windows-latest` prints four
 *  bare pending lines and a reader cannot tell a platform branch from a case
 *  somebody disabled. Each title also names the platform it belongs to, so
 *  the pending lines are legible even where this line is scrolled away. */
function whyNotHere(reason: string): void {
  console.log(`      · not run on ${process.platform}: ${reason}`);
}

/** Each stage expires fast on POSIX: what is under test is the LADDER and the
 *  kernel, not how long the extension is willing to wait. On Windows the forced
 *  stage is an async `taskkill /T /F` — loaded CI hosts need longer budgets than
 *  a laptop. */
const QUICK: TerminationProfile =
  process.platform === "win32"
    ? { label: "test", stopMs: 50, inputMs: 400, termMs: 3_000, killMs: 8_000 }
    : { label: "test", stopMs: 50, inputMs: 400, termMs: 400, killMs: 400 };

const HANG_SH = `#!/bin/sh
# A provider that answers nothing, ignores every polite signal, and
# double-forks a worker: the middle subshell exits at once, so the worker is
# re-parented to pid 1 and leaves this process's TREE while staying in its
# process GROUP. A \`pgrep -P\` tree walk misses it; a group kill does not.
trap '' TERM HUP INT
echo $$ > "$PIDF.provider"
( "$(dirname "$0")/worker.sh" & )
while :; do sleep 1; done
`;

const WORKER_SH = `#!/bin/sh
trap '' TERM HUP INT
echo $$ > "$PIDF.worker"
while :; do sleep 1; done
`;

const FORGE_SH = `#!/bin/sh
# A HOSTILE provider. The control channel between the editor and the
# supervisor is fd 3; this tries to write a forged report on it and records
# whether the descriptor was even there. A provider that could write here
# could tell the editor it had exited (after which nothing is ever signalled)
# and name any pid it liked for the editor's last-resort \`kill(-pid)\`.
echo $$ > "$PIDF.provider"
if echo '{"pid":1}' >&3 2>/dev/null; then
    echo open > "$PIDF.fd3"
else
    echo closed > "$PIDF.fd3"
fi
cat > /dev/null
exit 0
`;

const QUIET_SH = `#!/bin/sh
# A well-behaved stdio server: it ends when its stdin does.
echo $$ > "$PIDF.provider"
cat > /dev/null
exit 7
`;

const ENV_SH = `#!/bin/sh
# Writes the environment it was actually given, then waits for stdin EOF.
env > "$PIDF.env"
echo $$ > "$PIDF.provider"
cat > /dev/null
`;

let dir = "";

function fixture(name: string, body: string): string {
  const file = path.join(dir, name);
  writeFileSync(file, body);
  chmodSync(file, 0o755);
  return file;
}

/** Through the ONE constructor production uses, over a real sandbox minted
 *  for this suite's directory: the launch under test is the launch that
 *  ships, scrubbed environment included, rather than a look-alike literal
 *  (which the `SandboxCwd` brand no longer lets a test write anyway). A test
 *  may ADD names to the scrub — never replace it — to prove the overlay
 *  reaches the spawn with a variable nothing else could have removed. */
function resolution(
  command: string,
  args: string[] = [],
  alsoScrub: Readonly<Record<string, undefined>> = {}
): ProcessServer {
  const sandbox = launchSandbox(dir);
  return processServer(command, args, "test provider", "provider", {
    ...sandbox,
    env: { ...sandbox.env, ...alsoScrub },
  });
}

const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

/** `process.kill`, named so `sweep` reads as what it does. */
const kill = (pid: number, signal: NodeJS.Signals): void => {
  process.kill(pid, signal);
};

function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

/** Wait until `read` returns something, or give up.
 *
 *  The default is a LIVENESS wait, not a measurement: it bounds how long a
 *  freshly spawned shell gets to write its pid on a host that may be busy
 *  (a 5 s default expired once under the full gate, right after a cargo
 *  build). What the suite measures — that processes END — is bounded by
 *  the ladder's own profile, `QUICK`, not by this. */
const LIVENESS_MS = 20_000;

/** The suite's own bound, stated where the waits it has to cover are.
 *
 *  A test here can hold THREE liveness waits (a provider's pid file, its
 *  worker's, then the wait for both to be gone), and mocha's `--timeout`
 *  bounds the whole test, not each wait. At the CLI's 30 000 the second wait
 *  could never reach its own deadline: a slow host produced mocha's
 *  "Timeout of 30000ms exceeded" — which names no process, no directory and
 *  no pid file — instead of the message `until` was written to print. The
 *  suite therefore asks for room for its own waits plus the ladder and the
 *  teardown's grace, so the diagnostic the suite can give is the one that
 *  arrives. */
const SUITE_TIMEOUT_MS = 3 * LIVENESS_MS + 10_000;

async function until<T>(read: () => T | undefined, ms = LIVENESS_MS): Promise<T> {
  const deadline = Date.now() + ms;
  for (;;) {
    const value = read();
    if (value !== undefined) return value;
    if (Date.now() > deadline) {
      let holds = "(unreadable)";
      try {
        holds = readdirSync(dir).join(", ") || "(empty)";
      } catch {
        /* reported as unreadable */
      }
      throw new Error(
        `timed out waiting for the process to report; ${dir} holds: ${holds}; ` +
          `PIDF=${process.env.PIDF ?? "(unset)"}`
      );
    }
    await sleep(25);
  }
}

function pidFile(suffix: string): number | undefined {
  try {
    const value = Number(readFileSync(path.join(dir, `pids.${suffix}`), "utf8").trim());
    return Number.isFinite(value) && value > 0 ? value : undefined;
  } catch {
    return undefined;
  }
}

/** Every pid this suite knows it started: the supervisor / direct child
 *  (`child.pid`), and every process that wrote its own pid under the test
 *  directory — providers AND the workers they double-forked. Exact and
 *  tool-free on every platform: `process.kill(pid, 0)` tests existence on
 *  Windows too. (Pid reuse inside one test's few seconds is negligible.) */
function recordedPids(): number[] {
  const pids = new Set<number>();
  for (const started of launched) {
    if (started.child.pid !== undefined) pids.add(started.child.pid);
    if (started.server.pid !== undefined) pids.add(started.server.pid);
  }
  try {
    for (const name of readdirSync(dir)) {
      if (!name.startsWith("pids.")) continue;
      const value = Number(readFileSync(path.join(dir, name), "utf8").trim());
      if (Number.isFinite(value) && value > 0) pids.add(value);
    }
  } catch {
    /* the directory is already gone */
  }
  return [...pids];
}

function escapeWmiLikeLiteral(marker: string): string {
  return marker
    .replace(/\\/g, "\\\\")
    .replace(/'/g, "''")
    .replace(/\[/g, "[[]")
    .replace(/%/g, "[%-]")
    .replace(/_/g, "[_]");
}

/** Every process on the machine whose command line names the test
 *  directory — the net for a process the fixtures did NOT record. Each
 *  platform has its own listing tool, and a platform whose tool is MISSING
 *  fails the assertion rather than passing it: a check that cannot run must
 *  say so. (Without that rule this assertion was vacuous on Windows, where
 *  `pgrep` does not exist and the error was swallowed.) */
function processesMentioning(marker: string): string {
  if (process.platform === "win32") {
    const escaped = escapeWmiLikeLiteral(marker);
    const query =
      `Get-CimInstance Win32_Process -Filter "CommandLine LIKE '%${escaped}%'" | ` +
      "ForEach-Object { \"$($_.ProcessId) $($_.CommandLine)\" }";
    return execFileSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", query], {
      encoding: "utf8",
    }).trim();
  }
  try {
    return execFileSync("pgrep", ["-fl", marker], { encoding: "utf8" }).trim();
  } catch (err) {
    // pgrep exits 1 when nothing matches; anything else (ENOENT above all)
    // is the tool failing, and the assertion must not read that as "clean".
    const status = (err as { status?: number | null }).status;
    if (status === 1) return "";
    throw err;
  }
}

/** Everything still running that this suite started: the recorded pids that
 *  are still alive, then anything else whose command line names the test
 *  directory. The teardown's assertion and the report's evidence are the
 *  same text. */
async function stragglers(): Promise<string> {
  // A process that has been ENDED is not gone the same instant: the
  // supervisor leaves 200 ms after it reports the provider's exit, and a
  // killed child is a zombie until its parent reaps it. A leak is a process
  // that is still there after that grace, so the recorded pids get a bounded
  // wait — and only the ones that outlast it are reported.
  const deadline = Date.now() + (process.platform === "win32" ? 5_000 : 1_500);
  let leftover = recordedPids().filter(alive);
  while (leftover.length > 0 && Date.now() < deadline) {
    await sleep(25);
    leftover = recordedPids().filter(alive);
  }
  const alivePids = leftover.map((pid) => `recorded pid ${pid} still alive`);
  const named = processesMentioning(dir);
  return [...alivePids, named].filter(Boolean).join("\n");
}

/** Kill anything this suite started that is somehow still there.
 *
 *  Only ever a safety net — the assertion above has already recorded the
 *  failure. It exists because this file is the one place where a BROKEN
 *  mechanism (the state every mutation puts it in) can leave a `sleep` loop
 *  behind, and a leaked process poisons every measurement taken after it. */
function sweep(): void {
  for (const process_ of launched) {
    const pid = process_.server.pid;
    if (pid !== undefined) {
      try {
        kill(-pid, "SIGKILL");
      } catch {
        /* gone */
      }
    }
    try {
      process_.child.kill("SIGKILL");
    } catch {
      /* gone */
    }
  }
  for (const pid of recordedPids()) {
    try {
      if (process.platform === "win32") {
        execFileSync("taskkill", ["/T", "/F", "/PID", String(pid)], { stdio: "ignore" });
      } else {
        process.kill(pid, "SIGKILL");
      }
    } catch {
      /* gone */
    }
  }
  launched.length = 0;
  if (process.platform !== "win32") {
    try {
      execFileSync("pkill", ["-9", "-f", dir]);
    } catch {
      /* nothing matched */
    }
  }
}

/** Everything this suite launched, so the safety net can find it. */
const launched: LaunchedServer[] = [];

/** `launchServerProcess`, remembered. */
function launch(
  command: string,
  args: string[] = [],
  env: Readonly<Record<string, undefined>> = {}
): LaunchedServer {
  const started = launchServerProcess(resolution(command, args, env), () => undefined);
  launched.push(started);
  return started;
}

/** The provider's own environment, as it wrote it out: `KEY=value` per line. */
function providerEnv(): Map<string, string> | undefined {
  let text: string;
  try {
    text = readFileSync(path.join(dir, "pids.env"), "utf8");
  } catch {
    return undefined;
  }
  const seen = new Map<string, string>();
  for (const line of text.split("\n")) {
    const at = line.indexOf("=");
    if (at > 0) seen.set(line.slice(0, at), line.slice(at + 1));
  }
  // `env` is written in one `write`, but a reader can still catch a partial
  // file; the marker is the last thing the caller checks for, so require a
  // plausible size before believing it.
  return seen.size > 3 ? seen : undefined;
}

suite("processLaunch/ending a real server", function () {
  this.timeout(SUITE_TIMEOUT_MS);

  setup(() => {
    // Short root: the supervisor's own path travels in `argv`, and a temp
    // directory under `/var/folders/...` is most of a socket path's budget.
    dir = mkdtempSync(path.join(POSIX ? "/tmp" : os.tmpdir(), "nml-proc-"));
    process.env.PIDF = path.join(dir, "pids");
  });

  teardown(async () => {
    // The EVIDENCE is taken first — what was still running when the test
    // ended — and only then is anything cleaned up. A teardown that killed
    // first and looked afterwards would assert nothing.
    const left = await stragglers();
    sweep();
    const dead = dir;
    dir = "";
    rmSync(dead, { recursive: true, force: true });
    assert.strictEqual(left, "", `this test left processes running:\n${left}`);
  });

  test("POSIX: a provider that ignores TERM, HUP and INT — and its double-forked worker — are both dead after terminate()", async function () {
    if (!POSIX) {
      whyNotHere("Windows has no process groups; its forced stage is the tree kill below");
      this.skip();
      return;
    }
    fixture("worker.sh", WORKER_SH);
    const started = launch(fixture("hang.sh", HANG_SH));
    const provider = await until(() => pidFile("provider"));
    const worker = await until(() => pidFile("worker"));
    // The worker really did leave the provider's tree — otherwise this test
    // would pass with a tree walk, which is the thing that does not work.
    const parent = execFileSync("ps", ["-o", "ppid=", "-p", String(worker)], {
      encoding: "utf8",
    }).trim();
    assert.notStrictEqual(Number(parent), provider, "the worker is still a child of the provider");

    const verdict: TerminationVerdict = await started.server.terminate(QUICK);
    assert.strictEqual(verdict, "killed", "it had to be killed, and the verdict says so");
    assert.strictEqual(alive(provider), false, "the provider survived");
    assert.strictEqual(alive(worker), false, "the re-parented worker survived");
  });

  test("POSIX: a provider that ends on stdin EOF is never signalled, and the verdict is `exited`", async function () {
    if (!POSIX) {
      whyNotHere("this is the supervised launch; Windows spawns directly (`Windows:` cases below)");
      this.skip();
      return;
    }
    const started = launch(fixture("quiet.sh", QUIET_SH));
    const provider = await until(() => pidFile("provider"));
    const verdict = await started.server.terminate(QUICK);
    assert.strictEqual(verdict, "exited");
    assert.strictEqual(alive(provider), false);
    // …and the supervisor left with the provider's own exit code, which is
    // the regression pin for the fd-3 threadpool bug: read as a FILE, the
    // control descriptor parked a thread and the supervisor never exited.
    const exit = await started.server.exited;
    assert.strictEqual(exit.code, 7, "the provider's exit code did not reach the editor");
    assert.strictEqual(exit.signal, null);
  });

  test("POSIX: a hostile provider cannot reach the editor's control channel", async function () {
    if (!POSIX) {
      whyNotHere("there is no control channel on the Windows branch: the provider is spawned directly");
      this.skip();
      return;
    }
    const lines: string[] = [];
    const started = launchServerProcess(resolution(fixture("forge.sh", FORGE_SH)), (m) =>
      lines.push(m)
    );
    launched.push(started);
    const provider = await until(() => pidFile("provider"));
    const reach = await until(() => {
      try {
        return readFileSync(path.join(dir, "pids.fd3"), "utf8").trim() || undefined;
      } catch {
        return undefined;
      }
    });
    // The provider inherits fds 0/1/2 — the LSP pipes — and NOTHING else:
    // Node sets FD_CLOEXEC on every descriptor it starts with
    // (`uv_disable_stdio_inheritance`), and libuv clears it only for the fds
    // a spawn's `stdio` array names. Three of them here.
    assert.strictEqual(reach, "closed", "the provider was handed the control descriptor");

    assert.strictEqual(await started.server.terminate(QUICK), "exited");
    assert.strictEqual(
      started.server.pid,
      provider,
      "the editor believed a pid the supervisor did not report"
    );
    assert.ok(
      !lines.some((m) => m.includes("process 1.")),
      `a forged report reached the editor: ${lines.join(" | ")}`
    );
  });

  test("POSIX: the editor being SIGKILLed ends the provider and its worker", async function () {
    if (!POSIX) {
      whyNotHere("the death pipe is the POSIX mechanism; Windows uses the runtime's job object");
      this.skip();
      return;
    }
    fixture("worker.sh", WORKER_SH);
    const provider = fixture("hang.sh", HANG_SH);
    // A stand-in for the extension host: it launches through the real
    // `processLaunch` and then idles. It is SIGKILLed, so nothing it might
    // have run on the way out can be what ends the provider.
    const editor = fixture(
      "editor.js",
      `const { launchServerProcess } = require(${JSON.stringify(path.join(__dirname, "..", "..", "processLaunch.js"))});
       launchServerProcess({ kind: "process", command: ${JSON.stringify(provider)}, args: [], cwd: ${JSON.stringify(dir)}, env: {}, label: "x" }, () => {});
       setInterval(() => {}, 1000);
`
    );
    const host = spawn(process.execPath, [editor], { stdio: "ignore", env: process.env });
    const providerPid = await until(() => pidFile("provider"));
    const workerPid = await until(() => pidFile("worker"));

    process.kill(host.pid as number, "SIGKILL");
    await until(() => (alive(providerPid) ? undefined : true), 5_000);
    assert.strictEqual(alive(providerPid), false, "the provider outlived the editor");
    await until(() => (alive(workerPid) ? undefined : true), 5_000);
    assert.strictEqual(alive(workerPid), false, "the worker outlived the editor");
  });

  test("POSIX: the supervisor being killed ends the provider group from the editor side", async function () {
    if (!POSIX) {
      whyNotHere("there is no supervisor on Windows: the provider is spawned directly");
      this.skip();
      return;
    }
    fixture("worker.sh", WORKER_SH);
    const started = launch(fixture("hang.sh", HANG_SH));
    const provider = await until(() => pidFile("provider"));
    const worker = await until(() => pidFile("worker"));

    // The supervisor is the mechanism; this is what happens when the
    // mechanism itself is taken away. The editor still holds the pid its
    // parent reported, and sweeps the group rather than leaving an orphan.
    process.kill(started.child.pid as number, "SIGKILL");
    await until(() => (alive(provider) ? undefined : true), 5_000);
    assert.strictEqual(alive(provider), false, "an orphaned provider was left running");
    assert.strictEqual(alive(worker), false, "an orphaned worker was left running");
  });

  test("POSIX: the scrub reaches the provider's environment, and the supervisor's own variable does not", async function () {
    if (!POSIX) {
      whyNotHere("the direct-spawn branch has no supervisor to scrub after");
      this.skip();
      return;
    }
    // NOTHING asserted this end to end. The unit harness checks the
    // resolution's `env` overlay on the way IN (what `launchServerProcess` is
    // handed); the real suite launched with `env: {}` and never looked. So
    // the two things the overlay exists for — `undefined` really removing an
    // inherited variable at `spawn`, and the supervisor deleting the
    // ELECTRON_RUN_AS_NODE that made IT a Node runtime — were carried by
    // reading, on the extension's sharpest edge (loader injection into a
    // program the editor starts).
    //
    // Two-sided by construction: an unrelated variable set the same way MUST
    // arrive, or "absent" would only mean the provider got no environment.
    // EMPTY values, deliberately. A loader variable with a real-looking path
    // makes the LAUNCH fail rather than the assertion fire (macOS `dyld`
    // refuses a missing insert, and the supervisor is a plain Node binary
    // that gets the variable too), so a broken scrub would go red for the
    // wrong reason and the message would name a timeout instead of a leak.
    // Empty is inert to both loaders and still prints as `LD_PRELOAD=`.
    //
    // NML_TEST_SCRUBBED is the platform-independent half: nothing but this
    // overlay can explain its absence, whereas an absent DYLD_* could also
    // be the kernel stripping it from a restricted interpreter.
    process.env.LD_PRELOAD = "";
    process.env.DYLD_INSERT_LIBRARIES = "";
    process.env.NML_TEST_SCRUBBED = "leaked";
    process.env.NML_TEST_INHERITED = "yes";
    try {
      const started = launch(fixture("env.sh", ENV_SH), [], {
        LD_PRELOAD: undefined,
        DYLD_INSERT_LIBRARIES: undefined,
        NML_TEST_SCRUBBED: undefined,
      });
      const seen = await until(() => providerEnv());
      assert.strictEqual(
        seen.get("NML_TEST_INHERITED"),
        "yes",
        "the provider inherited nothing at all, so the absences below prove nothing"
      );
      assert.strictEqual(
        seen.get("NML_TEST_SCRUBBED"),
        undefined,
        "the scrub overlay did not reach the spawn"
      );
      assert.strictEqual(seen.get("LD_PRELOAD"), undefined, "the scrub did not reach the spawn");
      assert.strictEqual(
        seen.get("DYLD_INSERT_LIBRARIES"),
        undefined,
        "the scrub did not reach the spawn"
      );
      assert.strictEqual(
        seen.get("ELECTRON_RUN_AS_NODE"),
        undefined,
        "the supervisor passed on the variable that made IT a Node runtime"
      );
      assert.strictEqual(await started.server.terminate(QUICK), "exited");
    } finally {
      delete process.env.LD_PRELOAD;
      delete process.env.DYLD_INSERT_LIBRARIES;
      delete process.env.NML_TEST_SCRUBBED;
      delete process.env.NML_TEST_INHERITED;
    }
  });

  test("every platform: the supervisor ships beside the code that runs it", function () {
    // A path that is right in the SOURCE tree and wrong in the VSIX is the
    // failure mode this catches before the E2E does — and only a DIRECTORY
    // comparison catches it. `endsWith(basename(dirname(file)) + sep +
    // "launchSupervisor.js")` is true of EVERY path ending in that name,
    // whatever directory it names, so it asserted the filename twice.
    // MEASURED: with `supervisorPath()` returning
    // `path.join(__dirname, "..", "src", "launchSupervisor.js")` — a real,
    // working script that the VSIX does not ship (`.vscodeignore` excludes
    // `src/**`) — this suite, the unit suite and the E2E all stayed green,
    // because every one of them runs from the checkout.
    //
    // The claim is about the module that RUNS it, so `require.resolve` names
    // that module rather than a path spelled out a second time here.
    const file = supervisorPath();
    const runner = require.resolve("../../processLaunch");
    assert.strictEqual(
      path.dirname(file),
      path.dirname(runner),
      `the supervisor is not in the directory of ${runner}, which is the only one the VSIX ships`
    );
    assert.strictEqual(path.basename(file), "launchSupervisor.js");
    assert.doesNotThrow(() => readFileSync(file, "utf8"));
  });

  // ── the Windows branch ───────────────────────────────────────────────
  //
  // Skipped on POSIX and never executed on the machine this was written on.
  // They exist so that the `windows-latest` leg of the extension workflow
  // tests something: without them that leg would run a suite whose every
  // process case skips itself, which is the vacuity this repository keeps
  // paying for. What is NOT tested anywhere is the job object — a child dying
  // with the editor is a property of the editor dying, and `@vscode/test-cli`
  // has no way to kill its own host.

  test("Windows: a provider that ends on stdin EOF is never forced", async function () {
    if (POSIX) {
      whyNotHere("there is no direct-spawn branch on POSIX; the `POSIX:` cases cover it");
      this.skip();
      return;
    }
    const started = launch(process.execPath, [
      "-e",
      "process.stdin.on('end', () => process.exit(7)); process.stdin.resume();",
      "--",
      dir, // names the test directory on the command line, for the straggler net
    ]);
    const provider = started.child.pid as number;
    assert.strictEqual(await started.server.terminate(QUICK), "exited");
    assert.strictEqual(alive(provider), false);
    assert.strictEqual((await started.server.exited).code, 7, "its exit code did not reach us");
  });

  test("Windows: the provider AND its own child are ended, root-first", async function () {
    if (POSIX) {
      whyNotHere("there is no direct-spawn branch on POSIX; the `POSIX:` cases cover it");
      this.skip();
      return;
    }
    // libuv's job object is created with SILENT_BREAKAWAY_OK, so it does NOT
    // cover the provider's own subprocesses. Ending the provider alone would
    // orphan them and leave a later tree kill with no root to walk from,
    // which is why both forced stages are `taskkill /T /F` on the live root.
    const script =
      "const cp=require('child_process');" +
      "const g=cp.spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'ignore'});" +
      "require('fs').writeFileSync(process.env.PIDF+'.worker',String(g.pid));" +
      "process.stdin.resume(); setInterval(()=>{},1000);";
    const started = launch(process.execPath, ["-e", script, "--", dir]);
    const provider = started.child.pid as number;
    const worker = await until(() => pidFile("worker"));
    assert.strictEqual(await started.server.terminate(QUICK), "killed");
    await until(() => (alive(worker) ? undefined : true), 5_000);
    assert.strictEqual(alive(provider), false, "the provider survived");
    assert.strictEqual(alive(worker), false, "the provider's own child was orphaned");
  });
});
