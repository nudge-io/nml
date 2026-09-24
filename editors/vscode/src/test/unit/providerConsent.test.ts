// MUST come first: routes `require("vscode")` to the stub before any module
// that (transitively) imports the real extension-host API is loaded.
import "../support/installVscodeStub";

import * as assert from "node:assert";
import { promises as fsp } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { ExtensionContext, Memento } from "vscode";
import {
  MAX_PROJECT_CONFIG_BYTES,
  clearSessionStandDowns,
  resolveServer,
} from "../../providerDiscovery";
import type { NmlLogs } from "../../logging";
import { LaunchSandbox, launchSandbox } from "../../pathSecurity";
import { NML_SERVER_NAME } from "../../providerTrust";
import {
  FileType,
  Uri,
  configurationValues,
  informationMessageAnswers,
  informationMessageHook,
  resetStubRecords,
  shownInformationMessages,
  statOverrides,
  workspace,
  workspaceFiles,
} from "../support/vscodeStub";

// ─────────────────────────────────────────────────────────────────────────
// The consent ladder, end to end, over a REAL filesystem: a real `PATH`
// directory with a real executable in it, a real `nml-project.nml`, and the
// real `fs.stat` the directory verdict is taken from.
//
// The property under test is a TIME one. Every check the extension can make
// before running a program is made before it runs — but one of them is made
// before a MODAL, and a modal is human time. What was true when the prompt
// went up need not be true when the operator answers it.
// ─────────────────────────────────────────────────────────────────────────

const PROJECT = `project Demo:
    provider:
        tool = "nudge"
`;

// Minted, never hand-written: `LaunchSandbox.cwd` is branded so that the
// only sandbox a resolution can carry is one this function made.
const SANDBOX: LaunchSandbox = launchSandbox("/fake/private-cwd");

/** The modal's accept button, as `providerConsentPrompt` spells it. */
const ACCEPT = 'Use "nudge"';

const logs: NmlLogs = {
  client: undefined as never,
  trace: undefined as never,
  info: (m) => logged.push(m),
  warn: (m) => logged.push(m),
  error: (m) => logged.push(m),
  showClient: () => undefined,
  showTrace: () => undefined,
};
let logged: string[] = [];

/** A workspace-state Memento over a Map. */
function memento(): Memento {
  const values = new Map<string, unknown>();
  return {
    keys: () => [...values.keys()],
    get: <T>(key: string, fallback?: T): T | undefined =>
      (values.has(key) ? values.get(key) : fallback) as T | undefined,
    update: (key: string, value: unknown): Promise<void> => {
      if (value === undefined) values.delete(key);
      else values.set(key, value);
      return Promise.resolve();
    },
  } as unknown as Memento;
}

