import { SchemaInfo, formatHashShort } from "./contracts/schemaInfo";
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

const WARNING_BG = "statusBarItem.warningBackground";

/** Pure presentation logic — unit-tested without a VS Code host. */
export function buildStatusPresentation(
  lifecycle: ClientLifecycleState,
  serverLabel: string,
  schemaLookup: SchemaInfoLookup,
  hasActiveNmlEditor: boolean
): StatusPresentation {
  if (!hasActiveNmlEditor) {
    return { text: "", tooltip: "", backgroundColorId: undefined, hidden: true };
  }

  if (lifecycle === "absent") {
    return {
      text: "$(circle-slash) nml: no server",
      tooltip: "The NML language server is not running.",
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
    const noteLines = schemaInfo.notes.map((n) => n.message);
    const hasWarning = schemaInfo.notes.some((n) => n.severity === "warning");
    return {
      text: "$(info) nml: no schema",
      tooltip: [
        "No schema package governs this file.",
        `Server: ${serverLabel}`,
        "Commit a <name>.package.nml, or run your tool's `schema sync`.",
        ...noteLines,
      ].join("\n"),
      backgroundColorId: hasWarning ? WARNING_BG : undefined,
      hidden: false,
    };
  }

  const hashShort = formatHashShort(schemaInfo.contentHash);
  const lines = [
    `Schema: ${schemaInfo.package} ${schemaInfo.version}`,
    `Hash: ${hashShort} (${schemaInfo.contentHash})`,
    `Channel: ${schemaInfo.source}`,
    `Binding: ${schemaInfo.binding} (${schemaInfo.step})`,
    `Root: ${schemaInfo.root}`,
    `Server: ${serverLabel}`,
  ];
  if (schemaInfo.shadowsStore) {
    lines.push("(workspace manifest shadows the store copy)");
  }
  if (schemaInfo.actions.includes("pin")) {
    lines.push("", "_Pin available via lightbulb (💡) menu_");
  }
  for (const note of schemaInfo.notes) {
    lines.push(note.message);
  }

  const hasWarning = schemaInfo.notes.some((n) => n.severity === "warning");

  return {
    text: `$(check) nml: ${schemaInfo.package} ${schemaInfo.version}`,
    tooltip: lines.join("\n"),
    backgroundColorId: hasWarning ? WARNING_BG : undefined,
    hidden: false,
  };
}
