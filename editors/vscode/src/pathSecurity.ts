import { promises as fsp } from "fs";
import * as path from "path";

/** `fs.realpath`, but never throwing — a racing delete must not crash activation. */
export async function realPath(p: string): Promise<string> {
  try {
    return await fsp.realpath(p);
  } catch {
    return p;
  }
}

/** True when `targetReal` lies inside any of `rootReals` (each already realpath'd). */
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

/** Defense in depth: refuse binaries that live inside an open workspace folder. */
export async function isPathInsideWorkspace(
  p: string,
  workspaceRoots: readonly string[]
): Promise<boolean> {
  if (workspaceRoots.length === 0) return false;
  const targetReal = await realPath(p);
  const rootReals = await Promise.all(workspaceRoots.map((r) => realPath(r)));
  return isPathInsideWorkspaceRoots(targetReal, rootReals);
}
