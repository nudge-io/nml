import * as assert from "node:assert";
import * as os from "node:os";
import * as path from "node:path";
import {
  isValidToolName,
  parseProviderTool,
} from "../../contracts/providerProject";
import { isPathInsideWorkspaceRoots } from "../../pathSecurity";

suite("contracts/providerProject", () => {
  test("parseProviderTool reads tool from provider block", () => {
    const text = `project App:
    provider:
        tool = "nudge"
`;
    assert.strictEqual(parseProviderTool(text), "nudge");
  });

  test("parseProviderTool ignores comments and blank lines", () => {
    const text = `// header
project App:
    provider:
        // pick one
        tool = "nudge"
`;
    assert.strictEqual(parseProviderTool(text), "nudge");
  });

  test("parseProviderTool stops at sibling blocks", () => {
    const text = `project App:
    provider:
        tool = "nudge"
    other:
        tool = "evil"
`;
    assert.strictEqual(parseProviderTool(text), "nudge");
  });

  test("parseProviderTool returns undefined when provider missing", () => {
    assert.strictEqual(parseProviderTool("project App:\n"), undefined);
  });

  test("isValidToolName rejects path-like names", () => {
    assert.strictEqual(isValidToolName("nudge"), true);
    assert.strictEqual(isValidToolName("../nudge"), false);
    assert.strictEqual(isValidToolName("Nudge"), false);
    assert.strictEqual(isValidToolName(""), false);
  });
});

suite("pathSecurity/isPathInsideWorkspaceRoots", () => {
  const root = path.join(os.tmpdir(), "nml-ws-root");
  const nested = path.join(root, "pkg", "bin");

  test("detects binary inside workspace root", () => {
    assert.strictEqual(
      isPathInsideWorkspaceRoots(nested, [root]),
      true
    );
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
