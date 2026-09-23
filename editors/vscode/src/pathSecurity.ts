import { promises as fsp } from "fs";
import * as os from "os";
import * as path from "path";
import { scrubbedEnvOverlay } from "./providerTrust";

/** `fs.realpath`, but never throwing — a racing delete must not crash activation. */
export async function realPath(p: string): Promise<string> {
  try {
    return await fsp.realpath(p);
  } catch {
    return p;
  }
}

/** Expand a leading `~/` (or bare `~`) to `homedir` — the documented
 *  `~/.cargo/bin/nml-lsp` form must work. Only the invoking user's home:
 *  `~otheruser/...` is NOT resolved (no passwd lookups) and falls through to
 *  the absolute-path check, which refuses it. A missing or relative `homedir`
 *  leaves the path untouched. */
export function expandHomePrefix(p: string, homedir: string): string {
  if (!homedir || !path.isAbsolute(homedir)) return p;
  if (p === "~") return homedir;
  if (p.startsWith("~/")) return path.join(homedir, p.slice(2));
  return p;
}

/** The working directory every process-backed server is spawned in —
 *  a project's `<tool> lsp`, an `nml.server.path` override, the native
 *  default. NEVER a workspace folder: `vscode-languageclient` defaults an
 *  executable's cwd to the first one, and the tool name a project declares
 *  may resolve to an interpreter (`sh`, `node`, `python3` all satisfy
 *  `TOOL_NAME`), for which the fixed argument `lsp` is a SCRIPT PATH
 *  resolved against the cwd — a repository shipping a file named `lsp`
 *  beside its `nml-project.nml` would run it the moment the operator
 *  accepted the prompt. The home directory is the operator's own (a
 *  terminal's starting point, nothing a repository controls); a homeless
 *  or relative one falls back to the filesystem root, where nothing
 *  relative resolves. No server reads its cwd: LSP hands it the roots. */
export function providerWorkingDir(homedir: string, fsRoot: string): string {
  return homedir && path.isAbsolute(homedir) ? homedir : fsRoot;
}

/** A working directory that [`launchSandbox`] minted, and that nothing else
 *  can spell.
 *
 *  The brand is type-level only — `declare const` emits nothing, and the
 *  property is never written at runtime — and it is what makes the sentence
 *  `processLaunch.ts` opens with TRUE rather than agreed: `ProcessServer.cwd`
 *  is this type, so a resolution site cannot hand the launch a bare path.
 *  Without it the interface was structural and a hand-written
 *  `{ kind: "process", cwd: someFolder, env: {} }` typechecked — a spawn in a
 *  workspace folder, where the fixed `lsp` argument is a file an interpreter
 *  would run, with nothing scrubbed out of the environment. */
declare const launchSandboxBrand: unique symbol;
export type SandboxCwd = string & { readonly [launchSandboxBrand]: true };

/** Everything about a spawn that is not the program and its arguments: where
 *  it starts, and which inherited environment variables are removed first.
 *  Minted in exactly one place ([`launchSandbox`]) and carried by exactly one
 *  constructor (`serverAcquisition.processServer`), so no resolution site can
 *  reach a spawn with half of it. */
export interface LaunchSandbox {
  readonly cwd: SandboxCwd;
  /** Overlaid on the inherited environment; every value is `undefined`,
   *  which is what REMOVES a variable (see `providerTrust.scrubbedEnvOverlay`). */
  readonly env: Readonly<Record<string, undefined>>;
}

/** The sandbox every process-backed server is spawned into.
 *
 *  `privateDir` is an empty directory the extension owns
 *  (`clientManager.privateWorkingDir` creates it under the extension's global
 *  storage, mode 0700). Empty is the point: the fixed `lsp` argument is a
 *  SCRIPT PATH for any interpreter a declared tool name resolves to, and on
 *  Windows the default DLL search order includes the current directory — an
 *  empty directory has nothing for either to find. Global storage rather than
 *  a temp directory: on a shared machine `/tmp` is reachable by other local
 *  users, and the profile directory is not.
 *
 *  It falls back to [`providerWorkingDir`] when the private directory cannot
 *  be made (a read-only or full profile): the operator's home is still never
 *  a workspace folder, which is the property that matters, and a server that
 *  cannot start is worse than one starting in a directory with files in it. */
