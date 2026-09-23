import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, rmSync } from "node:fs";
import { resolve } from "node:path";

/**
 * `spawn("pnpm")` on Windows resolves the Corepack / npm shim `pnpm.cmd`.
 * CreateProcess cannot execute a `.cmd` (ENOENT, `status === null`), so a
 * caller that only checks `status` exits 1 and prints nothing — which is
 * what `verify:ci` did on `windows-latest` at `test:real`. Node's `shell`
 * option runs it through `cmd.exe /d /s /c`, and that option joins arguments
 * with spaces, so an argument that contains a space or a cmd metacharacter
 * must be refused rather than split.
 *
 * @param {NodeJS.Platform} platform
 * @param {string} cmd
 */
export function needsWindowsCmdShell(platform, cmd) {
  return platform === "win32" && (cmd === "pnpm" || cmd === "pnpm.cmd");
}

const CMD_META = /[\s"&|<>^%]/;

/**
 * Run a command in the extension package directory; exit non-zero on failure.
 * @param {string} packageDir
 * @param {string} cmd
 * @param {string[]} args
 * @param {{ shell?: boolean }} [opts]
 */
export function runInPackage(packageDir, cmd, args, opts = {}) {
  const shell = opts.shell ?? needsWindowsCmdShell(process.platform, cmd);
  if (shell && process.platform === "win32") {
    const unsafe = [cmd, ...args].find((arg) => CMD_META.test(arg));
    if (unsafe !== undefined) {
      console.error(
        `refusing to run ${cmd} through cmd.exe: ${JSON.stringify(unsafe)} ` +
          "contains a space or a cmd metacharacter, and Node joins shell arguments with spaces"
      );
      process.exit(1);
    }
  }
  const result = spawnSync(cmd, args, {
    cwd: packageDir,
    stdio: "inherit",
    shell,
  });
  if (result.error) {
    console.error(`failed to run ${cmd} ${args.join(" ")}: ${result.error.message}`);
    process.exit(1);
  }
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
  // The typescript bin is a JavaScript file. Running it with this Node
  // avoids a nested `pnpm exec` — the Windows failure `needsWindowsCmdShell`
  // exists for. `pnpm exec esbuild` stays, because that bin is a native
  // executable on Unix and a JS launcher on Windows.
  const tscBin = resolve(packageDir, "node_modules", "typescript", "bin", "tsc");
  if (!existsSync(tscBin)) {
    console.error(`compile: missing typescript at ${tscBin}\n  Run: pnpm install`);
    process.exit(1);
  }
  runInPackage(packageDir, process.execPath, [
    tscBin,
    "-b",
    "tsconfig.json",
    "tsconfig.test.json",
  ]);
  copySupervisor(packageDir, "out");
}
