import { constants as fsConstants, promises as fsp } from "fs";
import * as os from "os";
import * as path from "path";
import { ExtensionContext, FileType, Memento, Uri, window, workspace } from "vscode";
import { isValidToolName, parseProviderBlock } from "./contracts/providerProject";
import { LaunchSandbox, isPathInsideWorkspace } from "./pathSecurity";
import { NmlLogs } from "./logging";
import {
  APPROVAL_STATE_KEY,
  APPROVAL_RECORD_VERSION,
  ApprovalRecord,
  LEGACY_APPROVAL_KEY_PREFIX,
  NML_SERVER_NAME,
  ProviderDeclaration,
  ProviderDeclarationSource,
  ProviderSkip,
  approvalDecision,
  askingMessage,
  classifyResolutionDirectory,
  declarationDigest,
  declinedMessage,
  providerConsentPrompt,
  refusedDirectoryMessage,
  skippedProviderMessage,
  stoodDownMessage,
} from "./providerTrust";
import { resolveNeutralServer } from "./serverAcquisition";
import { ServerResolution, processServer } from "./serverResolution";

/** The file a project declares its provider in (RFC 0035). */
const PROJECT_FILE = "nml-project.nml";

/** The bound the extension reads that file under — the KERNEL's, for the
 *  same file (`nml_validate::fs::MAX_MANIFEST_BYTES`, published as
 *  `nml limits`' 256 KiB row for a package manifest or project config).
 *
 *  The extension reads `nml-project.nml` itself, before the language
 *  server exists, to learn which server to run — and it read it with no
 *  bound at all while the kernel refused the same file past this one. A
 *  repository is what writes it: `workspace.fs.readFile` returns the
 *  whole thing, so a large file is that many bytes of the EXTENSION
 *  HOST's heap, and a `nml-project.nml` that is a symlink to `/dev/zero`
 *  is unbounded (MEASURED on this platform's Node: 7.5 GB resident in
 *  8 s, and never returning; a FIFO blocks forever). That read happens
 *  BEFORE the Workspace Trust gate, because an untrusted workspace still
 *  gets told which tool it declared and why it was not run — so the
 *  bound, not the gate, is what makes it safe. */
export const MAX_PROJECT_CONFIG_BYTES = 256 * 1024;

/** Approvals withdrawn or stood down THIS session, keyed by
 *  `${digest}|${command}` — the same identity an approval is pinned to.
 *
 *  Session state, not persisted: it exists so that the fallback after a
 *  failed handshake cannot resolve straight back into the launch that just
 *  failed and prompt again in a loop. A window reload clears it, which is
 *  right — the operator asked for a fresh start. */
const sessionStandDowns = new Set<string>();

/** Test seam: drop this session's stand-downs. */
export function clearSessionStandDowns(): void {
  sessionStandDowns.clear();
}

function standDownKey(digest: string, command: string): string {
  return `${digest}|${command}`;
}

/** Every workspace folder that declares a provider, and the tool they agree
 *  on. Folders that disagree resolve to nothing: a workspace with two
 *  different declared tools has no single answer, and guessing one of them
 *  is the wrong kind of helpful.
 *
 *  The DISAGREEMENT is returned rather than swallowed. "Nothing was declared"
 *  and "two folders declared different things" are the same `undefined` to
 *  the ladder and opposite facts to the operator: the second is a project
 *  that asked for something and did not get it, which is exactly what has to
 *  reach the log. */
async function declaredProvider(
  logs: NmlLogs
): Promise<ProviderDeclaration | ProviderSkip | undefined> {
  const folders = workspace.workspaceFolders ?? [];
  const sources: ProviderDeclarationSource[] = [];
  const tools = new Set<string>();
  for (const folder of folders) {
    const uri = Uri.joinPath(folder.uri, PROJECT_FILE);
    let text: string;
    try {
      // A regular file, under the kernel's own bound for it. Both halves
      // are the read's, not the reader's: `FileType.File` is off for a
      // directory, a FIFO, a socket and a device (those are `Unknown`),
      // and the size is checked BEFORE the read so an oversized file is
      // never held. The byte length is checked again afterwards because a
      // file can grow between the two calls.
      const stat = await workspace.fs.stat(uri);
      if ((stat.type & FileType.File) === 0) {
        logs.warn(refusedProjectFileMessage(folder.name, "it is not a regular file"));
        continue;
      }
      if (stat.size > MAX_PROJECT_CONFIG_BYTES) {
        logs.warn(refusedProjectFileMessage(folder.name, `it is larger than ${MAX_PROJECT_CONFIG_BYTES} bytes`));
        continue;
      }
      const bytes = await workspace.fs.readFile(uri);
      if (bytes.byteLength > MAX_PROJECT_CONFIG_BYTES) {
        logs.warn(refusedProjectFileMessage(folder.name, `it is larger than ${MAX_PROJECT_CONFIG_BYTES} bytes`));
        continue;
      }
      text = new TextDecoder().decode(bytes);
    } catch {
      continue;
    }
    const block = parseProviderBlock(text);
    if (!block?.tool || !isValidToolName(block.tool)) continue;
    tools.add(block.tool);
    sources.push({ folder: folder.name, file: PROJECT_FILE, block: block.canonical });
  }
  if (tools.size > 1) return { kind: "conflicting", tools: [...tools] };
  if (tools.size === 0) return undefined;
  return { tool: [...tools][0], sources };
}

