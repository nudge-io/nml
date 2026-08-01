import { isRecord, readBoolean, readString, readStringArray } from "./wire";

/** Mirrors [`server.rs` schema_info wire shape](nml/crates/nml-lsp/src/server.rs). */

export interface SchemaInfoNote {
  message: string;
  severity: "warning" | "info";
}

export interface SchemaInfoBound {
  bound: true;
  package: string;
  version: string;
  contentHash: string;
  binding: string;
  source: string;
  step: "pinned" | "auto-associated";
  root: string;
  shadowsStore: boolean;
  actions: readonly string[];
  notes: readonly SchemaInfoNote[];
}

export interface SchemaInfoUnbound {
  bound: false;
  notes: readonly SchemaInfoNote[];
}

export type SchemaInfo = SchemaInfoBound | SchemaInfoUnbound;

export type SchemaInfoResult =
  | { kind: "ok"; info: SchemaInfo }
  | { kind: "error"; message: string }
  | { kind: "unavailable" };

/** Parse `nml/schemaInfo` wire — distinguishes server error payloads from misses. */
export function parseSchemaInfoResult(raw: unknown): SchemaInfoResult {
  if (!isRecord(raw)) return { kind: "unavailable" };
  const error = readString(raw, "error");
  if (error) return { kind: "error", message: error };
  const info = parseSchemaInfo(raw);
  return info ? { kind: "ok", info } : { kind: "unavailable" };
}

/** Matches [`nml_validate::store::hash8`](nml/crates/nml-validate/src/store.rs). */
export function hash8(contentHash: string): string {
  const bare = contentHash.startsWith("blake3:")
    ? contentHash.slice("blake3:".length)
    : contentHash;
  return bare.slice(0, 8);
}

/** Display form used by server hover at (0,0) — `blake3:{hash8}`. */
export function formatHashShort(contentHash: string): string {
  return `blake3:${hash8(contentHash)}`;
}

function parseNotes(raw: unknown): SchemaInfoNote[] | undefined {
  if (!Array.isArray(raw)) return undefined;
  const notes: SchemaInfoNote[] = [];
  for (const item of raw) {
    if (!isRecord(item)) return undefined;
    const message = readString(item, "message");
    const severity = readString(item, "severity");
    if (!message || (severity !== "warning" && severity !== "info")) return undefined;
    notes.push({ message, severity });
  }
  return notes;
}

export function parseSchemaInfo(raw: unknown): SchemaInfo | undefined {
  if (!isRecord(raw)) return undefined;
  const bound = readBoolean(raw, "bound");
  const notes = parseNotes(raw.notes) ?? [];

  if (bound === false) {
    return { bound: false, notes };
  }
  if (bound !== true) return undefined;

  const packageName = readString(raw, "package");
  const version = readString(raw, "version");
  const contentHash = readString(raw, "contentHash");
  const binding = readString(raw, "binding");
  const source = readString(raw, "source");
  const step = readString(raw, "step");
  const root = readString(raw, "root");
  const shadowsStore = readBoolean(raw, "shadowsStore");
  const actions = readStringArray(raw, "actions");

  if (
    !packageName ||
    !version ||
    !contentHash ||
    !binding ||
    !source ||
    !root ||
    shadowsStore === undefined ||
    !actions ||
    (step !== "pinned" && step !== "auto-associated")
  ) {
    return undefined;
  }

  return {
    bound: true,
    package: packageName,
    version,
    contentHash,
    binding,
    source,
    step,
    root,
    shadowsStore,
    actions,
    notes,
  };
}
