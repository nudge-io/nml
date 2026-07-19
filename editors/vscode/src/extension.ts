import {
  commands,
  ExtensionContext,
  languages,
  OutputChannel,
  StatusBarAlignment,
  StatusBarItem,
  ThemeColor,
  Uri,
  window,
  workspace,
} from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";
import { constants as fsConstants, promises as fsp } from "fs";
import * as path from "path";
import { WasmProcess } from "@vscode/wasm-wasi/v1";
import {
  createWasmServer,
  NeutralServer,
  resolveNeutralServer,
  wasmUriConverters,
} from "./serverAcquisition";

// ─────────────────────────────────────────────────────────────────────────
// RFC 0035 — default-safe discovery + Workspace-Trust gate + status surface.
//
// The neutral `nml-lsp` binary is the safe floor for every workspace. A
// project may OPT IN (committed `nml-project.nml`, `provider: tool = "<name>"`)
// to its tool's own language server (`<tool> lsp`, e.g. `nudge lsp`) — but only
// in a TRUSTED workspace, only a user-global (PATH) binary, and only after a
// per-workspace prompt. Launching a workspace-resident binary is refused: it is
// the workspace-trust RCE class. Reading committed schema is always safe (nml
// is data-not-code), so the neutral server runs even in untrusted workspaces.
// ─────────────────────────────────────────────────────────────────────────

let client: LanguageClient | undefined;
/** The bundled WASM neutral server's process, when that backend is running.
 *  A function `ServerOptions` does not own the process, so the client won't reap
 *  it — we terminate it ourselves on stop/restart to avoid leaking one. */
let wasmProcess: WasmProcess | undefined;
let status: StatusBarItem | undefined;
/** Human label of the server actually launched, for the status surface. */
let serverLabel = "";
/** One output channel for the extension's lifetime. Created lazily and owned by
 *  `context.subscriptions`; a `LanguageClient` given a channel it did not create
 *  never disposes it, so creating one per start would leak a duplicate "NML
 *  Language Server" channel on every restart/trust-grant. */
let outputChannel: OutputChannel | undefined;

function ensureOutputChannel(context: ExtensionContext): OutputChannel {
  if (!outputChannel) {
    outputChannel = window.createOutputChannel("NML Language Server");
    context.subscriptions.push(outputChannel);
  }
  return outputChannel;
}

/** Same charset as a package name — the tool is both a package name and a spawn
 *  target, so this guards path-traversal / spawn abuse (RFC 0035 Security). */
const TOOL_NAME = /^[a-z][a-z0-9-]*$/;

/** A resolved server to launch: a declared provider's `<tool> lsp` and the
 *  native neutral server are both processes; the bundled WASM neutral server is
 *  the third shape. Provider vs neutral is the discovery ladder; the neutral
 *  *delivery* (wasm/native) is [`resolveNeutralServer`]. */
type Resolution = NeutralServer;

/** Lightweight bootstrap read of `provider: tool = "<name>"` from an
 *  `nml-project.nml`. The server does the authoritative parse; this only has to
 *  decide which server to launch, so a scan is enough (and it charset-validates
 *  inline). */
function parseProviderTool(text: string): string | undefined {
  const lines = text.split(/\r?\n/);
  let providerIndent = -1;
  for (const raw of lines) {
    const trimmed = raw.trim();
    if (trimmed === "" || trimmed.startsWith("//")) continue;
    const indent = raw.length - raw.trimStart().length;
    if (providerIndent < 0) {
      if (/^provider\s*:/.test(trimmed)) providerIndent = indent;
      continue;
    }
    if (indent <= providerIndent) break; // provider block ended
    const m = trimmed.match(/^tool\s*=\s*"([^"]*)"/);
    if (m) return m[1];
  }
  return undefined;
}

/** The declared provider tool for this window, read from each workspace
 *  folder's root `nml-project.nml`. A tool with an invalid name is ignored
 *  (the server surfaces that separately). Divergent providers across roots →
 *  neutral (no single answer). */
async function declaredProviderTool(): Promise<string | undefined> {
  const folders = workspace.workspaceFolders ?? [];
  const found = new Set<string>();
  for (const folder of folders) {
    // `workspace.fs` (not node `fs`) so the read goes through the workspace's
    // filesystem provider — correct on remote/virtual workspaces, and never
    // blocking the shared extension host.
    let text: string;
    try {
      const bytes = await workspace.fs.readFile(
        Uri.joinPath(folder.uri, "nml-project.nml")
      );
      text = new TextDecoder().decode(bytes);
    } catch {
      continue;
    }
    const tool = parseProviderTool(text);
    if (tool && TOOL_NAME.test(tool)) found.add(tool);
  }
  return found.size === 1 ? [...found][0] : undefined;
}

