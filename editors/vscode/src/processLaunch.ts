import { ChildProcess, execFile, spawn } from "node:child_process";
import * as path from "node:path";
import type { Socket } from "node:net";
import type { ProcessServer } from "./serverResolution";
import {
  ExitInfo,
  ProcessControl,
  ServerProcess,
  serverProcessOf,
} from "./serverProcess";

// ─────────────────────────────────────────────────────────────────────────
// THE ONLY MODULE IN THIS EXTENSION THAT SPAWNS A PROCESS.
//
// Pinned by a source ratchet (`sourceRatchet.test.ts`): no other file under
// `src/` may import `child_process`. Spawning is the extension's sharpest
// edge — a working directory, an environment, a process group and a way to end
// what was started — and it is reviewable only if it happens in one place.
//
// The sandbox is required BY TYPE, and the type is what enforces it: the only
// way to describe a process-backed server is [`ProcessServer`], whose `cwd` is
// a BRANDED `SandboxCwd` that only `pathSecurity.launchSandbox` can mint
// (private empty cwd + the scrubbed-environment overlay), stamped onto the
// resolution by the one `serverResolution.processServer` constructor. A
// hand-written `{ kind: "process", cwd: someFolder, env: {} }` does not
// compile, so there is no launch path that can reach this file with half of
// it — including from a test, which is where the look-alike literals were.
//
// TWO PLATFORMS, TWO MECHANISMS, ONE INTERFACE:
//
//   POSIX   — through `launchSupervisor.js`, run by `process.execPath` with
//             ELECTRON_RUN_AS_NODE=1 (inside VS Code that path is the Electron
//             helper; on a remote/WSL host it is plain Node, where the
//             variable is ignored). The supervisor holds a control pipe on
//             fd 3 and group-kills the provider when that pipe reaches EOF —
//             so the provider dies with the editor even when the editor is
//             SIGKILLed. No LSP byte passes through it: the provider inherits
//             fds 0/1/2, which are the very pipes the language client reads
//             and writes.
//
//   WINDOWS — spawned DIRECTLY, never `detached`. libuv puts a non-detached
//             child into a global job object with
//             JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE (`src/win/process.c`), so the
//             provider already dies with the editor, however the editor dies.
//             A supervisor would add a process and take that away. The job is
//             created with SILENT_BREAKAWAY_OK, so the provider's OWN
//             subprocesses are not covered — which is why the forced stage is
//             `taskkill /T /F`, a TREE kill, and not `child.kill()`.
// ─────────────────────────────────────────────────────────────────────────

/** The file the POSIX launch runs. Beside this module in every layout: the
 *  esbuild bundle (`dist/`) and the tsc output (`out/`) both receive a copy —
 *  see `scripts/toolchain.mjs`, and `gate-ext-package` fails if the packaged
 *  VSIX does not carry one. */
export function supervisorPath(): string {
  return path.join(__dirname, "launchSupervisor.js");
}

/** A server process the extension owns, plus the handle the language client
 *  is given. The client receives it as `{ process, detached: true }`, for
 *  which `vscode-languageclient` pipes stderr to the log and builds the
 *  message streams — and records NO `_serverProcess`, so it never runs its own
 *  `pgrep -P` tree walk behind our back (`lib/node/main.js`). Lifecycle is
 *  wholly ours. */
export interface LaunchedServer {
  readonly child: ChildProcess;
  readonly server: ServerProcess;
}

/** A place for the launch to say what happened. */
export type LaunchLog = (message: string) => void;

