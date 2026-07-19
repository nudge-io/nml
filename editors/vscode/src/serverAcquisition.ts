import { Readable, Writable } from "stream";
import { ExtensionContext, OutputChannel, Uri, workspace } from "vscode";
import { Wasm, WasmProcess } from "@vscode/wasm-wasi/v1";
import { StreamMessageReader, StreamMessageWriter } from "vscode-jsonrpc/node";
import { MessageTransports } from "vscode-languageclient/node";


// ─────────────────────────────────────────────────────────────────────────
// RFC 0035 — neutral-server delivery. The provider path (`<tool> lsp`) is
// resolved in extension.ts; this module owns the *neutral* server: the bundled
// WASM backend (universal, offline, WASI-sandboxed — the preferred VS Code
// delivery), with the native binary as the override/fallback.
//
// The WASM bridge deliberately uses the STABLE toolchain — `@vscode/wasm-wasi`
// (1.x) → Node streams → `vscode-jsonrpc` framing — NOT `@vscode/wasm-wasi-lsp`,
// which is pre-release and would drag the whole extension onto a pre-release
// `vscode-languageclient`. No compromise on dependency stability.
// ─────────────────────────────────────────────────────────────────────────

const DEFAULT_NATIVE_PATH =
  (process.env.HOME || process.env.USERPROFILE || "") + "/.cargo/bin/nml-lsp";

export type NeutralServer =
  | { kind: "process"; command: string; args: string[]; label: string }
  | { kind: "wasm"; module: Uri; label: string };

/** Resolve the neutral server, in priority order:
 *  1. `nml.server.path` (machine-scoped user setting) — air-gapped / self-built.
 *  2. The bundled WASM backend, if present (shipped by the build's `bundle:wasm`).
 *  3. The native default path (`~/.cargo/bin/nml-lsp`).
 */
export async function resolveNeutralServer(ctx: ExtensionContext): Promise<NeutralServer> {
  const override = workspace.getConfiguration("nml").get<string>("server.path", "");
  if (override) {
    return { kind: "process", command: override, args: [], label: "neutral (nml.server.path)" };
  }
  const module = Uri.joinPath(ctx.extensionUri, "server", "nml-lsp.wasm");
  if (await exists(module)) {
    return { kind: "wasm", module, label: "neutral nml-lsp (wasm)" };
  }
  return { kind: "process", command: DEFAULT_NATIVE_PATH, args: [], label: "neutral nml-lsp" };
}

async function exists(uri: Uri): Promise<boolean> {
  try {
    await workspace.fs.stat(uri);
    return true;
  } catch {
    return false;
  }
}

/** Map between host file URIs and the WASM server's WASI filesystem namespace.
 *  `wasm-wasi-core`'s `mapWorkspaceFolder` mounts a lone workspace folder at
 *  `/workspace` and each folder of a multi-root workspace at
 *  `/workspaces/${folder.name}` (verified against its source) — so the mount
 *  segment is exactly `WorkspaceFolder.name`, which is what this reads: the two
 *  sides agree by construction, not by guess. The language client otherwise
 *  sends host paths (`file:///Users/…/ws/app.nml`), which do not exist in the
 *  guest's fs — so the server's `std::fs` workspace reads (indexing, sibling
 *  `*.model.nml`/`*.package.nml`) find nothing. These converters rewrite every
 *  URI on the wire so the server sees the mounted paths it can actually read,
 *  and the client sees host paths back. This is the stable-toolchain equivalent
 *  of what `@vscode/wasm-wasi-lsp` does. */
export function wasmUriConverters(): {
  code2Protocol: (uri: Uri) => string;
  protocol2Code: (value: string) => Uri;
} {
  const folders = workspace.workspaceFolders ?? [];
  const single = folders.length === 1;
  const mapping = folders
    .map((f) => ({
      hostPrefix: f.uri.toString().replace(/\/$/, ""),
      wasiPrefix: `file://${single ? "/workspace" : `/workspaces/${f.name}`}`,
    }))
    // Longest host prefix first: with nested workspace folders (`/a` and
    // `/a/b`) the most specific mount must win, not whichever comes first.
    .sort((a, b) => b.hostPrefix.length - a.hostPrefix.length);
  return {
    code2Protocol: (uri: Uri): string => {
      const s = uri.toString();
      for (const m of mapping) {
        if (s === m.hostPrefix || s.startsWith(`${m.hostPrefix}/`)) {
          return m.wasiPrefix + s.slice(m.hostPrefix.length);
        }
      }
      return s;
    },
    protocol2Code: (value: string): Uri => {
      for (const m of mapping) {
        if (value === m.wasiPrefix || value.startsWith(`${m.wasiPrefix}/`)) {
          return Uri.parse(m.hostPrefix + value.slice(m.wasiPrefix.length));
        }
      }
      return Uri.parse(value);
    },
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
export async function createWasmServer(module: Uri, log: OutputChannel): Promise<WasmServer> {
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
  });
  // Runs until stdin EOF or `terminate()`. Normal exit resolves; an
  // instantiation/trap before stdio is wired rejects — surface it to the log
  // rather than let it become an unhandledRejection in the extension host.
  proc.run().catch((err) => log.append(`nml-lsp wasm process error: ${err}\n`));

  const wasmOut = proc.stdout;
  const wasmIn = proc.stdin;
  if (!wasmOut || !wasmIn) {
    throw new Error("wasm process was created without piped stdio");
  }
  // Drain stderr to the log — otherwise a panic is both invisible and, on an
  // undrained pipe, a backpressure hazard.
  proc.stderr?.onData((data) => log.append(new TextDecoder().decode(data)));

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