/** Resolve an executable via `PATH` only — never from workspace directories, so
 *  a workspace-resident binary can never be launched by discovery. Returns the
 *  absolute path, or undefined if not found on PATH. (PATH is a host concept, so
 *  this uses node fs — asynchronously, to keep the extension host responsive.) */
async function resolveOnPath(tool: string): Promise<string | undefined> {
  const exts =
    process.platform === "win32"
      ? (process.env.PATHEXT ?? ".EXE;.CMD;.BAT").split(";")
      : [""];
  for (const dir of (process.env.PATH ?? "").split(path.delimiter)) {
    if (!dir) continue;
    for (const ext of exts) {
      const candidate = path.join(dir, tool + ext);
      try {
        await fsp.access(candidate, fsConstants.X_OK);
        return candidate;
      } catch {
        /* keep looking */
      }
    }
  }
  return undefined;
}

/** True when `p` lies inside any workspace folder — such a binary is refused as
 *  a language server (defense in depth atop the PATH-only resolution). */
async function insideWorkspace(p: string): Promise<boolean> {
  const real = await realPath(p);
  for (const folder of workspace.workspaceFolders ?? []) {
    const rel = path.relative(await realPath(folder.uri.fsPath), real);
    if (rel !== "" && !rel.startsWith("..") && !path.isAbsolute(rel)) return true;
  }
  return false;
}

/** `fs.realpath`, but never throwing (a racing delete must not crash
 *  activation) — falls back to the given path. */
async function realPath(p: string): Promise<string> {
  try {
    return await fsp.realpath(p);
  } catch {
    return p;
  }
}

/** The discovery ladder (RFC 0035). Prefers a declared provider tool's own LSP
 *  when it is safe to launch, else the neutral server ([`resolveNeutralServer`]:
 *  bundled WASM, or native). May prompt once per workspace for approval. */
async function resolveServer(context: ExtensionContext): Promise<Resolution> {
  const tool = await declaredProviderTool();
  if (!tool) return resolveNeutralServer(context);

  // Trust gate: never launch a project-resolved binary in an untrusted
  // workspace. The neutral server still validates committed/cached schema.
  if (!workspace.isTrusted) return resolveNeutralServer(context);

  const command = await resolveOnPath(tool);
  if (!command || (await insideWorkspace(command))) {
    // Declared but not a user-global install (or workspace-resident): fall back
    // rather than hunt for it in the workspace. Neutral still serves the
    // tool's published package by name (the tool→package fallback).
    return resolveNeutralServer(context);
  }

  // Per-workspace approval, remembered. Approving is trusting `<tool> lsp` to
  // run as this project's language server.
  const key = `nml.approvedProvider.${tool}`;
  const remembered = context.workspaceState.get<string>(key);
  if (remembered !== command) {
    const choice = await window.showInformationMessage(
      `This project asks to use "${tool}" as its NML language server ` +
        `(resolved to ${command}). Use it?`,
      "Use it",
      "Use neutral server"
    );
    if (choice !== "Use it") return resolveNeutralServer(context);
    await context.workspaceState.update(key, command);
  }

  return {
    kind: "process",
    command,
    args: ["lsp"],
    label: `${tool} (in-binary)`,
  };
}

// ── Status surface (RFC 0035 C3) ─────────────────────────────────────────

let statusTimer: ReturnType<typeof setTimeout> | undefined;

/** Coalesce bursty triggers (a store flip re-publishes diagnostics; typing
 *  churns them) into one trailing status refresh. */
function scheduleStatusRefresh(): void {
  if (statusTimer) clearTimeout(statusTimer);
  statusTimer = setTimeout(() => {
    statusTimer = undefined;
    void refreshStatus();
  }, 250);
}

async function refreshStatus(): Promise<void> {
  if (!status) return;
  const editor = window.activeTextEditor;
  if (!editor || editor.document.languageId !== "nml") {
    status.hide();
    return;
  }
  status.show();
  if (!client) {
    status.text = "$(circle-slash) nml: no server";
    status.tooltip = "The NML language server is not running.";
    status.backgroundColor = new ThemeColor("statusBarItem.warningBackground");
    return;
  }
  try {
    const info: any = await client.sendRequest("nml/schemaInfo", {
      uri: editor.document.uri.toString(),
    });
    status.backgroundColor = undefined;
    if (info?.bound) {
      // Name the active delivery channel (RFC 0035): source is "workspace
      // manifest" | "in-binary" | "store current" | "builtin".
      status.text = `$(check) nml: ${info.package} ${info.version}`;
      status.tooltip =
        `Schema: ${info.package} ${info.version}\n` +
        `Channel: ${info.source}\n` +
        `Server: ${serverLabel}` +
        (info.shadowsStore ? `\n(workspace manifest shadows the store copy)` : "");
    } else {
      status.text = "$(info) nml: no schema";
      status.tooltip =
        `No schema package governs this file.\nServer: ${serverLabel}\n` +
        `Commit a <name>.package.nml, or run your tool's \`schema sync\`.`;
    }
  } catch {
    status.text = "$(check) nml";
    status.tooltip = `Server: ${serverLabel}`;
  }
}