// ── the control channel, as DATA ─────────────────────────────────────────
//
// The supervisor is the only party that can write here: fd 3 is not among
// the descriptors the provider inherits. The provider gets 0/1/2 and nothing
// else, because Node sets FD_CLOEXEC on every descriptor it starts with
// (`uv_disable_stdio_inheritance`, called from `node::InitializeOncePerProcess`)
// and libuv clears that flag only for the fds a spawn's `stdio` array names —
// three of them here. MEASURED on the real supervisor with a `/bin/sh`
// provider: `echo >&3` is EBADF, and the forged line never reaches the editor
// (the real-process suite pins it).
//
// That is a property of the RUNTIME, though, not of this file, and what rides
// on it is sharp: a `pid` read from here becomes `process.kill(-pid)`, where
// 1 means "every process this account may signal" and 0 means "this editor's
// own process group". So the channel is parsed as untrusted data — bounded
// lines, and a pid that has to name a process before it can name a group.

/** The most one control line may accumulate before the editor gives up on
 *  it. The supervisor's lines are a pid, a short error or an exit record: a
 *  line that passes this without a newline is not one of them, and holding
 *  it would grow the extension host's heap with no bound at all. */
export const MAX_CONTROL_LINE = 64 * 1024;

/** What a control line reports. */
export interface ControlReport {
  readonly pid?: number;
  readonly error?: string;
  readonly exit?: ExitInfo;
}

/** Whole lines out of a stream, plus what is left over.
 *
 *  A remainder that reaches [`MAX_CONTROL_LINE`] with no newline in it is
 *  DROPPED rather than carried: the next newline resynchronises the stream,
 *  and nothing accumulates. */
export function takeControlLines(
  buffered: string,
  chunk: string
): { readonly lines: string[]; readonly rest: string } {
  let rest = buffered + chunk;
  const lines: string[] = [];
  for (let nl = rest.indexOf("\n"); nl >= 0; nl = rest.indexOf("\n")) {
    lines.push(rest.slice(0, nl));
    rest = rest.slice(nl + 1);
  }
  return { lines, rest: rest.length > MAX_CONTROL_LINE ? "" : rest };
}

/** One control line, read as data. Anything that is not the contract is
 *  nothing: a line that does not parse, a pid that is not a whole number, and
 *  a pid that names no process (`kill(-1)` is every process this account may
 *  signal; `kill(-0)` is the editor's own group). */
export function parseControlLine(text: string): ControlReport | undefined {
  let report: { pid?: unknown; error?: unknown; exit?: { code?: unknown; signal?: unknown } };
  try {
    report = JSON.parse(text) as typeof report;
  } catch {
    return undefined;
  }
  if (report === null || typeof report !== "object") return undefined;
  const out: { pid?: number; error?: string; exit?: ExitInfo } = {};
  if (typeof report.pid === "number" && Number.isSafeInteger(report.pid) && report.pid > 1) {
    out.pid = report.pid;
  }
  if (typeof report.error === "string") out.error = report.error;
  if (report.exit && typeof report.exit === "object") {
    out.exit = {
      code: typeof report.exit.code === "number" ? report.exit.code : null,
      signal: typeof report.exit.signal === "string" ? report.exit.signal : null,
    };
  }
  return out;
}

/** The environment a provider is spawned with: everything inherited, minus
 *  what the scrub removes (each such key is present with value `undefined`,
 *  which is what makes Node omit it), plus whatever the launch itself needs. */
function spawnEnv(
  resolution: ProcessServer,
  extra: Readonly<Record<string, string>>
): NodeJS.ProcessEnv {
  return { ...process.env, ...resolution.env, ...extra };
}

/** Run a provider on this platform, and own it. */
export function launchServerProcess(
  resolution: ProcessServer,
  log: LaunchLog
): LaunchedServer {
  return process.platform === "win32"
    ? launchDirect(resolution, log)
    : launchSupervised(resolution, log);
}

// ── POSIX ────────────────────────────────────────────────────────────────

