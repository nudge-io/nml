import { existsSync } from "node:fs";
import { dirname, join, relative } from "node:path";

const WORKSPACE_FILE = "pnpm-workspace.yaml";

/**
 * @param {string} fromDir absolute directory to search upward from
 * @returns {{ workspaceRoot: string; packageDir: string; importerKey: string }}
 */
export function resolveWorkspaceContext(fromDir) {
  let dir = fromDir;
  while (true) {
    if (existsSync(join(dir, WORKSPACE_FILE))) {
      const importerKey = relative(dir, fromDir).split("\\").join("/");
      if (!importerKey || importerKey === ".") {
        throw new Error(
          `resolveWorkspaceContext: package directory must be a workspace member (got ${fromDir})`
        );
      }
      return { workspaceRoot: dir, packageDir: fromDir, importerKey };
    }
    const parent = dirname(dir);
    if (parent === dir) {
      throw new Error(`resolveWorkspaceContext: no ${WORKSPACE_FILE} found above ${fromDir}`);
    }
    dir = parent;
  }
}
