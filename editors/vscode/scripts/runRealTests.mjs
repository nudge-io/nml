#!/usr/bin/env node
/** Real-process mocha suite — compile, discover tests, run with an explicit file list.
 *
 * `out/test/real/*.js` in package.json is not expanded on Windows shells, and
 * mocha's own globbing of that literal has been flaky there; listing `out/test/real`
 * and passing each `.test.js` path (forward slashes) matches `test:unit`'s intent
 * without relying on the shell. Timeout matches `SUITE_TIMEOUT_MS` in the suite. */
import { readdirSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { compileTsc, runInPackage } from "./toolchain.mjs";

const packageDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const SUITE_TIMEOUT_MS = 3 * 20_000 + 10_000;

compileTsc(packageDir);
const realDir = join(packageDir, "out", "test", "real");
const files = readdirSync(realDir)
  .filter((name) => name.endsWith(".test.js"))
  .map((name) => relative(packageDir, join(realDir, name)).replace(/\\/g, "/"));
if (files.length === 0) {
  console.error(`test:real: no compiled tests under ${realDir}`);
  process.exit(1);
}
runInPackage(packageDir, "pnpm", [
  "exec",
  "mocha",
  "--ui",
  "tdd",
  "--timeout",
  String(SUITE_TIMEOUT_MS),
  "--exit",
  ...files,
]);
