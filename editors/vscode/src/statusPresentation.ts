import { LayersInfo, RootFacts, SchemaInfo, formatHashShort } from "./contracts/schemaInfo";
import { ClientLifecycleState } from "./contracts/lifecycle";

export type { ClientLifecycleState } from "./contracts/lifecycle";

export type SchemaInfoLookup =
  | { kind: "ok"; info: SchemaInfo }
  | { kind: "error"; message: string }
  | { kind: "unavailable" }
  | { kind: "skipped" };

export interface StatusPresentation {
  text: string;
  tooltip: string;
  backgroundColorId: string | undefined;
  hidden: boolean;
}

/**
 * WHY this file is unbound, and WHAT TO DO — one answer per universe
 * state, because the two remedies are opposites and the bar used to give
 * the open one for both. Over a CLOSED universe a package manifest
 * already exists: committing another is not the fix, adding the file to
 * a `files` glob is. An older server sends no `universe`, and then the
 * tooltip says only what it knows rather than guessing.
 *
 * The verb is CLAIMS, which is the verb `nml binding` prints for the
 * same file (`no files glob claims this file`) and the one the kernel's
 * own vocabulary uses — one concept, one word across the two surfaces.
 * The sentence also does not promise there is exactly ONE manifest: a
 * closed universe can have several, and the wire carries only the two
 * words `open` and `closed`, so the tooltip says no more than it knows.
 */
function unboundReason(universe: SchemaInfo["universe"]): string[] {
  switch (universe) {
    case "closed":
      return [
        "This workspace root has a package manifest, so only the files a `files` glob claims are checked against a model — and no glob claims this one. Its syntax is still checked.",
        "Add this file to the `files` glob of the manifest that should own it.",
      ];
    case "open":
      return [
        "No package manifest was found in this workspace root, so nothing checks this file against a model. Its syntax is still checked.",
        "Commit a <name>.package.nml, or run your tool's `schema sync`.",
      ];
    default:
      return ["Commit a <name>.package.nml, or run your tool's `schema sync`."];
  }
}

const WARNING_BG = "statusBarItem.warningBackground";
const ERROR_BG = "statusBarItem.errorBackground";

/**
 * The item's colour for a resolved document: the error background when a
 * note is an error (the server refused the document, or the universe it
 * defines failed to load); the warning background when a note warns or a
 * manifest above the derived root is being ignored (`rootShadowed` — the
 * server logs that as a warning); else none.
 */
function backgroundFor(info: SchemaInfo): string | undefined {
  if (info.notes.some((n) => n.severity === "error")) return ERROR_BG;
  if (info.notes.some((n) => n.severity === "warning") || info.rootShadowed) return WARNING_BG;
  return undefined;
}

/**
 * How the document's root was fixed, in the words the CLI's
 * `note: workspace root …` line uses (`universe` is kept for the
 * open/closed world, as the CLI keeps it: one line, one sense) —
 * undefined for a workspace folder (nothing to disclose) and for a
 * server that sends no facts.
 */
export function describeRoot(facts: RootFacts): string | undefined {
  switch (facts.rootOrigin) {
    case "derivedVcsFence": {
      const fence =
        facts.rootFence === "file"
          ? "within a .git FILE fence — a linked worktree's, a submodule's or a planted entry"
          : "within the .git fence";
      const shadow = facts.rootShadowed
        ? `; shadowed by \`${facts.rootShadowed}\` above it`
        : "";
      return `derived ${fence}${shadow} — open the workspace folder to choose the root`;
    }
    case "derivedTargetDir":
      return "derived: no .git fence found, so the file's own directory is the workspace root — open the workspace folder to choose one";
    default:
      return undefined;
  }
}

/**
 * The binding's grant in `nml binding`'s words: `granted — allowRefs[0] =
 * "…", …`, or `none — composition denied (NML2064)`; undefined for a
 * server that sends no grant.
 */
export function describeLayers(layers: LayersInfo | undefined): string | undefined {
  if (!layers) return undefined;
  if (!layers.granted) return "none — composition denied (NML2064)";
  const rules = [
    ...(layers.allowRefs ?? []).map((g, i) => `allowRefs[${i}] = ${JSON.stringify(g)}`),
    ...(layers.denyRefs ?? []).map((g, i) => `denyRefs[${i}] = ${JSON.stringify(g)}`),
    ...(typeof layers.maxStackDepth === "number" ? [`maxStackDepth = ${layers.maxStackDepth}`] : []),
  ];
  return rules.length ? `granted — ${rules.join(", ")}` : "granted";
}

/** ` (derived …)` after a bound document's root, or nothing for a folder. */
function rootSuffix(facts: RootFacts): string {
  const described = describeRoot(facts);
  return described ? ` (${described})` : "";
}

/** The item is CLICKABLE, and its click restarts the language server
 *  (`statusBar.ts` sets `item.command = "nml.restartServer"`). A control
 *  whose action appears nowhere in its own tooltip is found by accident —
 *  the walkthrough said it once, at install time, and no state the operator
 *  reads later said it at all. Every tooltip that does not already name the
 *  restart command ends with this line; the three that do (failed,
 *  disconnected, absent) would only be saying it twice. */
const CLICK_NOTE = "Click here to restart the NML language server.";

