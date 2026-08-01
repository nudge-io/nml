#!/usr/bin/env node
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { validatePackageManifest } from "./vscodeEnginePolicy.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

function lockfileTypesVersion(lock) {
  const direct =
    lock.packages?.["node_modules/@types/vscode"]?.version ??
    lock.dependencies?.["@types/vscode"]?.version;
  return typeof direct === "string" ? direct : undefined;
}

const pkg = readJson(join(root, "package.json"));
let lockVersion;
try {
  lockVersion = lockfileTypesVersion(readJson(join(root, "package-lock.json")));
} catch {
  lockVersion = undefined;
}

const result = validatePackageManifest(pkg, lockVersion);
if (!result.ok) {
  console.error(`check-vscode-engine: ${result.reason}`);
  process.exit(1);
}

console.log(
  `check-vscode-engine: ok (engines.vscode ${pkg.engines.vscode}, @types/vscode ${pkg.devDependencies["@types/vscode"]}` +
    (lockVersion ? `, lockfile ${lockVersion}` : "") +
    ")"
);
