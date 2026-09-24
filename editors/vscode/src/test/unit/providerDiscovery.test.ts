// MUST come first: routes `require("vscode")` to the stub before any module
// that (transitively) imports the real extension-host API is loaded.
import "../support/installVscodeStub";

import * as assert from "node:assert";
import { promises as fsp } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { resolveOnPath } from "../../providerDiscovery";

suite("providerDiscovery/resolveOnPath", () => {
  let dir: string;
  let savedPath: string | undefined;
  let savedCwd: string;

  setup(async () => {
    dir = await fsp.mkdtemp(path.join(os.tmpdir(), "nml-resolve-on-path-"));
    await fsp.writeFile(path.join(dir, "nudge"), "#!/bin/sh\n", { mode: 0o755 });
    savedPath = process.env.PATH;
    savedCwd = process.cwd();
  });

  teardown(async () => {
    process.env.PATH = savedPath;
    process.chdir(savedCwd);
    await fsp.rm(dir, { recursive: true, force: true });
  });

  test("an absolute PATH entry resolves the tool", async function () {
    if (process.platform === "win32") this.skip();
    process.env.PATH = dir;
    assert.strictEqual(await resolveOnPath("nudge"), path.join(dir, "nudge"));
  });

  test("a relative PATH entry never resolves it, even from the directory that holds it", async function () {
    if (process.platform === "win32") this.skip();
    // `.` and an empty segment both mean the host's cwd — the spawn would
    // depend on where the extension host happens to run.
    process.chdir(dir);
    process.env.PATH = [".", "", "relative/bin"].join(path.delimiter);
    assert.strictEqual(await resolveOnPath("nudge"), undefined);
  });

  test("a DIRECTORY named like the tool does not end the search", async function () {
    if (process.platform === "win32") this.skip();
    // `access(X_OK)` succeeds on a directory — there the execute bit is the
    // search bit. A directory taken as the hit would be shown in the consent
    // prompt, approved, and then fail to spawn, while the real program
    // further along `PATH` was never reached.
    const shadow = await fsp.mkdtemp(path.join(os.tmpdir(), "nml-shadow-"));
    try {
      await fsp.mkdir(path.join(shadow, "nudge"), { mode: 0o755 });
      process.env.PATH = [shadow, dir].join(path.delimiter);
      assert.strictEqual(await resolveOnPath("nudge"), path.join(dir, "nudge"));
    } finally {
      await fsp.rm(shadow, { recursive: true, force: true });
    }
  });
});