/** Pure presentation logic — unit-tested without a VS Code host. */
export function buildStatusPresentation(
  lifecycle: ClientLifecycleState,
  serverLabel: string,
  schemaLookup: SchemaInfoLookup,
  hasActiveNmlEditor: boolean
): StatusPresentation {
  const presentation = statusPresentation(
    lifecycle,
    serverLabel,
    schemaLookup,
    hasActiveNmlEditor
  );
  if (presentation.hidden || presentation.tooltip.includes("NML: Restart Language Server")) {
    return presentation;
  }
  return { ...presentation, tooltip: `${presentation.tooltip}\n${CLICK_NOTE}` };
}

function statusPresentation(
  lifecycle: ClientLifecycleState,
  serverLabel: string,
  schemaLookup: SchemaInfoLookup,
  hasActiveNmlEditor: boolean
): StatusPresentation {
  if (!hasActiveNmlEditor) {
    return { text: "", tooltip: "", backgroundColorId: undefined, hidden: true };
  }

  if (lifecycle === "absent") {
    // Every other unhappy state names the way out; this one used to state a
    // fact and stop. It is the state the bar shows between one server being
    // retired and the next one starting, and the one it holds if nothing
    // takes that server's place — so it is precisely the state an operator
    // looks at when they want to know what to do next.
    return {
      text: "$(circle-slash) nml: no server",
      tooltip:
        "The NML language server is not running, so NML files are not being checked.\n" +
        "Run **NML: Restart Language Server** to start it, or **NML: Show Language Server Log** for what happened.",
      backgroundColorId: WARNING_BG,
      hidden: false,
    };
  }

  if (lifecycle === "starting") {
    return {
      text: "$(sync~spin) nml: starting…",
      tooltip: `Starting NML language server (${serverLabel || "resolving…"})`,
      backgroundColorId: undefined,
      hidden: false,
    };
  }

  if (lifecycle === "failed") {
    return {
      text: "$(error) nml: server failed",
      tooltip:
        `The NML language server failed to start (${serverLabel}).\n` +
        "Run **NML: Show Language Server Log** for details, or **NML: Restart Language Server**.",
      backgroundColorId: WARNING_BG,
      hidden: false,
    };
  }

  if (lifecycle === "disconnected") {
    return {
      text: "$(debug-disconnect) nml: disconnected",
      tooltip:
        `The NML language server connection closed unexpectedly.\n` +
        `Server: ${serverLabel}\n` +
        "Run **NML: Restart Language Server**, or **NML: Show Language Server Log** for details.",
      backgroundColorId: WARNING_BG,
      hidden: false,
    };
  }

  if (schemaLookup.kind === "error") {
    return {
      text: "$(warning) nml: schema lookup failed",
      tooltip: [
        `Could not fetch schema binding: ${schemaLookup.message}`,
        `Server: ${serverLabel}`,
      ].join("\n"),
      backgroundColorId: WARNING_BG,
      hidden: false,
    };
  }

  if (schemaLookup.kind === "unavailable" || schemaLookup.kind === "skipped") {
    return {
      text: "$(check) nml",
      tooltip: `Server: ${serverLabel}`,
      backgroundColorId: undefined,
      hidden: false,
    };
  }

  const schemaInfo = schemaLookup.info;

  if (!schemaInfo.bound) {
    const root = describeRoot(schemaInfo);
    const refusal = schemaInfo.notes.find((n) => n.severity === "error");
    if (refusal) {
      // Refused: the note is the whole report — why, and what to do.
      // "No schema package governs this file" would be false (two may
      // claim it) and "commit a manifest" the wrong remedy.
      return {
        text: "$(error) nml: not validated",
        tooltip: [
          `Not validated: ${refusal.message}`,
          ...(root ? [`Root: ${root}`] : []),
          `Server: ${serverLabel}`,
          ...schemaInfo.notes.filter((n) => n !== refusal).map((n) => n.message),
        ].join("\n"),
        backgroundColorId: ERROR_BG,
        hidden: false,
      };
    }
    return {
      text: "$(info) nml: no schema",
      tooltip: [
        "No schema package governs this file.",
        ...unboundReason(schemaInfo.universe),
        ...(root ? [`Root: ${root}`] : []),
        `Server: ${serverLabel}`,
        ...schemaInfo.notes.map((n) => n.message),
      ].join("\n"),
      backgroundColorId: backgroundFor(schemaInfo),
      hidden: false,
    };
  }

  const hashShort = formatHashShort(schemaInfo.contentHash);
  const lines = [
    `Schema: ${schemaInfo.package} ${schemaInfo.version}`,
    `Hash: ${hashShort} (${schemaInfo.contentHash})`,
    `Source: ${schemaInfo.source}`,
    `Binding: ${schemaInfo.binding} (${schemaInfo.step})`,
    `Root: ${schemaInfo.root}${rootSuffix(schemaInfo)}`,
    `Server: ${serverLabel}`,
  ];
  const layers = describeLayers(schemaInfo.layers);
  if (layers) {
    lines.splice(5, 0, `Layers: ${layers}`);
  }
  if (schemaInfo.shadowsStore) {
    lines.push("(workspace manifest shadows the store copy)");
  }
  if (schemaInfo.actions.includes("pin")) {
    lines.push("", "_Pin available via lightbulb (💡) menu_");
  }
  for (const note of schemaInfo.notes) {
    lines.push(note.message);
  }

  return {
    text: `$(check) nml: ${schemaInfo.package} ${schemaInfo.version}`,
    tooltip: lines.join("\n"),
    backgroundColorId: backgroundFor(schemaInfo),
    hidden: false,
  };
}
