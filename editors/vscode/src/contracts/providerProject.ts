/** Same charset as a package name — the tool is both a package name and a spawn
 *  target, so this guards path-traversal / spawn abuse (RFC 0035 Security). */
export const TOOL_NAME = /^[a-z][a-z0-9-]*$/;

export function isValidToolName(tool: string): boolean {
  return TOOL_NAME.test(tool);
}

/** Lightweight bootstrap read of `provider: tool = "<name>"` from `nml-project.nml`.
 *  The server does the authoritative parse; discovery only needs enough to pick
 *  which language server to launch. */
export function parseProviderTool(text: string): string | undefined {
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
    if (indent <= providerIndent) break;
    const m = trimmed.match(/^tool\s*=\s*"([^"]*)"/);
    if (m) return m[1];
  }
  return undefined;
}
