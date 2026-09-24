// The death-pipe supervisor: how a POSIX language server stops existing when
// the editor does.
//
// WHY THIS PROCESS EXISTS. A stdio language server is ended by closing its
// stdin — but only if someone is alive to close it. Three cases break that:
//
//   1. The extension host is SIGKILLed (a crash, `kill -9`, the OS reclaiming
//      memory). Nothing runs on the way out; the server is re-parented to pid 1
//      and keeps the workspace open for as long as the machine is up.
//   2. The server ignores stdin EOF, SIGTERM, SIGHUP and SIGINT. Nothing the
//      editor can send ends it; only SIGKILL does.
//   3. The server double-forks a worker. The worker is re-parented away from
//      the server, so a parent-child TREE walk never finds it — but it stays in
//      the server's PROCESS GROUP, so a group kill does.
//
// This process closes all three. It is the provider's PARENT and its process
// GROUP LEADER's parent: it spawns the provider `detached`, i.e. in a fresh
// group, so one `kill(-pid)` reaches the provider and everything it forked.
// It holds one extra inherited descriptor — fd 3, a pipe whose other end the
// editor holds — and when that pipe reaches EOF FOR ANY REASON, including the
// editor dying without running a line of its own code, it SIGKILLs the group.
// The kernel delivers that EOF; no timer, no polling, no `processId` watchdog.
//
// NO LSP BYTE PASSES THROUGH HERE. The provider inherits fds 0/1/2 — the same
// pipes the editor created — so the language client reads and writes the
// provider directly and this process is not in the data path: it cannot stall
// it, reorder it, or truncate it, and it needs no buffering of its own.
//
// AND NOTHING ELSE. fd 3 below is the editor's, not the provider's, and a
// provider that could write on it could report an exit it had not had (after
// which the editor signals nothing, ever) or read the `T`/`K` bytes meant for
// this process out of the stream. It does not reach it: a Node runtime sets
// FD_CLOEXEC on every descriptor it starts with (`uv_disable_stdio_inheritance`,
// from `node::InitializeOncePerProcess`), and libuv clears that flag only for
// the descriptors a spawn's `stdio` array names — the three above. That is a
// property of the runtime rather than of this file, so it is PINNED by a real
// process: `test/real/processLaunch.test.ts` runs a provider that tries the
// write and requires it to fail.
//
// Being the provider's parent is also what makes the kill SAFE: this process
// learns of the exit from its own `exit` event, on its own single thread,
// before the pid can be recycled — so it can never signal an innocent process
// that inherited the number.
//
// CONTRACT (fd 3, one JSON object per line, newline-terminated). The other
// half is `processLaunch.ts` — `readControlLine` parses what is written here,
// and `forceExit` writes what is read below:
//   supervisor -> editor   {"pid":N}                  the provider's pid
//                          {"error":"..."}            the provider could not be spawned
//                          {"exit":{"code":C,"signal":S}}  how the provider ended
//   editor -> supervisor   "T"   SIGTERM the provider's group
//                          "K"   SIGKILL the provider's group
//                          EOF   SIGKILL the provider's group (the editor is gone)
// The editor owns the budget between T and K: staging lives in one place
// (the extension's termination profile), not in two that can disagree.
//
// It must stay dependency-free CommonJS: it is run by `process.execPath`,
// which inside VS Code is the Electron helper with ELECTRON_RUN_AS_NODE=1 —
// a plain Node runtime with no extension, no bundler and no module resolution
// beyond Node's own. A source ratchet pins its imports to `child_process` and
// `net`.
"use strict";

const cp = require("child_process");
const net = require("net");

const [command, ...args] = process.argv.slice(2);

// The variable that made THIS process a Node runtime must not reach the
// provider: it is the editor's implementation detail, and an `nml-lsp` that
// happened to be an Electron application would silently become a Node script.
const env = { ...process.env };
delete env.ELECTRON_RUN_AS_NODE;

// A SOCKET, never `fs.createReadStream(null, {fd: 3})`. MEASURED: a file read
// of a pipe parks a libuv threadpool thread in a blocking `read(2)`, and this
// process then cannot exit while the editor holds the other end — so a
// provider that exits by itself (the NORMAL shutdown) left the supervisor
// alive and the editor never saw the exit. A socket is event-driven and never
// blocks a thread.
const control = new net.Socket({ fd: 3, readable: true, writable: true });

function tell(object) {
  try {
    control.write(`${JSON.stringify(object)}\n`);
  } catch {
    /* the editor is gone; the EOF handler is what matters now */
  }
}

const child = cp.spawn(command, args, {
  stdio: ["inherit", "inherit", "inherit"],
  detached: true,
  env,
});

let exited = false;

function killGroup(signal) {
  if (exited || child.pid === undefined) return;
  try {
    process.kill(-child.pid, signal);
  } catch {
    /* already gone, or never started */
  }
}

/** Leave with `code`, after the control line has actually been written. */
function leave(code) {
  const done = () => process.exit(code);
  try {
    control.end(done);
  } catch {
    done();
  }
  // A pipe the editor abandoned may never flush; never hang on it.
  setTimeout(done, 200).unref();
}

child.on("error", () => {
  exited = true;
  tell({ error: `could not run ${command}` });
  leave(127);
});

child.on("exit", (code, signal) => {
  exited = true;
  tell({ exit: { code: code === undefined ? null : code, signal: signal ?? null } });
  leave(code === null || code === undefined ? (signal ? 1 : 0) : code);
});

control.on("data", (bytes) => {
  const text = String(bytes);
  if (text.includes("K")) killGroup("SIGKILL");
  else if (text.includes("T")) killGroup("SIGTERM");
});

// The editor is gone — this is the whole reason this process exists.
control.on("end", () => {
  killGroup("SIGKILL");
  setTimeout(() => process.exit(1), 200).unref();
});
control.on("error", () => {
  killGroup("SIGKILL");
  process.exit(1);
});

// Only the pipe decides. A SIGTERM aimed at this process (a shell's Ctrl-C
// reaching the whole group, a supervisor sweep) must not orphan the provider
// by killing its parent out from under it.
for (const signal of ["SIGTERM", "SIGHUP", "SIGINT"]) process.on(signal, () => {});

if (child.pid !== undefined) tell({ pid: child.pid });
