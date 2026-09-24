import type { Uri } from "vscode";
import { LaunchSandbox } from "./pathSecurity";

// ─────────────────────────────────────────────────────────────────────────
// WHAT the extension decided to run, before anything runs it.
//
// PURE by design — `import type` only for the editor's `Uri`, so this module
// carries no runtime dependency on `vscode` or on the WASI host extension.
// That is not tidiness: `processLaunch.ts` (the one spawn site) and the
// real-process suite both need the resolution VOCABULARY and neither may pull
// the extension host in. Before this module existed the vocabulary lived in
// `serverAcquisition.ts` beside `Wasm.load()`, so the only way a pure
// consumer could name a resolution was `import type` — i.e. it could describe
// one but never CONSTRUCT one, and the real-process suite wrote look-alike
// literals with no sandbox in them instead.
//
// Two producers use it — `serverAcquisition.resolveNeutralServer` (the
// bundled wasm backend, `nml.server.path`, the native default) and
// `providerDiscovery.resolveServer` (a `<tool> lsp` a repository declared) —
// and every consumer (session, launch, the operator's messages) reads it.
// ─────────────────────────────────────────────────────────────────────────

/** What the bundled WebAssembly backend is called, in one place — the
 *  resolution and the running server must not be able to disagree. */
export const WASM_LABEL = "neutral nml-lsp (wasm)";

/** WHOSE decision put this program on the command line — the fact the
 *  operator's remedy turns on when it fails to start.
 *
 *  Carried as a TYPE because the alternative was reading it back off the
 *  display LABEL (`label.endsWith("(in-binary)")`,
 *  `label.includes("nml.server.path")`): a label is what the status bar
 *  prints, so rewording one silently handed the operator the wrong remedy,
 *  and a fourth origin would have been a fourth unexhausted `if`. */
export type ServerOrigin =
  /** A `<tool> lsp` a repository declared and the operator approved
   *  (RFC 0035's in-binary channel). The project chose it; no setting
   *  overrides it. */
  | "provider"
  /** The operator's own machine-scoped `nml.server.path`. */
  | "setting"
  /** The native default (`~/.cargo/bin/nml-lsp`, or `nml-lsp` on `PATH`),
   *  reached only by a build that bundles no wasm server. */
  | "default";

/** What the client must see in the `initialize` answer for this process to
 *  keep running. Present only on a resolution the extension was TOLD to
 *  launch by a repository (`<tool> lsp`): the operator's own
 *  `nml.server.path` and the native default are binaries they chose
 *  themselves, and holding those to a handshake would break every
 *  legitimate fork of the server. */
export interface ServerIdentityContract {
  /** `serverInfo.name` (LSP 3.17) the launched program must answer with. */
  readonly expect: string;
  /** What the operator called it — the tool name, for the message. */
  readonly tool: string;
  /** Run when the handshake gives a DEFINITE negative: the approval is
   *  withdrawn and the provider is skipped for the rest of this session, so
   *  the fallback cannot loop back into the same launch. */
  readonly repudiate: () => Promise<void>;
  /** Run when the handshake is unanswered inside its budget, or answered
   *  without a name. Skips the provider for this session and leaves the
   *  approval alone — neither is evidence against what the operator
   *  approved. */
  readonly standDown: () => Promise<void>;
}

/** A server that is a child process; `cwd` and `env` are the sandbox it is
 *  spawned into — always a [`LaunchSandbox`], stamped by [`processServer`] so
 *  no resolution site can leave the working directory to the language
 *  client's workspace default or the environment unscrubbed. */
export interface ProcessServer {
  kind: "process";
  command: string;
  args: string[];
  /** Branded: only `launchSandbox` can mint one, so there is no launch
   *  path into `processLaunch.ts` with half a sandbox. */
  cwd: LaunchSandbox["cwd"];
  env: Readonly<Record<string, undefined>>;
  label: string;
  /** Who chose this program — see [`ServerOrigin`]. */
  origin: ServerOrigin;
  identity?: ServerIdentityContract;
}

/** The bundled WebAssembly backend, which has no command line at all. */
export interface WasmServerResolution {
  kind: "wasm";
  module: Uri;
  label: string;
}

/** What one round of resolution decided to run: the bundled wasm backend,
 *  the operator's own binary, the native default, or the `<tool> lsp` a
 *  repository declared. Named for what it IS and for nothing narrower — a
 *  union named after the commonest of its members is a name that asserts a
 *  property the rest of them do not have. */
export type ServerResolution = ProcessServer | WasmServerResolution;

/** The one constructor of a process-backed resolution. */
export function processServer(
  command: string,
  args: string[],
  label: string,
  origin: ServerOrigin,
  sandbox: LaunchSandbox,
  identity?: ServerIdentityContract
): ProcessServer {
  return {
    kind: "process",
    command,
    args,
    cwd: sandbox.cwd,
    env: sandbox.env,
    label,
    origin,
    ...(identity ? { identity } : {}),
  };
}