suite("providerConsent/the directory verdict is taken again after the modal", () => {
  let root = "";
  let bin = "";
  let project = "";
  let savedPath: string | undefined;
  let context: ExtensionContext;

  setup(async function () {
    if (process.platform === "win32") {
      this.skip();
      return;
    }
    resetStubRecords();
    clearSessionStandDowns();
    logged = [];
    root = await fsp.mkdtemp(path.join(os.tmpdir(), "nml-consent-"));
    bin = path.join(root, "bin");
    project = path.join(root, "ws");
    await fsp.mkdir(bin, { mode: 0o755 });
    await fsp.mkdir(project, { mode: 0o755 });
    await fsp.chmod(bin, 0o755);
    await fsp.writeFile(path.join(bin, "nudge"), "#!/bin/sh\n", { mode: 0o755 });
    savedPath = process.env.PATH;
    process.env.PATH = bin;

    const folder = Uri.file(project);
    workspace.workspaceFolders = [{ name: "ws", uri: folder }];
    workspace.isTrusted = true;
    workspaceFiles.set(Uri.joinPath(folder, "nml-project.nml").toString(), PROJECT);
    context = {
      workspaceState: memento(),
      extensionUri: Uri.file(path.join(root, "ext")),
    } as unknown as ExtensionContext;
  });

  teardown(async () => {
    process.env.PATH = savedPath;
    if (root) await fsp.rm(root, { recursive: true, force: true });
    root = "";
    resetStubRecords();
    clearSessionStandDowns();
  });

  test("a directory that is safe when the prompt goes up, and safe when it is answered, runs", async () => {
    informationMessageAnswers.push(ACCEPT);
    const resolved = await resolveServer(context, logs, SANDBOX);
    assert.strictEqual(shownInformationMessages.length, 1, "the operator was asked");
    assert.ok(resolved.kind === "process");
    assert.strictEqual(resolved.command, path.join(bin, "nudge"));
    assert.strictEqual(resolved.label, "nudge (in-binary)");
  });

  test("a directory made WORLD-WRITABLE while the modal is up is refused, consent or not", async () => {
    // The whole window this closes: the verdict was taken before the prompt,
    // the operator took a minute to read it, and by the time they clicked
    // "Use nudge" any local user could replace the program it names.
    informationMessageAnswers.push(ACCEPT);
    informationMessageHook.run = (): void => {
      // eslint-disable-next-line no-sync -- the stub's hook is synchronous
      require("node:fs").chmodSync(bin, 0o777);
    };
    const resolved = await resolveServer(context, logs, SANDBOX);
    assert.strictEqual(shownInformationMessages.length, 1, "the operator was still asked");
    assert.notStrictEqual(
      resolved.label,
      "nudge (in-binary)",
      "the editor ran a program out of a directory that went world-writable during the prompt"
    );
    assert.ok(
      logged.some((line) => line.includes("any user on this machine can replace programs")),
      `the refusal was not explained; log was:\n${logged.join("\n")}`
    );
  });

  test("a declared tool that is not on PATH is SAID, not silently dropped", async () => {
    // Driven through the real ladder over a real filesystem: the project
    // declares "nudge" and PATH holds nothing of that name. Before this, the
    // editor returned the built-in server and wrote not one word anywhere.
    await fsp.rm(path.join(bin, "nudge"));
    const resolved = await resolveServer(context, logs, SANDBOX);
    assert.deepStrictEqual(shownInformationMessages, [], "there is nothing to ask about");
    assert.notStrictEqual(resolved.label, "nudge (in-binary)");
    assert.ok(
      logged.some((line) => line.includes('no program called "nudge" is on PATH')),
      `the skipped provider left no trace; log was:\n${logged.join("\n")}`
    );
  });

  test("two folders naming two tools is reported, not swallowed", async () => {
    // "nothing was declared" and "two folders declared different things" were
    // the same `undefined` to the ladder and are opposite facts to the
    // operator: the second is a project that asked for something and did not
    // get it.
    const second = Uri.file(path.join(root, "ws2"));
    workspace.workspaceFolders = [
      ...(workspace.workspaceFolders ?? []),
      { name: "ws2", uri: second },
    ];
    workspaceFiles.set(
      Uri.joinPath(second, "nml-project.nml").toString(),
      PROJECT.replace('"nudge"', '"other"')
    );
    const resolved = await resolveServer(context, logs, SANDBOX);
    assert.deepStrictEqual(shownInformationMessages, [], "there is no single answer to ask about");
    assert.notStrictEqual(resolved.label, "nudge (in-binary)");
    assert.ok(
      logged.some((line) => line.includes("declare different language servers")),
      `the disagreement left no trace; log was:\n${logged.join("\n")}`
    );
  });

  test("an untrusted workspace says which tool it did not run", async () => {
    workspace.isTrusted = false;
    try {
      const resolved = await resolveServer(context, logs, SANDBOX);
      assert.deepStrictEqual(shownInformationMessages, [], "an untrusted workspace is never asked");
      assert.notStrictEqual(resolved.label, "nudge (in-binary)");
      assert.ok(
        logged.some((line) => line.includes("this workspace is not trusted")),
        `the trust refusal left no trace; log was:\n${logged.join("\n")}`
      );
    } finally {
      workspace.isTrusted = true;
    }
  });

  test("a directory that was ALREADY world-writable is refused before any prompt", () => {
    // The pre-modal half of the same rule, so this file proves both ends.
    return fsp.chmod(bin, 0o777).then(async () => {
      const resolved = await resolveServer(context, logs, SANDBOX);
      assert.deepStrictEqual(shownInformationMessages, [], "it must not even ask");
      assert.notStrictEqual(resolved.label, "nudge (in-binary)");
    });
  });

  // ── what each resolution SAYS it is ──────────────────────────────────
  //
  // `ServerOrigin` is what `startFailureMessage` dispatches on, so a
  // resolution that stamps the wrong one hands the operator another
  // server's remedy — "clear nml.server.path" for a program the PROJECT
  // chose, say. The remedies themselves are pinned over literals in
  // clientManager.test.ts; these three pin the stamping, through the real
  // resolution paths, so the two halves cannot drift apart. Each also
  // pins the invariant that ties them: a handshake contract exists on the
  // provider resolution and on no other.

  test("a provider the operator approves is stamped `provider`, with its handshake", async () => {
    informationMessageAnswers.push(ACCEPT);
    const resolved = await resolveServer(context, logs, SANDBOX);
    assert.ok(resolved.kind === "process");
    assert.strictEqual(resolved.origin, "provider");
    assert.strictEqual(resolved.identity?.expect, NML_SERVER_NAME);
  });

  test("the native fallback is stamped `default`, and is held to no handshake", async () => {
    // Declined, so the ladder falls through to the neutral server; this
    // build bundles no wasm, so the native default is what it reaches.
    informationMessageAnswers.push(undefined);
    const resolved = await resolveServer(context, logs, SANDBOX);
    assert.ok(resolved.kind === "process");
    assert.strictEqual(resolved.origin, "default");
    assert.strictEqual(resolved.identity, undefined, "a binary the operator chose themselves");
  });

  test("an accepted nml.server.path is stamped `setting`", async () => {
    const chosen = path.join(bin, "my-nml-lsp");
    await fsp.writeFile(chosen, "#!/bin/sh\n", { mode: 0o755 });
    configurationValues.set("nml.server.path", chosen);
    informationMessageAnswers.push(undefined);
    const resolved = await resolveServer(context, logs, SANDBOX);
    assert.ok(resolved.kind === "process");
    assert.strictEqual(resolved.command, chosen);
    assert.strictEqual(resolved.origin, "setting");
    assert.strictEqual(resolved.identity, undefined);
  });

  // ───────────────────────────────────────────────────────────────────
  // The declaring file is a REPOSITORY's, and the extension reads it
  // itself — in the extension host, before the language server exists,
  // and before Workspace Trust is consulted (an untrusted workspace is
  // still told which tool it declared). The kernel reads the same file
  // under a 256 KiB bound and refuses anything that is not a regular
  // file; the extension read it whole, with neither check, so a
  // `nml-project.nml` that is a symlink to `/dev/zero` was 7.5 GB of the
  // extension host's heap in 8 seconds (measured on this platform's
  // Node) and a FIFO blocked forever.

  test("a nml-project.nml past the kernel's bound is not read, and the refusal is said", async () => {
    const uri = Uri.joinPath(Uri.file(project), "nml-project.nml");
    statOverrides.set(uri.toString(), { size: MAX_PROJECT_CONFIG_BYTES + 1 });
    const resolved = await resolveServer(context, logs, SANDBOX);
    assert.deepStrictEqual(shownInformationMessages, [], "nothing was declared, so nothing is asked");
    assert.notStrictEqual(resolved.label, "nudge (in-binary)");
    assert.ok(
      logged.some((line) => line.includes("Not reading nml-project.nml") && line.includes("larger than")),
      `the refusal was not explained; log was:\n${logged.join("\n")}`
    );
  });

  test("a nml-project.nml that is a FIFO, a device or a directory is not read", async () => {
    const uri = Uri.joinPath(Uri.file(project), "nml-project.nml");
    // `FileType.Unknown` is what the extension host reports for a FIFO,
    // a socket and a character device — the shapes whose READ never
    // returns.
    statOverrides.set(uri.toString(), { type: FileType.Unknown });
    const resolved = await resolveServer(context, logs, SANDBOX);
    assert.notStrictEqual(resolved.label, "nudge (in-binary)");
    assert.ok(
      logged.some((line) => line.includes("Not reading nml-project.nml") && line.includes("not a regular file")),
      `the refusal was not explained; log was:\n${logged.join("\n")}`
    );
  });

  test("a file that GREW between the stat and the read is refused on what arrived", async () => {
    // The stat and the read are two calls, and a repository is what
    // writes the file between them: a `stat` that says 10 bytes and a
    // `readFile` that returns a gigabyte is the shape the second check
    // exists for, and it is the only one a size check taken ONCE
    // cannot see.
    const uri = Uri.joinPath(Uri.file(project), "nml-project.nml");
    workspaceFiles.set(uri.toString(), PROJECT + "x".repeat(MAX_PROJECT_CONFIG_BYTES));
    statOverrides.set(uri.toString(), { size: 10 });
    const resolved = await resolveServer(context, logs, SANDBOX);
    assert.deepStrictEqual(shownInformationMessages, [], "nothing was declared, so nothing is asked");
    assert.notStrictEqual(resolved.label, "nudge (in-binary)");
    assert.ok(
      logged.some((line) => line.includes("Not reading nml-project.nml") && line.includes("larger than")),
      `the refusal was not explained; log was:\n${logged.join("\n")}`
    );
  });

  test("a symlinked nml-project.nml of a normal size is still read", async () => {
    // The bit set, not equality: a symlink to a regular file is
    // `File | SymbolicLink`, and refusing it would break every
    // repository that keeps its project config behind a link.
    const uri = Uri.joinPath(Uri.file(project), "nml-project.nml");
    statOverrides.set(uri.toString(), { type: FileType.File | FileType.SymbolicLink });
    informationMessageAnswers.push(ACCEPT);
    const resolved = await resolveServer(context, logs, SANDBOX);
    assert.strictEqual(shownInformationMessages.length, 1, "the declaration was read");
    assert.ok(resolved.kind === "process");
    assert.strictEqual(resolved.label, "nudge (in-binary)");
  });
});
