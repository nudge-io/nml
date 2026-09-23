import { existsSync, mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "@vscode/test-cli";

// End-to-end tests for the nml extension in a real (headless) VS Code, driven
// by @vscode/test-cli. These exercise the extension in the actual editor: real
// pull-diagnostics round-trips, real cross-file focus-heal, and — for the
// bundled WASM backend — the WASI URI mapping.
//
// FOUR launches (each config = its own VS Code instance / workspace / profile):
//   • single-root, wasm   — pull diagnostics + cross-file focus-heal.
//   • multi-root,  wasm   — per-folder `/workspaces/<name>` mount.
//   • single-root, native — the SAME assertions over the native server.
//   • multi-root,  native — the same, across two folders.
//
// The native pair runs only when NML_TEST_NATIVE=1, and then it is REQUIRED:
// a missing binary fails loudly instead of silently reducing the suite to the
// wasm half (`just gate-ext-e2e` builds it first and sets the variable). A
// gate that quietly skips half of itself is the vacuity this repository keeps
// paying for.
//
// Prerequisites (CI): the WASM server must be bundled first
//   cargo build -p nml-lsp --target wasm32-wasip1 --release --locked
//   pnpm run bundle:wasm && pnpm run compile
// The WASI host extension is auto-installed into each test instance below.

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "../..");

// ── the user-data directory, and the socket budget it has to fit in ──────
//
// VS Code puts its IPC sockets under `--user-data-dir`, and a unix-domain
// socket path is bounded by `sizeof(struct sockaddr_un.sun_path)`: MEASURED at
// 104 bytes on this platform, so 103 usable characters. The bound is the
// WHOLE path, so the directory's own length is a budget the test harness
// spends before VS Code has written a single byte.
//
// The repository-relative default (`editors/vscode/.vscode-test/user-data`) is
// not a fixed cost — it is "wherever the checkout happens to be". MEASURED:
// 86 characters in a developer's home checkout (17 left), and 181 characters
// in a CI-style scratch clone — over the whole limit before VS Code starts.
// That is why this harness never uses the default: it makes its own directory
// under the shortest temp root on the platform and ASSERTS the margin, so the
// failure is a clear message here rather than a mystery "ENAMETOOLONG"/"could
// not connect" deep inside Electron.
const SUN_PATH_MAX = 103;
// Reserve for what VS Code appends: `/<version>-main.sock` (18 characters at
// 1.138.0) and Chromium's `/SingletonSocket` (16), with room for both to grow.
const SOCKET_SUFFIX_RESERVE = 40;

function shortTempRoot() {
  if (process.platform !== "win32" && existsSync("/tmp")) {
    // `/tmp` is 4 characters; its realpath is 12 on macOS. `os.tmpdir()` is
    // the per-user `/var/folders/...` directory there — MEASURED at 48
    // characters, which is most of the budget spent on nothing.
    return "/tmp";
  }
  return os.tmpdir();
}

/** A fresh, empty VS Code profile for one launch.
 *
 *  Fresh per launch, and removed on exit: a profile carried over from a
 *  previous run brings its window state, its extension cache and — the one
 *  that actually bites — its `User/settings.json`, so a config that seeds a
 *  MACHINE-scoped setting would leak it into the launch that must not have
 *  it. Every run of every configuration starts from nothing. */
function userDataDir(label, settings) {
  const dir = mkdtempSync(path.join(shortTempRoot(), `nml-vsct-${label}-`));
  const measured = realpathSync(dir);
  const margin = SUN_PATH_MAX - measured.length - SOCKET_SUFFIX_RESERVE;
  if (margin < 0) {
    throw new Error(
      `the user-data directory leaves no socket budget: ${measured} is ` +
        `${measured.length} characters, and VS Code needs up to ` +
        `${SOCKET_SUFFIX_RESERVE} more inside a ${SUN_PATH_MAX}-character ` +
        "unix socket path. Set TMPDIR to a shorter directory."
    );
  }
  if (settings) {
    // `nml.server.path` is MACHINE-scoped (package.json), so a workspace
    // fixture's `.vscode/settings.json` cannot set it — the only place that
    // works is the profile's own User settings, which is exactly what a
    // private user-data directory gives us.
    mkdirSync(path.join(dir, "User"), { recursive: true });
    writeFileSync(path.join(dir, "User", "settings.json"), JSON.stringify(settings, null, 2));
  }
  process.on("exit", () => {
    rmSync(dir, { recursive: true, force: true });
  });
  return dir;
}

