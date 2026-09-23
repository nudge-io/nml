/** Same charset as a package name — the tool is both a package name and a spawn
 *  target, so this guards path-traversal / spawn abuse (RFC 0035 Security). */
export const TOOL_NAME = /^[a-z][a-z0-9-]*$/;

export function isValidToolName(tool: string): boolean {
  return TOOL_NAME.test(tool);
}

/** A repository's `provider:` declaration, read once and used twice: the
 *  `tool` picks which language server to launch, and `canonical` is the text
 *  an approval is pinned to (`providerTrust.declarationDigest`). One walk
 *  produces both, so the name that was approved and the name that is spawned
 *  can never come from different readings of the file. */
export interface ProviderBlock {
  /** The declared `tool = "<name>"`, if the block names one. */
  readonly tool: string | undefined;
  /** The block's significant lines — comments, blank lines and the file's
   *  absolute indentation removed, each remaining line kept at its depth
   *  RELATIVE to `provider:` and its internal whitespace collapsed.
   *
   *  So: re-indenting the file, adding a comment, or editing an unrelated
   *  section leaves this identical (no re-prompt), while changing what the
   *  block says — a different tool, a new key, a key nested one level
   *  deeper — changes it (a fresh question). */
  readonly canonical: string;
}

const INDENT_STEP = 4;

/** Lightweight bootstrap read of the `provider:` block from `nml-project.nml`.
 *  The server does the authoritative parse; discovery only needs enough to
 *  pick which language server to launch and to know what it was asked. */
export function parseProviderBlock(text: string): ProviderBlock | undefined {
  const lines = text.split(/\r?\n/);
  let providerIndent = -1;
  let tool: string | undefined;
  const canonical: string[] = [];
  for (const raw of lines) {
    const withoutComment = raw.replace(/\/\/.*$/, "");
    const trimmed = withoutComment.trim();
    if (trimmed === "") continue;
    const indent = withoutComment.length - withoutComment.trimStart().length;
    if (providerIndent < 0) {
      if (/^provider\s*:/.test(trimmed)) {
        providerIndent = indent;
        canonical.push("provider:");
      }
      continue;
    }
    if (indent <= providerIndent) break;
    const depth = Math.max(1, Math.round((indent - providerIndent) / INDENT_STEP));
    canonical.push(`${" ".repeat(depth * 2)}${trimmed.replace(/\s+/g, " ")}`);
    if (tool === undefined) {
      const m = trimmed.match(/^tool\s*=\s*"([^"]*)"/);
      if (m) tool = m[1];
    }
  }
  if (providerIndent < 0) return undefined;
  return { tool, canonical: canonical.join("\n") };
}

/** The declared tool name alone. */
export function parseProviderTool(text: string): string | undefined {
  return parseProviderBlock(text)?.tool;
}
