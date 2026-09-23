#!/usr/bin/env node
/** Real-process mocha suite — compile, discover tests, run with an explicit file list.
 *
 * `out/test/real/*.js` in package.json is not expanded on Windows shells, and
 * mocha's own globbing of that literal has been flaky there; listing `out/test/real`
 * and passing each `.test.js` path (forward slashes) matches `test:unit`'s intent
 * without relying on the shell. Timeout matches `SUITE_TIMEOUT_MS` in the suite. */
import { existsSync, readdirSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { compileTsc, runInPackage } from "./toolchain.mjs";

const packageDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const SUITE_TIMEOUT_MS = 3 * 20_000 + 10_000;

compileTsc(packageDir);

// mocha's bin is JavaScript. `node <bin>` is the same program `pnpm exec mocha`
// would run, without spawning the Windows `pnpm.cmd` shim (see
// `needsWindowsCmdShell`).
const mochaBin = join(packageDir, "node_modules", "mocha", "bin", "mocha.js");
if (!existsSync(mochaBin)) {
  console.error(`test:real: missing mocha at ${mochaBin} — run pnpm install`);
  process.exit(1);
}

const realDir = join(packageDir, "out", "test", "real");
let names;
try {
  names = readdirSync(realDir);
} catch (err) {
  const message = err instanceof Error ? err.message : String(err);
  console.error(`test:real: cannot read ${realDir}: ${message}`);
  process.exit(1);
}
const files = names
  .filter((name) => name.endsWith(".test.js"))
  .map((name) => relative(packageDir, join(realDir, name)).replace(/\\/g, "/"));
if (files.length === 0) {
  console.error(`test:real: no compiled tests under ${realDir}`);
  process.exit(1);
}
runInPackage(packageDir, process.execPath, [
  mochaBin,
  "--ui",
  "tdd",
  "--timeout",
  String(SUITE_TIMEOUT_MS),
  "--exit",
  ...files,
]);
