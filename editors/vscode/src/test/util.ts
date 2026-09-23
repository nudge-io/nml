import * as assert from "node:assert";
import { execFileSync } from "node:child_process";
import * as vscode from "vscode";
import type { NmlExtensionApi } from "../extension";

/** The anti-vacuity check every E2E launch runs first.
 *
 *  `.vscode-test.mjs` runs each suite twice: once over the bundled wasm
 *  backend, once over the native binary, which it selects by seeding the
 *  MACHINE-scoped `nml.server.path` into the launch's own profile. If that
 *  seeding silently failed — a workspace-scoped setting, a leftover profile,
 *  a renamed setting — the "native" lane would run the wasm server and pass
 *  every assertion below it, and the gate would be green and empty. So the
 *  lane states which backend it believes it is testing, and the extension
 *  says which one it actually started. */
/** The backend this launch declared, for a suite TITLE.
 *
 *  The titles are the only thing a `just gate-ext-e2e` log shows, and they
 *  used to say "WASM neutral server" on all four launches — including the
 *  two the native pair runs. `assertConfiguredBackend` below still holds the
 *  launch to it; this only makes the log say which lane is speaking. */
export function configuredBackend(): string {
  return process.env.NML_TEST_EXPECT_BACKEND ?? "unconfigured";
}

export async function assertConfiguredBackend(): Promise<string> {
  const expected = process.env.NML_TEST_EXPECT_BACKEND;
  assert.ok(
    expected === "wasm" || expected === "native",
    `the launch must declare which backend it is testing (NML_TEST_EXPECT_BACKEND), got ${expected}`
  );
  const extension = vscode.extensions.getExtension<NmlExtensionApi>("nudge.nml-lang");
  assert.ok(extension, "the nml extension must be installed in the test instance");
  const api = await extension.activate();
  const label = api.getServerLabel();
  const configured = vscode.workspace.getConfiguration("nml").get<string>("server.path", "");

  // Both directions, because either alone is satisfiable by a lane that
  // quietly did nothing: the MECHANISM must be in place (the machine-scoped
  // override really reached this profile), and the RESULT must match the
  // launch's declared intent.
  if (expected === "native") {
    assert.ok(
      configured,
      "this launch declared the native backend, but no nml.server.path reached the profile — " +
        "the machine-scoped setting was not seeded into <user-data-dir>/User/settings.json"
    );
    assert.match(
      label,
      /nml\.server\.path/,
      `this launch declared the native backend (nml.server.path=${configured}); ` +
        `the extension reports "${label}"`
    );
  } else {
    assert.strictEqual(
      configured,
      "",
      `this launch declared the wasm backend, but the profile carries nml.server.path=${configured}`
    );
    assert.match(
      label,
      /wasm/,
      `this launch declared the wasm backend; the extension reports "${label}"`
    );
  }
  assertSupervisionMatchesOnPosix(expected);
  return label;
}

/** How the running server is OWNED, asserted in BOTH directions.
 *
 *  This test runs INSIDE the extension host, so `process.pid` is the process
 *  that owns the server — and on POSIX a supervised launch is one of its
 *  children, running `launchSupervisor.js` out of the extension's own
 *  directory.
 *
 *  native ⇒ a supervisor MUST be there. Asserting the label alone would be
 *  satisfied by a direct spawn: the label says which BINARY was chosen, not
 *  how it is owned, and "how it is owned" is the whole design. Proven by
 *  mutation — pointing `supervisorPath()` at a file that does not exist turns
 *  the native lanes RED here.
 *
 *  wasm ⇒ there must be NO supervisor, and that half is not redundant with
 *  the label. The label is the extension reporting its own resolution; the
 *  process table is the kernel reporting what actually runs. A resolution
 *  that chose the bundled backend and ALSO left a native child behind — a
 *  fallback that launched before the wasm server won, a session the
 *  reconciler failed to retire — reads as a perfectly good wasm lane by every
 *  other assertion in this file, while an unowned `nml-lsp` sits on the
 *  machine. One `pgrep`, two verdicts, no cell unstated.
 *
 *  On Windows there is no supervisor by design (a non-detached child is in
 *  libuv's job object with KILL_ON_JOB_CLOSE), so the check states that and
 *  stops. */
function assertSupervisionMatchesOnPosix(expected: "wasm" | "native"): void {
  if (process.platform === "win32") return;
  const children = childrenOfThisHost();
  if (expected === "native") {
    assert.match(
      children,
      /launchSupervisor\.js/,
      "the native lane did not launch its server through the supervisor; the extension " +
        `host's children are:\n${children}`
    );
    return;
  }
  assert.doesNotMatch(
    children,
    /launchSupervisor\.js/,
    "the wasm lane has a supervised server process: the bundled backend needs no " +
      `process of its own, so this one is unowned. The extension host's children are:\n${children}`
  );
}

/** The extension host's direct children, with their command lines. `pgrep`
 *  exits 1 when nothing matches — which is a legitimate answer here (the wasm
 *  lane expects no children at all), so it reads as the empty list; anything
 *  else is the tool failing and must not be read as "no children". */
function childrenOfThisHost(): string {
  try {
    return execFileSync("pgrep", ["-P", String(process.pid), "-fl"], {
      encoding: "utf8",
    }).trim();
  } catch (err) {
    const status = (err as { status?: number | null }).status;
    if (status === 1) return "";
    throw err;
  }
}

/** How long ONE wait below may last. Generous: the first pull waits on
 *  extension activation and, in the wasm lanes, WASM instantiation. */
export const DIAGNOSTICS_WAIT_MS = 40_000;

/** The bound a suite has to ask mocha for, stated beside the waits it covers.
 *
 *  `.vscode-test.mjs` gives every launch 60 000 ms and that bounds the whole
 *  TEST, not each wait — while a test here can hold TWO (the cross-file heal
 *  waits for the diagnostic, edits the schema, and waits for it to go). The
 *  second wait could therefore never reach its own deadline: a slow host
 *  printed mocha's "Timeout of 60000ms exceeded", which names no file, no
 *  predicate and no diagnostics, instead of the message `waitForDiagnostics`
 *  was written to give. Each suite says how many waits its longest test
 *  holds; the constant is the margin for activation and the teardown's own
 *  edit. */
export function suiteTimeoutMs(waits: number): number {
  return waits * DIAGNOSTICS_WAIT_MS + 20_000;
}

/** A diagnostic's code as a plain string. The LSP client hands `code` back
 *  as a string, a number, or a `{ value, target }` when the server sent a
 *  description href — all three have to read the same here. */
export function diagnosticCode(d: vscode.Diagnostic): string {
  const code = d.code;
  if (code !== null && typeof code === "object") return String(code.value);
  return String(code);
}

/** Poll `getDiagnostics` (populated by the pull client applying server reports)
 *  until `predicate` holds or the timeout elapses. */
export async function waitForDiagnostics(
  uri: vscode.Uri,
  predicate: (d: readonly vscode.Diagnostic[]) => boolean,
  timeoutMs = DIAGNOSTICS_WAIT_MS
): Promise<readonly vscode.Diagnostic[]> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const diags = vscode.languages.getDiagnostics(uri);
    if (predicate(diags)) return diags;
    if (Date.now() > deadline) {
      throw new Error(
        `diagnostics predicate not met for ${uri.fsPath} within ${timeoutMs}ms; last: ${JSON.stringify(diags)}`
      );
    }
    await new Promise((r) => setTimeout(r, 250));
  }
}
