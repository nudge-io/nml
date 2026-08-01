import { promises as fsp } from "fs";
import * as os from "os";
import * as path from "path";

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