function launchSupervised(resolution: ProcessServer, log: LaunchLog): LaunchedServer {
  const child = spawn(
    process.execPath,
    [supervisorPath(), resolution.command, ...resolution.args],
    {
      cwd: resolution.cwd,
      // The supervisor needs a Node runtime out of `process.execPath`; it
      // deletes the variable again before spawning the provider, so the
      // provider's environment is exactly the scrubbed one.
      env: spawnEnv(resolution, { ELECTRON_RUN_AS_NODE: "1" }),
      // fd 3 is the control pipe. Its EOF — however it arrives — is what ends
      // the provider, so the editor holding this descriptor IS the liveness
      // signal. Nothing is polled and no pid is watched.
      stdio: ["pipe", "pipe", "pipe", "pipe"],
      windowsHide: true,
    }
  );

  let providerPid: number | undefined;
  let exitInfo: ExitInfo | undefined;
  let settle: (info: ExitInfo) => void = () => undefined;
  const exited = new Promise<ExitInfo>((resolve) => {
    settle = (info): void => {
      if (exitInfo) return;
      exitInfo = info;
      resolve(info);
    };
  });

  const control = child.stdio[3] as Socket | null | undefined;
  let controlOpen = control !== null && control !== undefined;
  let buffered = "";
  control?.on("data", (bytes: Buffer) => {
    const taken = takeControlLines(buffered, String(bytes));
    buffered = taken.rest;
    for (const one of taken.lines) readControlLine(one);
  });
  control?.on("error", () => {
    controlOpen = false;
  });

  /** One line of the supervisor's report. The grammar is stated ONCE, in
   *  `launchSupervisor.js`'s CONTRACT block; this function is its only other
   *  implementation, so a change to either half belongs in the same edit as
   *  the other. An unparsable line is dropped rather than fatal: the pipe
   *  carries a report, never the LSP stream (which is fds 0/1/2), so nothing
   *  the operator depends on rides on it. */
  function readControlLine(text: string): void {
    const report = parseControlLine(text);
    if (!report) return;
    if (report.pid !== undefined) {
      providerPid = report.pid;
      log(`${resolution.label} is running as process ${report.pid}.`);
    }
    if (report.error !== undefined) log(`${resolution.label}: ${report.error}`);
    if (report.exit) settle(report.exit);
  }

  /** The supervisor died without reporting the provider's exit: it was killed,
   *  or it crashed. The provider is then an orphan that nothing else knows
   *  about — this is the last moment the editor holds its pid, so the group
   *  goes now rather than at a later teardown that may never come.
   *
   *  The pid was told to us by the provider's own PARENT, which is the only
   *  party that could not have raced a recycled number; with that parent gone
   *  a recycled-pid race is no longer excluded, and it is the reason this is
   *  the LAST resort rather than the mechanism. */
  function orphanSweep(): void {
    if (providerPid === undefined) return;
    try {
      process.kill(-providerPid, "SIGKILL");
      log(`${resolution.label}: the launch supervisor died; ended process group ${providerPid}.`);
    } catch {
      /* already gone */
    }
  }

  child.on("error", (err) => {
    log(`${resolution.label} could not be launched: ${err}`);
    settle({ code: null, signal: null });
  });
  child.on("exit", () => {
    controlOpen = false;
    if (!exitInfo) {
      orphanSweep();
      settle({ code: null, signal: null });
    }
  });

  const control_: ProcessControl = {
    get pid(): number | undefined {
      return providerPid ?? child.pid;
    },
    exited,
    hasExited: () => exitInfo !== undefined,
    requestExit: () => {
      // The provider's stdin IS this pipe (it inherited fd 0). A stdio
      // language server ends on EOF — measured on `nml-lsp`, and the reason
      // the LSP `exit` notification alone is not enough for one.
      try {
        child.stdin?.end();
      } catch {
        /* already closed */
      }
    },
    forceExit: (hard) => {
      if (controlOpen && control) {
        try {
          control.write(hard ? "K" : "T");
          return;
        } catch {
          controlOpen = false;
        }
      }
      // No supervisor left to ask: signal the group ourselves.
      if (providerPid === undefined) return;
      try {
        process.kill(-providerPid, hard ? "SIGKILL" : "SIGTERM");
      } catch {
        /* already gone */
      }
    },
  };

  return { child, server: serverProcessOf(resolution.label, control_) };
}

