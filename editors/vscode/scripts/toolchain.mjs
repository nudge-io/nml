import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync } from "node:fs";
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
        "  Or:  just build-lsp-wasm"
    );
    process.exit(1);
  }
  const serverDir = resolve(packageDir, "server");
  mkdirSync(serverDir, { recursive: true });
  copyFileSync(wasmPath, resolve(serverDir, "nml-lsp.wasm"));
}

/** @param {string} packageDir */
export function bundleJs(packageDir) {
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
}

/** @param {string} packageDir */
export function compileTsc(packageDir) {
  pnpmExec(packageDir, "tsc", "-b", "tsconfig.json", "tsconfig.test.json");
}
