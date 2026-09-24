import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { needsWindowsCmdShell } from "./toolchain.mjs";

describe("needsWindowsCmdShell", () => {
  it("is only the Windows pnpm shim", () => {
    assert.equal(needsWindowsCmdShell("win32", "pnpm"), true);
    assert.equal(needsWindowsCmdShell("win32", "pnpm.cmd"), true);
    assert.equal(needsWindowsCmdShell("win32", "node.exe"), false);
    assert.equal(needsWindowsCmdShell("linux", "pnpm"), false);
    assert.equal(needsWindowsCmdShell("darwin", "pnpm"), false);
  });
});