/** An EMPTY schema store for one launch.
 *
 *  The native server resolves its store from `NML_SCHEMA_STORE_DIR`, else
 *  from the machine's data directory — so without this the native lanes read
 *  whatever schema packages the person (or the runner) running them happens
 *  to have installed. MEASURED on a developer machine with one package in
 *  `~/Library/Application Support/nml/schema-packages`: the single-root
 *  native lane bound `server` to THAT package and reported NML2004 ("block
 *  keyword 'server' has no model definition"), i.e. the cross-file schema in
 *  the fixture was never read — while the wasm lane, which has no such
 *  store, reported the NML2008 type mismatch the suite is named for. Both
 *  lanes were green, because the assertion was "a diagnostic arrived".
 *
 *  The variable survives the extension's environment scrub on purpose (it is
 *  a tool's own configuration), which is exactly why it has to be SET here
 *  rather than left to the machine. */
function emptyStoreDir(label) {
  const dir = mkdtempSync(path.join(shortTempRoot(), `nml-vsct-store-${label}-`));
  process.on("exit", () => {
    rmSync(dir, { recursive: true, force: true });
  });
  return dir;
}

function nativeServerPath() {
  const binary = path.join(
    repoRoot,
    "target",
    "release",
    process.platform === "win32" ? "nml-lsp.exe" : "nml-lsp"
  );
  if (!existsSync(binary)) {
    throw new Error(
      `NML_TEST_NATIVE=1 but ${binary} does not exist. Build it first: ` +
        "`cargo build -p nml-lsp --release --locked` (or `just gate-ext-e2e`, " +
        "which does). Refusing to run a native lane with no native server."
    );
  }
  return binary;
}

const LAUNCHES = [
  { label: "single-root", files: "out/test/extension.test.js", workspaceFolder: "src/test/fixtures/ws" },
  { label: "multi-root", files: "out/test/multiroot.test.js", workspaceFolder: "src/test/fixtures/multi.code-workspace" },
];

function configure(launch, backend, settings) {
  const label = `${launch.label} (${backend})`;
  return {
    label,
    files: launch.files,
    workspaceFolder: launch.workspaceFolder,
    installExtensions: ["ms-vscode.wasm-wasi-core"],
    launchArgs: ["--user-data-dir", userDataDir(`${backend}-${launch.label}`, settings)],
    // The launch's INTENT, reaching the test process. It has to be stated
    // independently of the mechanism that implements it: a test that only
    // checks "the running server matches the setting I can see" is happy
    // when the setting was never written, which is exactly the way a
    // "native" lane silently becomes a second wasm run. Proven: mutating
    // the seeding away left the suite GREEN until this line existed.
    //
    // …and the schema store the server resolves against, so that the two
    // lanes run the SAME experiment rather than the same assertions over
    // different universes (see `emptyStoreDir`).
    env: {
      NML_TEST_EXPECT_BACKEND: backend,
      NML_SCHEMA_STORE_DIR: emptyStoreDir(`${backend}-${launch.label}`),
    },
    mocha: { ui: "tdd", timeout: 60_000 },
  };
}

const configs = LAUNCHES.map((launch) => configure(launch, "wasm", undefined));

if (process.env.NML_TEST_NATIVE === "1") {
  const settings = { "nml.server.path": nativeServerPath() };
  for (const launch of LAUNCHES) configs.push(configure(launch, "native", settings));
}

export default defineConfig(configs);
