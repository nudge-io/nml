import * as assert from "node:assert";
import { promises as fsp } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import {
  evaluateNeutralServerPathOverride,
  expandHomePrefix,
  isPathInsideWorkspace,
  isPathInsideWorkspaceRoots,
} from "../../pathSecurity";

suite("pathSecurity/evaluateNeutralServerPathOverride", () => {
  const globalBin = path.join(os.tmpdir(), "nml-global-bin", "nml-lsp");

  test("rejects relative paths", async () => {
    const result = await evaluateNeutralServerPathOverride(
      "target/release/nml-lsp",
      []
    );
    assert.deepStrictEqual(result, { accepted: false, reason: "relative" });
  });

  test("rejects relative paths with leading ./", async () => {
    const result = await evaluateNeutralServerPathOverride("./nml-lsp", []);
    assert.deepStrictEqual(result, { accepted: false, reason: "relative" });
  });

  test("accepts absolute paths outside workspace", async () => {
    const result = await evaluateNeutralServerPathOverride(globalBin, []);
    assert.deepStrictEqual(result, { accepted: true, command: globalBin });
  });

  test("trims whitespace on absolute paths", async () => {
    const result = await evaluateNeutralServerPathOverride(`  ${globalBin}  `, []);
    assert.deepStrictEqual(result, { accepted: true, command: globalBin });
  });

  test("rejects absolute paths inside workspace roots", async () => {
    const root = path.join(os.tmpdir(), "nml-ws-absolute-test");
    const inside = path.join(root, "bin", "nml-lsp");
    const result = await evaluateNeutralServerPathOverride(inside, [root]);
    assert.deepStrictEqual(result, { accepted: false, reason: "inside-workspace" });
  });

  test("expands ~/ to the home directory and accepts", async () => {
    const result = await evaluateNeutralServerPathOverride("~/x", []);
    assert.deepStrictEqual(result, {
      accepted: true,
      command: path.join(os.homedir(), "x"),
    });
  });

  test("refuses ~otheruser paths as relative (no passwd lookups)", async () => {
    const result = await evaluateNeutralServerPathOverride("~otheruser/x", []);
    assert.deepStrictEqual(result, { accepted: false, reason: "relative" });
  });

  test("workspace containment fires on the expanded path", async () => {
    const root = path.join(os.homedir(), "nml-tilde-ws-test");
    const result = await evaluateNeutralServerPathOverride(
      "~/nml-tilde-ws-test/bin/nml-lsp",
      [root]
    );
    assert.deepStrictEqual(result, { accepted: false, reason: "inside-workspace" });
  });
});

suite("pathSecurity/expandHomePrefix", () => {
  const home = path.sep === "/" ? "/home/dev" : "C:\\Users\\dev";

  test("expands ~/ and bare ~", () => {
    assert.strictEqual(expandHomePrefix("~/x/y", home), path.join(home, "x", "y"));
    assert.strictEqual(expandHomePrefix("~", home), home);
  });

  test("leaves other-user and non-tilde paths untouched", () => {
    assert.strictEqual(expandHomePrefix("~other/x", home), "~other/x");
    assert.strictEqual(expandHomePrefix("plain/x", home), "plain/x");
  });

  test("no expansion when the home directory is missing or relative", () => {
    assert.strictEqual(expandHomePrefix("~/x", ""), "~/x");
    assert.strictEqual(expandHomePrefix("~/x", "relative/home"), "~/x");
  });
});

suite("pathSecurity/isPathInsideWorkspaceRoots", () => {
  const root = path.join(os.tmpdir(), "nml-ws-root");
  const nested = path.join(root, "pkg", "bin");

  test("detects binary inside workspace root", () => {
    assert.strictEqual(isPathInsideWorkspaceRoots(nested, [root]), true);
  });

  test("does not flag the workspace root itself", () => {
    assert.strictEqual(isPathInsideWorkspaceRoots(root, [root]), false);
  });

  test("does not flag paths outside workspace", () => {
    const outside = path.join(os.tmpdir(), "global-bin", "nml-lsp");
    assert.strictEqual(isPathInsideWorkspaceRoots(outside, [root]), false);
  });

  test("nested workspace roots use longest-specific match semantics", () => {
    const outer = path.join(os.tmpdir(), "outer");
    const inner = path.join(outer, "inner");
    const file = path.join(inner, "tool");
    assert.strictEqual(isPathInsideWorkspaceRoots(file, [outer, inner]), true);
  });
});

// Real-filesystem containment: the string check alone is defeated by
// case-variant spellings on case-insensitive filesystems (APFS, NTFS) and by
// symlinks. These probe an actual temp directory registered as a root.
suite("pathSecurity/isPathInsideWorkspace (filesystem identity)", () => {
  let root: string;

  suiteSetup(async () => {
    root = await fsp.mkdtemp(path.join(os.tmpdir(), "nmlcase-"));
    await fsp.mkdir(path.join(root, "bin"));
    await fsp.writeFile(path.join(root, "bin", "nml-lsp"), "#!/bin/sh\n");
  });

  suiteTeardown(async () => {
    await fsp.rm(root, { recursive: true, force: true });
  });

  /** The root's own spelling with its (always alphabetic) basename prefix
   *  case-flipped — resolves to the same directory only on a
   *  case-insensitive filesystem. */
  function caseVariantRoot(): string {
    return path.join(path.dirname(root), path.basename(root).toUpperCase());
  }

  test("case-variant spelling of a workspace-resident path is inside", async function () {
    // Probe the filesystem itself: if the case-flipped root does not stat,
    // the filesystem is case-sensitive and the bypass cannot exist.
    const variant = caseVariantRoot();
    try {
      await fsp.stat(variant);
    } catch {
      this.skip();
    }
    assert.strictEqual(
      await isPathInsideWorkspace(path.join(variant, "bin", "nml-lsp"), [root]),
      true
    );
    // A nonexistent leaf under the case-variant root is still judged inside:
    // the ancestor walk lands on the root's inode.
    assert.strictEqual(
      await isPathInsideWorkspace(path.join(variant, "bin", "ghost"), [root]),
      true
    );
  });

  test("the root itself is not inside, in any spelling", async () => {
    assert.strictEqual(await isPathInsideWorkspace(root, [root]), false);
    assert.strictEqual(
      await isPathInsideWorkspace(caseVariantRoot(), [root]),
      false
    );
  });

  test("a symlink outside the root pointing into it is inside", async function () {
    const outside = await fsp.mkdtemp(path.join(os.tmpdir(), "nmlsym-"));
    try {
      try {
        await fsp.symlink(
          path.join(root, "bin", "nml-lsp"),
          path.join(outside, "link-into-ws")
        );
      } catch {
        this.skip(); // no symlink privilege on this host
      }
      assert.strictEqual(
        await isPathInsideWorkspace(path.join(outside, "link-into-ws"), [root]),
        true
      );
    } finally {
      await fsp.rm(outside, { recursive: true, force: true });
    }
  });

  test("a genuinely outside sibling stays outside", async () => {
    assert.strictEqual(
      await isPathInsideWorkspace(path.join(os.tmpdir(), "elsewhere", "nml-lsp"), [
        root,
      ]),
      false
    );
  });
});
