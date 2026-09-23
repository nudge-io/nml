import { isRecord, readBoolean, readString, readStringArray } from "./wire";

/** Mirrors [`server.rs` schema_info wire shape](nml/crates/nml-lsp/src/server.rs). */

/**
 * One degraded-state note. An `error` note is a REFUSAL: the server validated
 * the document under nothing (two manifests claim it, a manifest or project
 * config failed to load, the walk could not finish, no key can carry its path)
 * and the note says why and what to do.
 */
export interface SchemaInfoNote {
  message: string;
  severity: "error" | "warning" | "info";
}

/**
 * How the server fixed the document's universe — the facts the CLI's
 * `note: workspace root …` line states. `rootOrigin` is `editor` for a
 * workspace folder and a derivation for a document outside every folder;
 * `rootFence` names the derived fence entry's kind and `rootShadowed` the
 * entry above the fence that shadows the universe, spelled from the root.
 * Every field is optional: an older server sends none.
 */
export interface RootFacts {
  rootOrigin?: "editor" | "derivedVcsFence" | "derivedTargetDir";
  rootFence?: "dir" | "file" | "symlink" | "other";
  rootShadowed?: string;
  /**
   * Whether the universe DECIDES what governs the files under it — the
   * kernel's own two words, the same ones the CLI's `--json` `binding`
   * row carries. `closed`: a package manifest was found, so a file no
   * `files` glob claims is governed by nothing, on purpose. `open`: no
   * manifest within the fence claims anything.
   *
   * The two UNBOUND states have DIFFERENT remedies, and without this
   * field the status bar gave the open one for both. Optional: an older
   * server sends none, and the tooltip then says only what it knows.
   */
  universe?: "open" | "closed";
}

/**
 * The binding's composition grant — the `--json` `binding` row's `layers`
 * object, one spelling: `granted` alone when composition is denied or the
 * open context permits it; the grant's own rules when a binding carries
 * one. What a denial's quick fix will produce, readable before it is
 * applied. Optional: an older server sends none.
 */
export interface LayersInfo {
  granted: boolean;
  allowRefs?: readonly string[];
  denyRefs?: readonly string[];
  maxStackDepth?: number | null;
}

export interface SchemaInfoBound extends RootFacts {
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
  layers?: LayersInfo;
}

export interface SchemaInfoUnbound extends RootFacts {
  bound: false;
  notes: readonly SchemaInfoNote[];
  layers?: LayersInfo;
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
    if (!message) return undefined;
    if (severity !== "error" && severity !== "warning" && severity !== "info") return undefined;
    notes.push({ message, severity });
  }
  return notes;
}

/**
 * The grant, kept only when every field it carries is what this client
 * knows — a malformed `layers` is absent, never a rejected payload (the
 * field is optional).
 */
function parseLayers(raw: unknown): LayersInfo | undefined {
  if (!isRecord(raw)) return undefined;
  const granted = readBoolean(raw, "granted");
  if (granted === undefined) return undefined;
  const layers: LayersInfo = { granted };
  if (raw.allowRefs !== undefined) {
    const allowRefs = readStringArray(raw, "allowRefs");
    if (!allowRefs) return undefined;
    layers.allowRefs = allowRefs;
  }
  if (raw.denyRefs !== undefined) {
    const denyRefs = readStringArray(raw, "denyRefs");
    if (!denyRefs) return undefined;
    layers.denyRefs = denyRefs;
  }
  if (raw.maxStackDepth !== undefined) {
    const depth = raw.maxStackDepth;
    if (depth !== null && typeof depth !== "number") return undefined;
    layers.maxStackDepth = depth;
  }
  return layers;
}

/** The root facts, each kept only when it is a word this client knows. */
function parseRootFacts(raw: Record<string, unknown>): RootFacts {
  const facts: RootFacts = {};
  const origin = readString(raw, "rootOrigin");
  if (origin === "editor" || origin === "derivedVcsFence" || origin === "derivedTargetDir") {
    facts.rootOrigin = origin;
  }
  const fence = readString(raw, "rootFence");
  if (fence === "dir" || fence === "file" || fence === "symlink" || fence === "other") {
    facts.rootFence = fence;
  }
  const shadowed = readString(raw, "rootShadowed");
  if (shadowed) facts.rootShadowed = shadowed;
  const universe = readString(raw, "universe");
  if (universe === "open" || universe === "closed") facts.universe = universe;
  return facts;
}

export function parseSchemaInfo(raw: unknown): SchemaInfo | undefined {
  if (!isRecord(raw)) return undefined;
  const bound = readBoolean(raw, "bound");
  const notes = parseNotes(raw.notes) ?? [];
  const facts = parseRootFacts(raw);
  const layers = parseLayers(raw.layers);

  if (bound === false) {
    return { bound: false, notes, ...facts, ...(layers ? { layers } : {}) };
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
    ...facts,
    ...(layers ? { layers } : {}),
  };
}
