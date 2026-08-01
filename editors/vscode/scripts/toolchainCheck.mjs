import { execSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { readLockfileTypesVscodeStrict } from "./lockfilePolicy.mjs";
import {
  satisfiesNodeEngine,
  satisfiesPnpmEngine,
  validatePackageManagerPin,
} from "./toolchainPolicy.mjs";
import { validatePackageManifest } from "./vscodeEnginePolicy.mjs";
import { resolveWorkspaceContext } from "./workspaceRoot.mjs";

/** @param {string} path */
function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

/**
 * Read the active Node/pnpm versions for policy checks.
 * Tests may inject `NML_TOOLCHAIN_TEST_RUNTIME` as JSON `{ pnpmVersion, nodeVersion }`.
 * @param {string} packageDir
 */
export function readToolchainRuntime(packageDir) {
  const override = process.env.NML_TOOLCHAIN_TEST_RUNTIME;
  if (override && process.env.CI !== "true") {
    return JSON.parse(override);
  }
  return {
    pnpmVersion: execSync("pnpm -v", { cwd: packageDir, encoding: "utf8" }).trim(),
    nodeVersion: process.versions.node,
  };
}

/**
 * Run the full extension toolchain policy gate.
 * @param {string} packageDir absolute path to editors/vscode
 * @param {{ pnpmVersion: string; nodeVersion: string }} [runtime]
 * @returns {{ ok: true; summary: string } | { ok: false; reason: string }}
 */
export function runToolchainCheck(packageDir, runtime = readToolchainRuntime(packageDir)) {
  const { workspaceRoot } = resolveWorkspaceContext(packageDir);
  const pkg = readJson(join(packageDir, "package.json"));
  const rootPkg = readJson(join(workspaceRoot, "package.json"));

  let lockVersion;
  try {
    lockVersion = readLockfileTypesVscodeStrict(packageDir);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ok: false, reason: message };
  }

  const manifest = validatePackageManifest(pkg, lockVersion);
  if (!manifest.ok) return manifest;

  const packageManager = rootPkg?.packageManager;
  if (!packageManager) {
    return {
      ok: false,
      reason:
        "root package.json must declare packageManager (e.g. pnpm@11.16.0) for Corepack pinning.",
    };
  }

  const pin = validatePackageManagerPin(packageManager, runtime.pnpmVersion);
  if (!pin.ok) return pin;

  const pnpmEngine = pkg?.engines?.pnpm;
  if (!pnpmEngine) {
    return { ok: false, reason: "package.json must declare engines.pnpm." };
  }
  const pnpmRange = satisfiesPnpmEngine(pnpmEngine, runtime.pnpmVersion);
  if (!pnpmRange.ok) return pnpmRange;

  const nodeEngine = pkg?.engines?.node;
  if (!nodeEngine) {
    return { ok: false, reason: "package.json must declare engines.node." };
  }
  const nodeRange = satisfiesNodeEngine(nodeEngine, runtime.nodeVersion);
  if (!nodeRange.ok) return nodeRange;

  return {
    ok: true,
    summary:
      `engines.vscode ${pkg.engines.vscode}, @types/vscode ${pkg.devDependencies["@types/vscode"]}, ` +
      `lockfile ${lockVersion}, node ${runtime.nodeVersion}, pnpm ${runtime.pnpmVersion}`,
  };
}