/** Serializes client start/stop. A restart (command, or a trust grant) can
 *  arrive while a start is mid-flight — e.g. the approval prompt is showing, or
 *  `client.start()` is in progress. Without serialization two `startClient`
 *  runs race and the first `LanguageClient` leaks (untracked, never stopped).
 *  Chaining makes every lifecycle op wait for the previous to settle. */
let lifecycle: Promise<unknown> = Promise.resolve();
function serialize(op: () => Promise<void>): Promise<void> {
  const next = lifecycle.then(op, op);
  lifecycle = next;
  return next;
}

async function startClient(context: ExtensionContext): Promise<void> {
  const resolution = await resolveServer(context);
  serverLabel = resolution.label;

  const channel = ensureOutputChannel(context);
  const serverOptions: ServerOptions =
    resolution.kind === "wasm"
      ? async () => {
          const server = await createWasmServer(resolution.module, channel);
          wasmProcess = server.process; // tracked so we can terminate it on stop
          return server.transports;
        }
      : { command: resolution.command, args: resolution.args };
  channel.appendLine(`Starting NML LSP (${resolution.label})`);

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "nml" }],
    outputChannel: channel,
    // The WASM neutral server runs a synchronous pump and cannot issue the
    // server→client request that registers file watchers (RFC 0035), so the
    // client watches on its behalf; the native server self-registers, so adding
    // it there too would double the events. It also lives in a WASI filesystem
    // namespace, so URIs are rewritten to the mounted paths it can actually read.
    ...(resolution.kind === "wasm"
      ? {
          synchronize: { fileEvents: workspace.createFileSystemWatcher("**/*.nml") },
          uriConverters: wasmUriConverters(),
        }
      : {}),
  };
  client = new LanguageClient("nml-lsp", "NML Language Server", serverOptions, clientOptions);
  try {
    await client.start();
  } catch (err) {
    channel.appendLine(`NML: failed to start language server: ${err}`);
    window.showErrorMessage(
      `NML: failed to start the NML language server (${resolution.label}). ` +
        `Set nml.server.path to an nml-lsp binary, or install one.`
    );
    client = undefined;
  }
  await refreshStatus();
}

/** Terminate the WASM backend, if one is running. A no-op for the process
 *  backend (the client reaps its own child). Must run AFTER the client releases
 *  the transports so we don't race an in-flight write. */
async function terminateWasm(): Promise<void> {
  const proc = wasmProcess;
  wasmProcess = undefined;
  if (proc) await proc.terminate().catch(() => undefined);
}

async function restartClient(context: ExtensionContext): Promise<void> {
  const old = client;
  client = undefined;
  if (old) await old.stop().catch(() => undefined);
  await terminateWasm();
  await startClient(context);
}

export async function activate(context: ExtensionContext): Promise<void> {
  status = window.createStatusBarItem(StatusBarAlignment.Right, 100);
  status.command = "nml.restartServer";
  context.subscriptions.push(status);

  context.subscriptions.push(
    commands.registerCommand("nml.restartServer", () =>
      serialize(() => restartClient(context))
    ),
    // Gaining trust can upgrade a neutral server to the declared provider's LSP.
    workspace.onDidGrantWorkspaceTrust(() =>
      serialize(() => restartClient(context))
    ),
    // Active-editor change is a discrete event — refresh immediately.
    window.onDidChangeActiveTextEditor(() => void refreshStatus()),
    // The freshness poll re-publishes diagnostics on a store flip, which keeps
    // the channel label current without a bespoke push notification. Refresh
    // only when the ACTIVE document's diagnostics changed, debounced — other
    // files' churn and per-keystroke bursts must not spam schemaInfo requests.
    languages.onDidChangeDiagnostics((e) => {
      const active = window.activeTextEditor?.document.uri.toString();
      if (active && e.uris.some((u) => u.toString() === active)) {
        scheduleStatusRefresh();
      }
    })
  );

  await serialize(() => startClient(context));
}

export function deactivate(): Thenable<void> | undefined {
  if (statusTimer) clearTimeout(statusTimer);
  const stop = client?.stop();
  if (!stop && !wasmProcess) return undefined;
  // Terminate the WASM backend only after the client releases the transports.
  return Promise.resolve(stop).then(() => terminateWasm());
}
