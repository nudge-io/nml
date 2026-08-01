import * as assert from "node:assert";
import {
  isValidToolName,
  parseProviderTool,
} from "../../contracts/providerProject";

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
