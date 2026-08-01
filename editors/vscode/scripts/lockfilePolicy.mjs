import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { parse as parseYaml } from "yaml";
import { resolveWorkspaceContext } from "./workspaceRoot.mjs";

/**
 * Strip pnpm lockfile version suffixes like "1.91.0(typescript@7.0.2)" → "1.91.0".
 * @param {string} raw
 */
export function normalizeLockfileVersion(raw) {
  if (typeof raw !== "string") return undefined;
  const m = raw.match(/^(\d+\.\d+\.\d+)/);
  return m ? m[1] : undefined;
}

/**
 * Resolve @types/vscode from a parsed pnpm lockfile object.
 * @param {unknown} lock parsed pnpm-lock.yaml
 * @param {string} importerKey e.g. "editors/vscode"
 * @returns {string}
 */
export function resolveLockfileTypesVscode(lock, importerKey) {
  const entry = lock?.importers?.[importerKey]?.devDependencies?.["@types/vscode"];
  if (!entry || typeof entry !== "object") {
    throw new Error(
      `pnpm-lock.yaml importer ${JSON.stringify(importerKey)} has no @types/vscode devDependency`
    );
  }

  const version = normalizeLockfileVersion(entry.version);
  if (!version) {
    throw new Error(
      `pnpm-lock.yaml @types/vscode version is not recognizable: ${JSON.stringify(entry.version)}`
    );
  }
  return version;
}

/**
 * Read @types/vscode from the workspace lockfile. Throws on missing/corrupt lockfile.
 * @param {string} packageDir absolute path to the workspace package
 * @returns {string} resolved semver triple
 */
export function readLockfileTypesVscodeStrict(packageDir) {
  const { workspaceRoot, importerKey } = resolveWorkspaceContext(packageDir);
  const lockPath = join(workspaceRoot, "pnpm-lock.yaml");
  if (!existsSync(lockPath)) {
    throw new Error(
      `missing ${lockPath} — run pnpm install from the repo root and commit the lockfile`
    );
  }

  let lock;
  try {
    lock = parseYaml(readFileSync(lockPath, "utf8"));
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    throw new Error(`failed to parse pnpm-lock.yaml: ${message}`);
  }

  return resolveLockfileTypesVscode(lock, importerKey);
}