/** The first executable `<dir>/<tool>[ext]` on `PATH`, ABSOLUTE entries
 *  only: a relative entry (`.`, an empty segment) would resolve against the
 *  extension host's working directory and spawn a relative command — the
 *  cwd-dependent launch `nml.server.path` refuses for the same reason. */
export async function resolveOnPath(tool: string): Promise<string | undefined> {
  const exts =
    process.platform === "win32"
      ? (process.env.PATHEXT ?? ".EXE;.CMD;.BAT").split(";")
      : [""];
  for (const dir of (process.env.PATH ?? "").split(path.delimiter)) {
    if (!dir || !path.isAbsolute(dir)) continue;
    for (const ext of exts) {
      const candidate = path.join(dir, tool + ext);
      try {
        // `access(X_OK)` on a DIRECTORY succeeds — the execute bit is the
        // search bit there — so a directory named like the tool would end the
        // search on something that can never be spawned, hiding the real
        // program later on `PATH` behind a consent prompt for a launch that
        // can only fail. The hit has to be a file (`stat`, so a symlink to
        // one counts).
        await fsp.access(candidate, fsConstants.X_OK);
        if (!(await fsp.stat(candidate)).isFile()) continue;
        return candidate;
      } catch {
        /* keep looking */
      }
    }
  }
  return undefined;
}

/** What the LOG says when a folder's `nml-project.nml` is not read at
 *  all. A file the extension refuses is a file whose `provider:` block
 *  does nothing, which the operator can learn nowhere else — the same
 *  rule every other rung of this ladder follows. */
function refusedProjectFileMessage(folder: string, why: string): string {
  return (
    `Not reading ${PROJECT_FILE} in the folder "${folder}": ${why}. ` +
    "Any language server it declares is not used; NML editing keeps working " +
    "with the built-in server."
  );
}

function workspaceRoots(): string[] {
  return (workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath);
}

/** Who can put a program in the directory the tool resolved in — the check
 *  that a content hash of the binary cannot make, and the one the threat
 *  model actually turns on (`providerTrust`, A3/A4). */
async function resolutionDirectoryVerdict(command: string) {
  let ownership: { mode: number; uid: number } | undefined;
  try {
    const st = await fsp.stat(path.dirname(command));
    ownership = { mode: st.mode, uid: st.uid };
  } catch {
    ownership = undefined;
  }
  const selfUid = typeof os.userInfo === "function" ? safeUid() : -1;
  return classifyResolutionDirectory(ownership, selfUid, process.platform);
}

function safeUid(): number {
  try {
    return os.userInfo().uid;
  } catch {
    return -1;
  }
}

/** Read this workspace's stored decision, and retire any approval written by
 *  a superseded record shape while we are here. The old records were granted
 *  against a prompt that named neither the declaring file nor the command
 *  line, so they are not evidence of consent to what is asked now: they are
 *  removed rather than migrated, and the operator is asked once. */
function readApproval(state: Memento): ApprovalRecord | undefined {
  const record = state.get<ApprovalRecord>(APPROVAL_STATE_KEY);
  if (!record || record.v !== APPROVAL_RECORD_VERSION) return undefined;
  return record;
}

/** Drop every stored provider decision for this workspace, current and
 *  superseded. The revoke half of "approval is remembered". */
export async function forgetProviderApprovals(context: ExtensionContext): Promise<number> {
  const state = context.workspaceState;
  const keys = state
    .keys()
    .filter((k) => k === APPROVAL_STATE_KEY || k.startsWith(LEGACY_APPROVAL_KEY_PREFIX));
  for (const key of keys) await state.update(key, undefined);
  clearSessionStandDowns();
  return keys.length;
}

/** The discovery ladder (RFC 0035), with its consent gate.
 *
 *  Order matters and is the design: every test that can be made BEFORE the
 *  program runs is made before it runs, because a handshake cannot authorize
 *  anything it has already executed. See `providerTrust.ts` for the threat
 *  model each rung answers. */
