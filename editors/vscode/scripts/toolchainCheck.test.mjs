import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, it } from "node:test";
import { runToolchainCheck } from "./toolchainCheck.mjs";

const packageDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const checkScript = join(dirname(fileURLToPath(import.meta.url)), "check-toolchain.mjs");

describe("runToolchainCheck", () => {
  it("accepts the live workspace toolchain", () => {
    const result = runToolchainCheck(packageDir, {
      pnpmVersion: "11.16.0",
      nodeVersion: process.versions.node,
    });
    assert.equal(result.ok, true);
  });

  it("rejects pnpm outside engines.pnpm", () => {
    const result = runToolchainCheck(packageDir, {
      pnpmVersion: "10.0.0",
      nodeVersion: "22.0.0",
    });
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.match(result.reason, /engines\.pnpm|below engines\.pnpm|packageManager/);
  });

  it("rejects pnpm that does not match packageManager pin", () => {
    const result = runToolchainCheck(packageDir, {
      pnpmVersion: "11.15.0",
      nodeVersion: "22.0.0",
    });
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.match(result.reason, /packageManager/);
  });

  it("rejects node below engines.node", () => {
    const result = runToolchainCheck(packageDir, {
      pnpmVersion: "11.16.0",
      nodeVersion: "20.0.0",
    });
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.match(result.reason, /engines\.node/);
  });
});

describe("check-toolchain CLI", () => {
  it("exits 0 on the live workspace", () => {
    const result = spawnSync(process.execPath, [checkScript], {
      cwd: packageDir,
      encoding: "utf8",
    });
    assert.equal(result.status, 0, result.stderr || result.stdout);
    assert.match(result.stdout, /check-toolchain: ok/);
  });

  it("exits 1 when runtime versions fail policy", () => {
    // GitHub Actions sets CI=true, which intentionally disables the test
    // runtime override — unset it so this negative-path test can inject
    // bad versions without fighting the real CI toolchain.
    const env = {
      ...process.env,
      NML_TOOLCHAIN_TEST_RUNTIME: JSON.stringify({
        pnpmVersion: "10.0.0",
        nodeVersion: "22.0.0",
      }),
    };
    delete env.CI;
    const result = spawnSync(process.execPath, [checkScript], {
      cwd: packageDir,
      encoding: "utf8",
      env,
    });
    assert.equal(result.status, 1, result.stdout || result.stderr);
    assert.match(result.stderr, /check-toolchain:/);
  });

  it("ignores NML_TOOLCHAIN_TEST_RUNTIME under CI", () => {
    const result = spawnSync(process.execPath, [checkScript], {
      cwd: packageDir,
      encoding: "utf8",
      env: {
        ...process.env,
        CI: "true",
        NML_TOOLCHAIN_TEST_RUNTIME: JSON.stringify({
          pnpmVersion: "10.0.0",
          nodeVersion: "20.0.0",
        }),
      },
    });
    assert.equal(result.status, 0, result.stderr || result.stdout);
  });
});
