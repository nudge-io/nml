import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  compareSemver,
  parseSemverTriple,
  validatePackageManifest,
  validateTypesLockfileAlignment,
  validateVscodeEnginePolicy,
} from "./vscodeEnginePolicy.mjs";

const basePkg = {
  engines: { vscode: "^1.91.0" },
  devDependencies: { "@types/vscode": "1.91.0" },
};

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

  it("rejects types below engine floor", () => {
    const result = validateVscodeEnginePolicy("^1.91.0", "1.90.0");
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.match(result.reason, /below the engines\.vscode floor/);
  });

  it("rejects types ahead of engine floor (Dependabot PR #8 case)", () => {
    const result = validateVscodeEnginePolicy("^1.91.0", "^1.125.0");
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.match(result.reason, /@types\/vscode/);
    assert.match(result.reason, /engines\.vscode/);
  });
});

describe("validateTypesLockfileAlignment", () => {
  it("accepts matching manifest and lockfile versions", () => {
    assert.deepEqual(validateTypesLockfileAlignment("1.91.0", "1.91.0"), { ok: true });
  });

  it("rejects manifest/lockfile semver mismatch with an actionable message", () => {
    const result = validateTypesLockfileAlignment("1.125.0", "1.124.0");
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.match(result.reason, /does not match pnpm-lock\.yaml/);
    assert.match(result.reason, /pnpm install/);
  });
});

describe("validatePackageManifest", () => {
  it("requires a lockfile version", () => {
    const result = validatePackageManifest(basePkg, "");
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.match(result.reason, /required/);
  });

  it("rejects lockfile drift above engine floor", () => {
    const result = validatePackageManifest(basePkg, "1.125.0");
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.match(result.reason, /pnpm-lock\.yaml @types\/vscode 1\.125\.0/);
    assert.match(result.reason, /newer than engines\.vscode/);
  });

  it("rejects stale lockfile below the coordinated API floor", () => {
    const drift = validatePackageManifest(
      {
        engines: { vscode: "^1.125.0" },
        devDependencies: { "@types/vscode": "1.125.0" },
      },
      "1.124.0"
    );
    assert.equal(drift.ok, false);
    if (drift.ok) return;
    assert.match(drift.reason, /below the engines\.vscode floor/);
  });

  it("rejects manifest/lockfile @types/vscode drift via lockfile policy", () => {
    const result = validatePackageManifest(basePkg, "1.90.0");
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.match(result.reason, /pnpm-lock\.yaml @types\/vscode 1\.90\.0/);
    assert.match(result.reason, /below the engines\.vscode floor/);
  });

  it("accepts aligned manifest and lockfile", () => {
    assert.deepEqual(validatePackageManifest(basePkg, "1.91.0"), { ok: true });
  });
});

describe("compareSemver", () => {
  it("orders patch levels", () => {
    assert.equal(compareSemver([1, 91, 1], [1, 91, 0]), 1);
    assert.equal(compareSemver([1, 91, 0], [1, 125, 0]), -1);
  });
});