export async function resolveServer(
  context: ExtensionContext,
  logs: NmlLogs,
  sandbox: LaunchSandbox
): Promise<ServerResolution> {
  const neutral = (): Promise<ServerResolution> =>
    resolveNeutralServer(context, logs, sandbox);

  // Every rung below SAYS what it did. A project that declares a tool and
  // does not get it is a fact about this session the operator can learn
  // nowhere else: the status bar names the server that IS running (correctly,
  // and that is the whole problem), and there is no prompt, because not
  // reaching the prompt is what happened.
  const declared = await declaredProvider(logs);
  if (!declared) return neutral();
  if (!("tool" in declared)) {
    logs.info(skippedProviderMessage("", declared));
    return neutral();
  }
  const declaration = declared;
  const say = (skip: ProviderSkip): void => {
    logs.info(skippedProviderMessage(declaration.tool, skip));
  };

  // Workspace Trust is the outer gate: an untrusted workspace never runs a
  // program its own files named.
  if (!workspace.isTrusted) {
    say({ kind: "untrusted" });
    return neutral();
  }

  const command = await resolveOnPath(declaration.tool);
  if (!command) {
    say({ kind: "not-on-path" });
    return neutral();
  }
  if (await isPathInsideWorkspace(command, workspaceRoots())) {
    say({ kind: "inside-workspace", command });
    return neutral();
  }

  const directory = await resolutionDirectoryVerdict(command);
  if (directory.kind === "refused") {
    logs.warn(refusedDirectoryMessage(declaration.tool, command, directory));
    return neutral();
  }

  const digest = declarationDigest(declaration);
  if (sessionStandDowns.has(standDownKey(digest, command))) {
    logs.info(stoodDownMessage(declaration.tool));
    return neutral();
  }

  const decision = approvalDecision(readApproval(context.workspaceState), digest, command);
  // A remembered decline is SAID, not merely obeyed: the project declared a
  // tool and the editor is not using it, which is a fact about this session
  // the operator can otherwise learn nowhere.
  if (decision.kind === "declined") {
    logs.info(declinedMessage(declaration.tool));
    return neutral();
  }
  if (decision.kind === "ask") {
    logs.info(askingMessage(declaration.tool, command, decision.because));
    const prompt = providerConsentPrompt({
      tool: declaration.tool,
      command,
      args: ["lsp"],
      sources: declaration.sources,
      directory,
    });
    // Modal, because this is a consent decision with code execution behind
    // it and a modal is the only VS Code message that renders `detail` —
    // the operator cannot be informed by a one-line toast that truncates
    // the path. VS Code's own Workspace Trust makes the same call. Cancel
    // (the modal's implicit third answer) is "not now": nothing is stored,
    // so the question comes back next time rather than becoming a silent no.
    const choice = await window.showInformationMessage(
      prompt.message,
      { modal: true, detail: prompt.detail },
      prompt.accept,
      prompt.decline
    );
    if (choice !== prompt.accept) {
      if (choice === prompt.decline) {
        await context.workspaceState.update(APPROVAL_STATE_KEY, {
          v: APPROVAL_RECORD_VERSION,
          digest,
          command,
          decision: "declined",
        } satisfies ApprovalRecord);
      }
      return neutral();
    }
    // The verdict above was taken BEFORE the modal, and a modal is human
    // time — seconds to minutes, during which the directory the program
    // resolved in can be made world-writable, or the tool replaced by one a
    // `PATH` entry's new owner put there. Consent is consent to what was
    // shown; the launch has to be about what is true NOW, so the one check
    // that reads the filesystem is taken again and a downgrade refuses.
    const afterConsent = await resolutionDirectoryVerdict(command);
    if (afterConsent.kind === "refused") {
      logs.warn(refusedDirectoryMessage(declaration.tool, command, afterConsent));
      return neutral();
    }
    await context.workspaceState.update(APPROVAL_STATE_KEY, {
      v: APPROVAL_RECORD_VERSION,
      digest,
      command,
      decision: "approved",
    } satisfies ApprovalRecord);
  }

  return processServer(command, ["lsp"], `${declaration.tool} (in-binary)`, "provider", sandbox, {
    expect: NML_SERVER_NAME,
    tool: declaration.tool,
    repudiate: async () => {
      sessionStandDowns.add(standDownKey(digest, command));
      await context.workspaceState.update(APPROVAL_STATE_KEY, undefined);
    },
    standDown: async () => {
      sessionStandDowns.add(standDownKey(digest, command));
    },
  });
}
