import { Readable, Writable } from "stream";
import { ExtensionContext, Uri, window, workspace } from "vscode";
import { Wasm, WasmProcess } from "@vscode/wasm-wasi/v1";
import { StreamMessageReader, StreamMessageWriter } from "vscode-jsonrpc/node";
import { MessageTransports } from "vscode-languageclient/node";
import { NmlLogs } from "./logging";
import { evaluateNeutralServerPathOverride } from "./pathSecurity";
import {
  buildUriMapping,
  duplicateFolderNames,
  hostToWasi,
  isPanicShapedStderr,
  wasiToHost,
} from "./wasmBridge";


// ─────────────────────────────────────────────────────────────────────────
// RFC 0035 — neutral-server delivery. The provider path (`<tool> lsp`) is
// resolved in providerDiscovery.ts; this module owns the *neutral* server: the bundled
// WASM backend (universal, offline, WASI-sandboxed — the preferred VS Code
// delivery), with the native binary as the override/fallback.
//
// The WASM bridge deliberately uses the STABLE toolchain — `@vscode/wasm-wasi`
// (1.x) → Node streams → `vscode-jsonrpc` framing — NOT `@vscode/wasm-wasi-lsp`,
// which is pre-release and would drag the whole extension onto a pre-release
// `vscode-languageclient`. No compromise on dependency stability.
// ─────────────────────────────────────────────────────────────────────────

/** The native default install location. Computed lazily so a homeless
 *  environment (empty HOME/USERPROFILE) degrades to bare `nml-lsp` — spawned
 *  via PATH resolution — instead of the nonsense `/.cargo/bin/nml-lsp`. */
function defaultNativeCommand(logs: NmlLogs): string {
  const home = process.env.HOME || process.env.USERPROFILE || "";
  if (home) return `${home}/.cargo/bin/nml-lsp`;
  logs.warn(
    "No home directory in the environment; resolving `nml-lsp` via PATH. " +
      "Set nml.server.path to pick an exact binary."
  );
  return "nml-lsp";
}

export type NeutralServer =
  | { kind: "process"; command: string; args: string[]; label: string }
  | { kind: "wasm"; module: Uri; label: string };

/** Resolve the neutral server, in priority order:
 *  1. `nml.server.path` (machine-scoped user setting) — air-gapped / self-built.
 *  2. The bundled WASM backend, if present (shipped by the build's `bundle:wasm`).
 *  3. The native default path (`~/.cargo/bin/nml-lsp`).
 */
export async function resolveNeutralServer(
  ctx: ExtensionContext,
  logs: NmlLogs
): Promise<NeutralServer> {
  const roots = (workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath);
  const override = workspace.getConfiguration("nml").get<string>("server.path", "");
  if (override) {
    const evaluated = await evaluateNeutralServerPathOverride(override, roots);
    if (evaluated.accepted) {
      return {
        kind: "process",
        command: evaluated.command,
        args: [],
        label: "neutral (nml.server.path)",
      };
    }
    if (evaluated.reason === "relative") {
      logs.warn(
        `Refusing nml.server.path — relative paths are not allowed (${override}). ` +
          "Set an absolute path (e.g. ~/.cargo/bin/nml-lsp)."
      );
    } else {
      logs.warn(
        `Refusing nml.server.path inside the workspace (${override}). ` +
          "Use a global install, bundled WASM, or a binary outside the workspace."
      );
    }
  }
  const module = Uri.joinPath(ctx.extensionUri, "server", "nml-lsp.wasm");
  if (await exists(module)) {
    return { kind: "wasm", module, label: "neutral nml-lsp (wasm)" };
  }
  return {
    kind: "process",
    command: defaultNativeCommand(logs),
    args: [],
    label: "neutral nml-lsp",
  };
}

async function exists(uri: Uri): Promise<boolean> {
  try {
    await workspace.fs.stat(uri);
    return true;
  } catch {
    return false;
  }
}

/** Rewrite every URI on the wire between host paths and the guest's WASI
 *  mounts (see wasmBridge.ts for the mount-scheme contract). The language
 *  client otherwise sends host paths (`file:///Users/…/ws/app.nml`), which do
 *  not exist in the guest's fs — so the server's `std::fs` workspace reads
 *  (indexing, sibling `*.model.nml`/`*.package.nml`) find nothing. This is the
 *  stable-toolchain equivalent of what `@vscode/wasm-wasi-lsp` does. */
