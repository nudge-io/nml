import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  compareSemver,
  parseSemverTriple,
  validatePackageManifest,
  validateVscodeEnginePolicy,
} from "./vscodeEnginePolicy.mjs";

describe("parseSemverTriple", () => {
  it("parses caret ranges and exact versions", () => {
    assert.deepEqual(parseSemverTriple("^1.91.0"), [1, 91, 0]);
    assert.deepEqual(parseSemverTriple("1.125.0"), [1, 125, 0]);
  });

  it("rejects garbage", () => {
    assert.equal(parseSemverTriple("vscode"), null);
  });
});

describe("validateVscodeEnginePolicy", () => {
  it("accepts types aligned with engine floor", () => {
    assert.deepEqual(
      validateVscodeEnginePolicy("^1.91.0", "1.91.0"),
      { ok: true }
    );
  });

  it("accepts types below engine floor", () => {
    assert.deepEqual(
      validateVscodeEnginePolicy("^1.91.0", "1.90.0"),
      { ok: true }
    );
  });

  it("rejects types ahead of engine floor (Dependabot PR #8 case)", () => {
    const result = validateVscodeEnginePolicy("^1.91.0", "^1.125.0");
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.match(result.reason, /@types\/vscode/);
    assert.match(result.reason, /engines\.vscode/);
  });
});

describe("validatePackageManifest", () => {
  it("rejects lockfile drift above engine floor", () => {
    const result = validatePackageManifest(
      {
        engines: { vscode: "^1.91.0" },
        devDependencies: { "@types/vscode": "1.91.0" },
      },
      "1.125.0"
    );
    assert.equal(result.ok, false);
  });
});

describe("compareSemver", () => {
  it("orders patch levels", () => {
    assert.equal(compareSemver([1, 91, 1], [1, 91, 0]), 1);
    assert.equal(compareSemver([1, 91, 0], [1, 125, 0]), -1);
  });
});
