import * as assert from "node:assert";
import * as os from "node:os";
import * as path from "node:path";
import {
  evaluateNeutralServerPathOverride,
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