// ── Windows ──────────────────────────────────────────────────────────────

/** The tree-killer, by ABSOLUTE path.
 *
 *  `taskkill` as a bare name is resolved by libuv's own Windows path search
 *  (`src/win/process.c`, `search_path`), which — for a name with no directory
 *  in it — looks in the process's CURRENT DIRECTORY first and only then scans
 *  `PATH`, appending `.com` before `.exe`. The extension host's working
 *  directory is inherited from whatever launched the editor, so `code .` in a
 *  repository makes that repository's root the first place a `taskkill.com`
 *  would be found — and this is the one call the extension makes with a bare
 *  program name. `resolveOnPath` already refuses relative `PATH` entries for
 *  exactly this reason (`providerDiscovery.ts`); the same hazard reaches in
 *  through this door.
 *
 *  `%SystemRoot%` is the operator's own environment, and a value that is not
 *  an absolute path is not a Windows directory: the documented default stands
 *  in, so a cleared or relative variable cannot turn this back into a
 *  directory-relative lookup. */
export function taskkillProgram(env: NodeJS.ProcessEnv = process.env): string {
  const root = env.SystemRoot ?? env.SYSTEMROOT ?? env.systemroot;
  const base = root !== undefined && path.win32.isAbsolute(root) ? root : "C:\\Windows";
  return path.win32.join(base, "System32", "taskkill.exe");
}

function launchDirect(resolution: ProcessServer, log: LaunchLog): LaunchedServer {
  const child = spawn(resolution.command, resolution.args, {
    cwd: resolution.cwd,
    env: spawnEnv(resolution, {}),
    stdio: ["pipe", "pipe", "pipe"],
    // NEVER `detached` here: detaching is exactly what takes the provider OUT
    // of the editor's job object, i.e. what would make it survive the editor.
    detached: false,
    windowsHide: true,
  });

  let exitInfo: ExitInfo | undefined;
  let settle: (info: ExitInfo) => void = () => undefined;
  const exited = new Promise<ExitInfo>((resolve) => {
    settle = (info): void => {
      if (exitInfo) return;
      exitInfo = info;
      resolve(info);
    };
  });
  child.on("error", (err) => {
    log(`${resolution.label} could not be launched: ${err}`);
    settle({ code: null, signal: null });
  });
  child.on("exit", (code, signal) => settle({ code, signal }));
  if (child.pid !== undefined) log(`${resolution.label} is running as process ${child.pid}.`);

  const control: ProcessControl = {
    get pid(): number | undefined {
      return child.pid;
    },
    exited,
    hasExited: () => exitInfo !== undefined,
    requestExit: () => {
      try {
        child.stdin?.end();
      } catch {
        /* already closed */
      }
    },
    forceExit: () => {
      const pid = child.pid;
      if (pid === undefined) return;
      // BOTH forced stages are the same TREE kill, and that is deliberate.
      //
      // Windows has no SIGTERM for a console child with no console, so there
      // is no graceful stage to offer: `child.kill()` is TerminateProcess on
      // the provider ALONE — and the job object libuv puts it in is created
      // with SILENT_BREAKAWAY_OK, so the provider's own subprocesses are NOT
      // in it. Ending the provider first would therefore ORPHAN its children
      // and leave the later tree kill with no root to walk from. `taskkill
      // /T /F` runs while the root is still alive, which is the only ordering
      // that reaches the whole tree. The two stages differ in how long the
      // editor waits, not in what it does.
      //
      // Async: a synchronous `execFileSync` here would block the extension
      // host's only thread — which is what the language client's own teardown
      // does, and what this design exists to stop.
      execFile(
        taskkillProgram(),
        ["/T", "/F", "/PID", String(pid)],
        { windowsHide: true },
        () => undefined
      );
    },
  };

  return { child, server: serverProcessOf(resolution.label, control) };
}