export function launchSandbox(privateDir: string | undefined): LaunchSandbox {
  const cwd =
    privateDir && path.isAbsolute(privateDir)
      ? privateDir
      : providerWorkingDir(os.homedir(), path.parse(process.cwd()).root);
  // The ONE place a [`SandboxCwd`] comes into existence.
  return { cwd: cwd as SandboxCwd, env: scrubbedEnvOverlay(process.env) };
}

/** True when `targetReal` lies inside any of `rootReals` (each already realpath'd).
 *  Pure string containment — the case-insensitive-filesystem hole is closed by
 *  the inode walk in [`isPathInsideWorkspace`]; this remains the fallback for
 *  paths that do not exist. */
export function isPathInsideWorkspaceRoots(
  targetReal: string,
  rootReals: readonly string[]
): boolean {
  for (const rootReal of rootReals) {
    const rel = path.relative(rootReal, targetReal);
    if (rel !== "" && !rel.startsWith("..") && !path.isAbsolute(rel)) {
      return true;
    }
  }
  return false;
}

interface FileIdentity {
  readonly dev: number;
  readonly ino: number;
}

async function fileIdentity(p: string): Promise<FileIdentity | undefined> {
  try {
    const st = await fsp.stat(p);
    return { dev: st.dev, ino: st.ino };
  } catch {
    return undefined;
  }
}

/** Inode-identity containment: `realpath` resolves symlinks but does NOT
 *  canonicalize case, so on a case-insensitive filesystem (APFS, NTFS) a
 *  case-variant spelling of a workspace-resident path passes the string check.
 *  Walk the target's ancestor chain comparing `(dev, ino)` against the root —
 *  any ancestor match ⇒ inside. The target itself matching the root does not
 *  count (mirrors the string semantics: the root is not inside itself).
 *  Nonexistent ancestors are skipped; a nonexistent root means no verdict. */
async function isInsideByIdentity(targetReal: string, rootReal: string): Promise<boolean> {
  const root = await fileIdentity(rootReal);
  if (!root) return false;
  let p = path.dirname(targetReal);
  for (;;) {
    const id = await fileIdentity(p);
    if (id && id.dev === root.dev && id.ino === root.ino) return true;
    const parent = path.dirname(p);
    if (parent === p) return false;
    p = parent;
  }
}

/** Defense in depth: refuse binaries that live inside an open workspace folder. */
export async function isPathInsideWorkspace(
  p: string,
  workspaceRoots: readonly string[]
): Promise<boolean> {
  if (workspaceRoots.length === 0) return false;
  const targetReal = await realPath(p);
  const rootReals = await Promise.all(workspaceRoots.map((r) => realPath(r)));
  if (isPathInsideWorkspaceRoots(targetReal, rootReals)) return true;
  for (const rootReal of rootReals) {
    if (await isInsideByIdentity(targetReal, rootReal)) return true;
  }
  return false;
}

export type NeutralServerPathOverride =
  | { accepted: true; command: string }
  | { accepted: false; reason: "relative" | "inside-workspace" };

/** Validate `nml.server.path` before spawn — absolute (after `~` expansion),
 *  outside workspace. The accepted command is the expanded path. */
export async function evaluateNeutralServerPathOverride(
  pathOverride: string,
  workspaceRoots: readonly string[]
): Promise<NeutralServerPathOverride> {
  const command = expandHomePrefix(pathOverride.trim(), os.homedir());
  if (!path.isAbsolute(command)) {
    return { accepted: false, reason: "relative" };
  }
  if (await isPathInsideWorkspace(command, workspaceRoots)) {
    return { accepted: false, reason: "inside-workspace" };
  }
  return { accepted: true, command };
}
