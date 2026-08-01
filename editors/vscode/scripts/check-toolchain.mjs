#!/usr/bin/env node
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { readToolchainRuntime, runToolchainCheck } from "./toolchainCheck.mjs";

const packageDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const runtime = readToolchainRuntime(packageDir);
const result = runToolchainCheck(packageDir, runtime);

if (!result.ok) {
  console.error(`check-toolchain: ${result.reason}`);
  process.exit(1);
}

console.log(`check-toolchain: ok (${result.summary})`);
