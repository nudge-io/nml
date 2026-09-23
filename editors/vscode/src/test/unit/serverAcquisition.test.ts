// MUST come first: routes `require("vscode")` to the stub before any module
// that (transitively) imports the real extension-host API is loaded.
import "../support/installVscodeStub";

import * as assert from "node:assert";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { ExtensionContext } from "vscode";
import { launchSandbox, providerWorkingDir } from "../../pathSecurity";
import { privateWorkingDir } from "../../serverAcquisition";
import { processServer } from "../../serverResolution";
import type { NmlLogs } from "../../logging";
import { NML_SERVER_NAME } from "../../providerTrust";

suite("serverAcquisition/processServer", () => {
  test("every process resolution carries the sandbox it was minted with", () => {
    const sandbox = launchSandbox("/var/profile/nml/server-cwd");
    const server = processServer("/opt/bin/nudge", ["lsp"], "nudge (in-binary)", "provider", sandbox);
    assert.strictEqual(server.kind, "process");
    assert.strictEqual(server.command, "/opt/bin/nudge");
    assert.deepStrictEqual(server.args, ["lsp"]);
    assert.strictEqual(server.label, "nudge (in-binary)");
    assert.strictEqual(server.cwd, "/var/profile/nml/server-cwd");
    assert.strictEqual(server.env, sandbox.env);
    assert.ok(
      path.isAbsolute(server.cwd),
      "an absolute cwd, so the spawn cannot depend on the host's"
    );
    assert.strictEqual(server.identity, undefined, "no handshake unless asked for");
  });

  test("a resolution the project asked for carries the handshake contract", () => {
    const server = processServer(
      "/opt/bin/nudge",
      ["lsp"],
      "nudge (in-binary)",
      "provider",
      launchSandbox("/var/profile/nml/server-cwd"),
      {
        expect: NML_SERVER_NAME,
        tool: "nudge",
        repudiate: () => Promise.resolve(),
        standDown: () => Promise.resolve(),
      }
    );
    assert.strictEqual(server.identity?.expect, NML_SERVER_NAME);
    assert.strictEqual(server.identity?.tool, "nudge");
  });

  test("a missing private directory still never yields a workspace-relative cwd", () => {
    // The fallback when the profile cannot be written: the operator's home,
    // absolute, and never a workspace folder — the property r106 closed.
    const server = processServer("/opt/bin/nml-lsp", [], "neutral", "default", launchSandbox(undefined));
    assert.strictEqual(
      server.cwd,
      providerWorkingDir(os.homedir(), path.parse(process.cwd()).root)
    );
    assert.ok(path.isAbsolute(server.cwd));
  });

  test("a relative private directory is refused, not trusted", () => {
    const server = processServer("/opt/bin/nml-lsp", [], "neutral", "default", launchSandbox("relative/dir"));
    assert.ok(path.isAbsolute(server.cwd), `cwd must be absolute, got ${server.cwd}`);
    assert.notStrictEqual(server.cwd, "relative/dir");
  });
});

suite("pathSecurity/launchSandbox", () => {
  test("the sandbox removes the loader-injection variables it finds in the environment", () => {
    const saved = { ...process.env };
    try {
      process.env.LD_PRELOAD = "/tmp/evil.so";
      process.env.DYLD_INSERT_LIBRARIES = "/tmp/evil.dylib";
      process.env.NODE_OPTIONS = "--require /tmp/evil.js";
      process.env.NML_KEEP_ME = "1";
      const sandbox = launchSandbox("/var/profile/nml/server-cwd");
      // `undefined` is the value that REMOVES a name from the child's
      // environment; `""` would leave it present and empty. The client
      // overlays this on a full copy of process.env, so a name that is
      // merely absent here is inherited unchanged.
      assert.strictEqual(sandbox.env.LD_PRELOAD, undefined);
      assert.ok("LD_PRELOAD" in sandbox.env, "present as a key, so it is removed");
      assert.ok("DYLD_INSERT_LIBRARIES" in sandbox.env);
      assert.ok("NODE_OPTIONS" in sandbox.env);
      assert.ok(
        !("NML_KEEP_ME" in sandbox.env),
        "a tool's own configuration is not ours to delete"
      );
      assert.ok(!("PATH" in sandbox.env), "the tool still needs to find its own helpers");
    } finally {
      for (const key of Object.keys(process.env)) delete process.env[key];
      Object.assign(process.env, saved);
    }
  });
});

suite("serverAcquisition/privateWorkingDir", () => {
  function context(storage: string): ExtensionContext {
    return { globalStorageUri: { fsPath: storage } } as unknown as ExtensionContext;
  }
  const warnings: string[] = [];
  const logs = {
    warn: (m: string) => warnings.push(m),
    info: () => undefined,
    error: () => undefined,
  } as unknown as NmlLogs;

  test("the directory is made fresh, empty and 0700, parents included", async () => {
    const storage = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "nml-cwd-")), "profile");
    try {
      const dir = await privateWorkingDir(context(storage), logs);
      assert.ok(dir, "a directory");
      assert.ok(fs.lstatSync(dir).isDirectory());
      assert.deepStrictEqual(fs.readdirSync(dir), []);
      if (process.platform !== "win32") {
        assert.strictEqual(fs.lstatSync(dir).mode & 0o7777, 0o700);
      }
      // Remade: whatever a previous session left is gone.
      fs.writeFileSync(path.join(dir, "lsp"), "#!/bin/sh\nexit 7\n");
      const again = await privateWorkingDir(context(storage), logs);
      assert.deepStrictEqual(fs.readdirSync(again as string), []);
    } finally {
      fs.rmSync(path.dirname(storage), { recursive: true, force: true });
    }
  });

  test("a symlink racing into the path does not become the working directory", async function () {
    if (process.platform === "win32") return this.skip();
    // The TOCTOU the `rm`+`mkdir` pair leaves open: `mkdir` with
    // `recursive` is idempotent, so a link planted after the `rm` and
    // before the `mkdir` was ADOPTED — with whatever the target holds,
    // which is exactly the `lsp` script an empty cwd exists to starve.
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "nml-cwd-race-"));
    const storage = path.join(root, "profile");
    const target = path.join(root, "attacker");
    fs.mkdirSync(storage, { recursive: true });
    fs.mkdirSync(target, { recursive: true });
    fs.writeFileSync(path.join(target, "lsp"), "#!/bin/sh\necho pwned\n");
    fs.symlinkSync(target, path.join(storage, "server-cwd"));
    try {
      const dir = await privateWorkingDir(context(storage), logs);
      if (dir !== undefined) {
        assert.ok(!fs.lstatSync(dir).isSymbolicLink(), "never a link");
        assert.deepStrictEqual(fs.readdirSync(dir), [], "never the attacker's directory");
      }
      // Either way the attacker's files were not adopted and not chmodded.
      assert.deepStrictEqual(fs.readdirSync(target), ["lsp"]);
    } finally {
      fs.rmSync(root, { recursive: true, force: true });
    }
  });
});
