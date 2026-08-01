// ─────────────────────────────────────────────────────────────────────────
// Pure logic of the WASM neutral-server bridge: host↔WASI URI mapping and
// stderr classification. No `vscode` import — the unit harness (plain Node
// mocha) exercises this directly; serverAcquisition.ts owns the VS Code
// wiring and delegates here.
// ─────────────────────────────────────────────────────────────────────────

/** The two facts about a workspace folder the WASI mount scheme consumes. */
export interface WasmFolder {
  readonly name: string;
  readonly uriString: string;
}

/** One host↔guest prefix pair; both sides are URI strings without a trailing slash. */
export interface WasmUriMapEntry {
  readonly hostPrefix: string;
  readonly wasiPrefix: string;
}

/** Folder names that appear more than once (each reported once).
 *  wasm-wasi-core mounts multi-root folders by NAME (`/workspaces/<name>`),
 *  so two folders sharing a basename collapse onto one guest mount and
 *  every URI for the loser cross-attributes to the winner — diagnostics
 *  land on the wrong files. The host scheme owns the collision; the best
 *  the extension can do is refuse to be silent about it. */
export function duplicateFolderNames(folders: readonly WasmFolder[]): string[] {
  const names = folders.map((f) => f.name);
  return [...new Set(names.filter((n, i) => names.indexOf(n) !== i))];
}

/** Map between host file URIs and the WASM server's WASI filesystem namespace.
 *  `wasm-wasi-core`'s `mapWorkspaceFolder` mounts a lone workspace folder at
 *  `/workspace` and each folder of a multi-root workspace at
 *  `/workspaces/${folder.name}` (verified against its source) — so the mount
 *  segment is exactly `WorkspaceFolder.name`, which is what this reads: the two
 *  sides agree by construction, not by guess. */
export function buildUriMapping(folders: readonly WasmFolder[]): WasmUriMapEntry[] {
  const single = folders.length === 1;
  return folders
    .map((f) => ({
      hostPrefix: f.uriString.replace(/\/$/, ""),
      wasiPrefix: `file://${single ? "/workspace" : `/workspaces/${f.name}`}`,
    }))
    // Longest host prefix first: with nested workspace folders (`/a` and
    // `/a/b`) the most specific mount must win, not whichever comes first.
    .sort((a, b) => b.hostPrefix.length - a.hostPrefix.length);
}

/** Host URI string → guest URI string; URIs outside every mount pass through. */
export function hostToWasi(mapping: readonly WasmUriMapEntry[], uri: string): string {
  for (const m of mapping) {
    if (uri === m.hostPrefix || uri.startsWith(`${m.hostPrefix}/`)) {
      return m.wasiPrefix + uri.slice(m.hostPrefix.length);
    }
  }
  return uri;
}

/** Guest URI string → host URI string; the inverse of [`hostToWasi`]. */
export function wasiToHost(mapping: readonly WasmUriMapEntry[], value: string): string {
  for (const m of mapping) {
    if (value === m.wasiPrefix || value.startsWith(`${m.wasiPrefix}/`)) {
      return m.hostPrefix + value.slice(m.wasiPrefix.length);
    }
  }
  return value;
}

/** True for stderr lines that look like a Rust abort (panic message or the
 *  RUST_BACKTRACE hint) — the lines worth echoing to console.error so the E2E
 *  harness can recover the cause of a dead server. */
export function isPanicShapedStderr(text: string): boolean {
  return text.includes("panicked") || text.includes("RUST_BACKTRACE");
}