export function wasmUriConverters(): {
  code2Protocol: (uri: Uri) => string;
  protocol2Code: (value: string) => Uri;
} {
  const folders = (workspace.workspaceFolders ?? []).map((f) => ({
    name: f.name,
    uriString: f.uri.toString(),
  }));
  const dupes = duplicateFolderNames(folders);
  if (dupes.length > 0) {
    const message =
      `nml: workspace folders with duplicate names ${JSON.stringify(dupes)} ` +
      "collide in the WASI mount scheme (/workspaces/<name>); " +
      "diagnostics may attach to the wrong folder. Rename the folders " +
      "(File > Rename Workspace Folder) or use the native server " +
      "(nml.server.path).";
    // Both channels: console.error for harness visibility, the toast for
    // actual users. Runs once per wasm start, so no debounce is needed.
    console.error(message);
    void window.showWarningMessage(message);
  }
  const mapping = buildUriMapping(folders);
  return {
    code2Protocol: (uri: Uri): string => hostToWasi(mapping, uri.toString()),
    protocol2Code: (value: string): Uri => Uri.parse(wasiToHost(mapping, value)),
  };
}

/** A running WASM neutral server: the transports the language client speaks over,
 *  plus the process handle so the caller can [`WasmProcess.terminate`] it on
 *  stop/restart (a function `ServerOptions` does not own the process, so the
 *  client won't reap it for us). */
export interface WasmServer {
  transports: MessageTransports;
  process: WasmProcess;
}

/** Instantiate the bundled WASM neutral server and bridge its WASI stdio to the
 *  language client. The workspace is mounted (`mountPoints`) so the server's
 *  resolver (`std::fs`) sees committed schema; the WASI sandbox scopes it to
 *  exactly that. `@vscode/wasm-wasi`'s streams are adapted to Node streams so
 *  `vscode-jsonrpc`'s `StreamMessageReader`/`Writer` do the LSP framing — no
 *  hand-rolled framing, no pre-release dependency. `stderr` is forwarded to
 *  `log` so a server panic is visible rather than lost (and its pipe drained). */
export async function createWasmServer(module: Uri, log: NmlLogs): Promise<WasmServer> {
  const wasm = await Wasm.load();
  // Copy into a fresh (non-shared) ArrayBuffer so the bytes satisfy
  // `WebAssembly.compile`'s `BufferSource` regardless of the FS provider.
  const bits = new Uint8Array(await workspace.fs.readFile(module));
  const compiled = await WebAssembly.compile(bits);
  const proc = await wasm.createProcess("nml-lsp", compiled, {
    stdio: {
      in: { kind: "pipeIn" },
      out: { kind: "pipeOut" },
      err: { kind: "pipeOut" },
    },
    mountPoints: [{ kind: "workspaceFolder" }],
    // Panic messages already reach the log via the stderr drain below;
    // the backtrace turns "panicked in std" into a named caller.
    env: { RUST_BACKTRACE: "1" },
  });
  // Runs until stdin EOF or `terminate()`. Normal exit resolves; an
  // instantiation/trap before stdio is wired rejects — surface it to the log
  // rather than let it become an unhandledRejection in the extension host.
  proc.run().catch((err) => log.error(`nml-lsp wasm process error: ${err}`));

  const wasmOut = proc.stdout;
  const wasmIn = proc.stdin;
  if (!wasmOut || !wasmIn) {
    // Unreachable under wasm-wasi-core 1.0.2 (pipeIn/pipeOut always wire
    // stdio), but a host that doesn't must not leak the just-started process.
    await proc.terminate().catch(() => undefined);
    throw new Error("wasm process was created without piped stdio");
  }
  // Drain stderr to the log — otherwise a panic is both invisible and, on an
  // undrained pipe, a backpressure hazard. Panic-shaped lines ALSO go to
  // console.error: the OutputChannel has no read-back API, so in the E2E
  // harness a server abort would otherwise surface only as downstream
  // timeouts with the actual Rust panic message unrecoverable (exactly how
  // a CI failure shipped with no cause attached).
  proc.stderr?.onData((data) => {
    const text = new TextDecoder().decode(data).replace(/\n$/, "");
    if (text) log.error(text);
    if (isPanicShapedStderr(text)) {
      console.error(`nml-lsp stderr: ${text}`);
    }
  });

  const nodeReadable = new Readable({ read() {} });
  wasmOut.onData((data) => nodeReadable.push(Buffer.from(data)));

  const nodeWritable = new Writable({
    write(chunk, _encoding, callback) {
      wasmIn.write(new Uint8Array(chunk)).then(() => callback(), callback);
    },
  });

  return {
    transports: {
      reader: new StreamMessageReader(nodeReadable),
      writer: new StreamMessageWriter(nodeWritable),
    },
    process: proc,
  };
}
