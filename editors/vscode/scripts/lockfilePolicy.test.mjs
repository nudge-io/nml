import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, it } from "node:test";
import { parse as parseYaml } from "yaml";
import {
  normalizeLockfileVersion,
  readLockfileTypesVscodeStrict,
  resolveLockfileTypesVscode,
} from "./lockfilePolicy.mjs";
import { resolveWorkspaceContext } from "./workspaceRoot.mjs";
import { validatePackageManifest } from "./vscodeEnginePolicy.mjs";

const packageDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const fixturesDir = join(dirname(fileURLToPath(import.meta.url)), "fixtures");
const fixturePath = join(fixturesDir, "pnpm-lock.fixture.yaml");
const noTypesFixturePath = join(fixturesDir, "pnpm-lock.no-types.yaml");

describe("normalizeLockfileVersion", () => {
  it("strips pnpm parenthetical suffixes", () => {
    assert.equal(normalizeLockfileVersion("1.91.0(typescript@7.0.2)"), "1.91.0");
    assert.equal(normalizeLockfileVersion("1.91.0"), "1.91.0");
  });

  it("rejects garbage", () => {
    assert.equal(normalizeLockfileVersion("vscode"), undefined);
  });
});

describe("resolveWorkspaceContext", () => {
  it("finds the workspace root and importer key for editors/vscode", () => {
    const ctx = resolveWorkspaceContext(packageDir);
    assert.equal(ctx.importerKey, "editors/vscode");
    assert.ok(ctx.workspaceRoot.endsWith("/nml") || ctx.workspaceRoot.endsWith("\\nml"));
  });

  it("throws when no pnpm-workspace.yaml exists above the directory", () => {
    const orphan = mkdtempSync(join(tmpdir(), "nml-no-workspace-"));
    assert.throws(
      () => resolveWorkspaceContext(orphan),
      /no pnpm-workspace\.yaml found/
    );
  });
});

describe("resolveLockfileTypesVscode", () => {
  it("reads @types/vscode from the fixture lockfile", () => {
    const lock = parseYaml(readFileSync(fixturePath, "utf8"));
    assert.equal(resolveLockfileTypesVscode(lock, "editors/vscode"), "1.91.0");
  });

  it("throws when the importer has no @types/vscode entry", () => {
    const lock = parseYaml(readFileSync(noTypesFixturePath, "utf8"));
    assert.throws(
      () => resolveLockfileTypesVscode(lock, "editors/vscode"),
      /has no @types\/vscode devDependency/
    );
  });
});

describe("readLockfileTypesVscodeStrict", () => {
  it("reads @types/vscode from the live workspace lockfile", () => {
    assert.equal(readLockfileTypesVscodeStrict(packageDir), "1.91.0");
  });

  it("throws when the lockfile is missing", () => {
    const root = mkdtempSync(join(tmpdir(), "nml-no-lock-"));
    const ext = join(root, "editors", "vscode");
    mkdirSync(ext, { recursive: true });
    writeFileSync(join(root, "pnpm-workspace.yaml"), "packages:\n  - editors/vscode\n");
    assert.throws(
      () => readLockfileTypesVscodeStrict(ext),
      /missing .*pnpm-lock\.yaml/
    );
  });

  it("throws when the lockfile YAML is corrupt", () => {
    const root = mkdtempSync(join(tmpdir(), "nml-bad-lock-"));
    const ext = join(root, "editors", "vscode");
    mkdirSync(ext, { recursive: true });
    writeFileSync(join(root, "pnpm-workspace.yaml"), "packages:\n  - editors/vscode\n");
    writeFileSync(join(root, "pnpm-lock.yaml"), ":\n  bad: [unclosed");
    assert.throws(
      () => readLockfileTypesVscodeStrict(ext),
      /failed to parse pnpm-lock\.yaml/
    );
  });
});

describe("validatePackageManifest with live lockfile", () => {
  it("rejects lockfile drift above engine floor", () => {
    const drift = validatePackageManifest(
      {
        engines: { vscode: "^1.91.0" },
        devDependencies: { "@types/vscode": "1.91.0" },
      },
      "1.125.0"
    );
    assert.equal(drift.ok, false);
    if (drift.ok) return;
    assert.match(drift.reason, /pnpm-lock\.yaml/);
  });
});
