import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, rmSync } from "node:fs";
import { resolve } from "node:path";

/**
 * Run a command in the extension package directory; exit non-zero on failure.
 * @param {string} packageDir
 * @param {string} cmd
 * @param {string[]} args
 * @param {{ shell?: boolean }} [opts]
 */
export function runInPackage(packageDir, cmd, args, opts = {}) {
  const result = spawnSync(cmd, args, {
    cwd: packageDir,
    stdio: "inherit",
    shell: opts.shell ?? false,
  });
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}

/** @param {string} packageDir */
export function pnpmExec(packageDir, ...args) {
  runInPackage(packageDir, "pnpm", ["exec", ...args]);
}

/** @param {string} packageDir */
export function bundleWasm(packageDir) {
  const wasmPath = resolve(
    packageDir,
    "../../target/wasm32-wasip1/release/nml-lsp.wasm"
  );
  if (!existsSync(wasmPath)) {
    console.error(
      `bundle:wasm: missing ${wasmPath}\n` +
        "  Run: cargo build -p nml-lsp --target wasm32-wasip1 --release\n" +
        "  Or:  just gate-wasm"
    );
    process.exit(1);
  }
  const serverDir = resolve(packageDir, "server");
  mkdirSync(serverDir, { recursive: true });
  copyFileSync(wasmPath, resolve(serverDir, "nml-lsp.wasm"));
}

/** The name of the launch supervisor, in `src/` and beside every build output. */
export const SUPERVISOR_FILE = "launchSupervisor.js";

/**
 * Put the launch supervisor beside the code that runs it.
 *
 * `processLaunch.ts` resolves it as `join(__dirname, SUPERVISOR_FILE)`, and
 * there are two `__dirname`s: `dist/` for the bundled extension the editor
 * loads, `out/` for the tsc output the real-process tests drive. It is a plain
 * CommonJS file with no imports beyond `child_process` and `net`, so it is
 * COPIED rather than bundled — esbuild would rewrite it into a module the
 * Electron helper cannot run as a script, and the whole point is that
 * `process.execPath <file>` works with nothing else present.
 *
 * @param {string} packageDir
 * @param {string} outDir directory name under packageDir
 */
export function copySupervisor(packageDir, outDir) {
  const source = resolve(packageDir, "src", SUPERVISOR_FILE);
  if (!existsSync(source)) {
    console.error(`copySupervisor: missing ${source}`);
    process.exit(1);
  }
  const target = resolve(packageDir, outDir);
  mkdirSync(target, { recursive: true });
  copyFileSync(source, resolve(target, SUPERVISOR_FILE));
}

/** @param {string} packageDir */
export function bundleJs(packageDir) {
  // From scratch. A file left behind by an EARLIER build is a file the VSIX
  // ships and nothing re-derives — so a packaging check over a directory that
  // is never cleaned passes on yesterday's output, which is exactly what it
  // exists to catch. (Proven: with `dist/` reused, deleting the supervisor
  // copy below left `verifyPackage.mjs` green.)
  rmSync(resolve(packageDir, "dist"), { recursive: true, force: true });
  pnpmExec(
    packageDir,
    "esbuild",
    "src/extension.ts",
    "--bundle",
    "--outfile=dist/extension.js",
    "--external:vscode",
    "--format=cjs",
    "--platform=node",
    "--minify",
    "--sourcemap"
  );
  copySupervisor(packageDir, "dist");
}

/** @param {string} packageDir */
export function compileTsc(packageDir) {
  pnpmExec(packageDir, "tsc", "-b", "tsconfig.json", "tsconfig.test.json");
  copySupervisor(packageDir, "out");
}
