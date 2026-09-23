import { createHash } from "crypto";

// ─────────────────────────────────────────────────────────────────────────
// RFC 0035 — deciding whether to run a program a REPOSITORY named.
//
// THE THREAT MODEL this module is judged against. Everything below exists
// because of a line in this table; a mechanism that defends nothing in it is
// not a mechanism, it is a prompt the operator learns to click through.
//
//   A1  The repository author. Controls `nml-project.nml` (so the tool NAME),
//       every file in the workspace, and workspace-scoped settings. Does NOT
//       control the operator's `PATH`, home directory, machine-scoped
//       settings, or any binary outside the workspace.
//   A2  A later `git pull` into a repository the operator ALREADY approved.
//       Same powers as A1, but arrives after consent was given — so consent
//       must be pinned to what was consented TO, not to the folder.
//   A3  Someone who can write to a directory on the operator's `PATH`. They
//       already run code as the operator every time a shell runs `git`. The
//       extension cannot defend this; it must only avoid AMPLIFYING it —
//       a repository must never get to choose WHICH `PATH` entry wins, and a
//       directory other users can write to is not a place to find a server.
//   A4  Another local user on a shared machine. Reaches world-writable
//       directories (`/tmp`-style) and world-writable `PATH` entries.
//
//   ASSET  Code execution as the operator, and — secondarily — the operator's
//          source tree being read by a process they did not intend to run.
//
// WHAT FOLLOWS FROM IT, in order of when it happens:
//
//   BEFORE the program runs (the only real authorization gate):
//     · VS Code Workspace Trust (A1/A2 — an untrusted workspace never gets
//       here; `providerDiscovery` checks `workspace.isTrusted`).
//     · The tool name's grammar (`TOOL_NAME`), so the name cannot be a path,
//       an option, or a traversal.
//     · Absolute `PATH` entries only, and never a binary inside the
//       workspace (`pathSecurity`) — A1 cannot supply the binary.
//     · The resolution DIRECTORY's ownership and writability
//       ([`classifyResolutionDirectory`]) — A3/A4: a server found in a
//       directory other users can write to is refused outright.
//     · Informed consent, pinned to (declaration, resolved path)
//       ([`approvalDecision`]) — A1/A2.
//
//   WHEN it runs (blast radius):
//     · An empty private working directory, so the fixed `lsp` argument
//       resolves to nothing if the program turns out to be an interpreter
//       (`python lsp` runs a file named `lsp` from the working
//       directory), and so a Windows DLL search of the current
//       directory finds nothing to load.
//     · A scrubbed environment ([`scrubbedEnvOverlay`]) — the loader- and
//       interpreter-injection variables are REMOVED, not merely reset.
//
//   AFTER it runs (integrity, never authorization):
//     · The LSP 3.17 handshake: `serverInfo.name` must name an NML language
//       server ([`judgeServerIdentity`]). A handshake cannot authorize
//       anything — by the time it answers, the program has run — so its job
//       is to catch the case where the operator approved something that is
//       not a language server at all, stop it, and say so.
//
// WHAT THIS MODULE DELIBERATELY DOES **NOT** DO, and why:
//
//   It does not hash the resolved binary. A content hash of a `PATH` hit
//   identifies neither the tool nor its version on real machines: `rustup`'s
//   proxies give `cargo`, `rustc` and `rust-analyzer` one identical hash that
//   never changes when the toolchain behind them does; `asdf`/`mise`/`pyenv`
//   shims are stable wrappers in front of a moving target; Homebrew and npm
//   `PATH` hits are symlinks whose hash moves on every routine upgrade. The
//   same mechanism is therefore simultaneously vacuous and noisy. And against
//   A3 — the only attacker who can change those bytes — it buys nothing,
//   because that attacker already owns every other program the operator runs.
//   `direnv` reached the same conclusion: `direnv allow` hashes the FILE THAT
//   DECLARES, not the programs it goes on to run. So does `mise trust`, and
//   so does `git`'s `safe.directory`, and so does VS Code's own Workspace
//   Trust. This module pins the declaration and the path it resolved to.
// ─────────────────────────────────────────────────────────────────────────

/** `serverInfo.name` an NML language server answers `initialize` with
 *  (`crates/nml-lsp/src/server.rs` `SERVER_NAME`). Every schema-provider tool
 *  embeds `nml_lsp::serve`, so a provider answers with this name too: it
 *  identifies the PROTOCOL implementation, which is the only thing a client
 *  can meaningfully verify about a binary it was told to run. */
export const NML_SERVER_NAME = "nml-lsp";

/** Bumped when a stored approval's MEANING changes, so records written by an
 *  older extension are not silently honoured under new rules. Version 1 is
 *  the first record shape: earlier releases stored a bare command string
 *  under a per-tool key against a prompt that named neither the declaring
 *  file nor what would run, so those approvals are not carried forward — the
 *  operator is asked again, once, against the prompt below. */
export const APPROVAL_RECORD_VERSION = 1;

/** The `workspaceState` key holding this folder's provider decision. One
 *  record, because a workspace resolves at most one provider tool. */
export const APPROVAL_STATE_KEY = "nml.providerApproval.v1";

/** Key prefix of the superseded per-tool approvals, cleared on sight. */
export const LEGACY_APPROVAL_KEY_PREFIX = "nml.approvedProvider.";

// ── what a repository declared ───────────────────────────────────────────

/** One folder's provider declaration, as read from its `nml-project.nml`. */
export interface ProviderDeclarationSource {
  /** The workspace folder's name, as the operator sees it in the explorer. */
  readonly folder: string;
  /** The declaring file, relative to that folder. Always `nml-project.nml`
   *  today; carried explicitly so the prompt never has to assume it. */
  readonly file: string;
  /** The declaration's canonical text — `parseProviderBlock().canonical`. */
  readonly block: string;
}

/** Every declaration behind one resolution, plus the tool they agree on. */
export interface ProviderDeclaration {
  readonly tool: string;
  readonly sources: readonly ProviderDeclarationSource[];
}

/** The digest an approval is pinned to. Covers the tool, and every declaring
 *  folder's canonical block keyed by folder name, in a stable order — so a
 *  `git pull` that edits ANY of them (A2), or an added folder that declares a
 *  second one, produces a different digest and a fresh question. */
export function declarationDigest(declaration: ProviderDeclaration): string {
  // Length-prefixed fields, so no separator can be forged from inside one:
  // a folder literally named `x:1:y` cannot make two different declarations
  // hash alike. (A delimiter byte would have needed an escape in the source,
  // and a source file with control characters in it is its own problem.)
  //
  // UTF-16, and the length is the count of the units that are hashed.
  // `hash.update(string)` encodes as UTF-8, which is LOSSY for a JavaScript
  // string: a lone surrogate (one unit, and a legal `string`) becomes the
  // three bytes of U+FFFD, so `"\uD800"` and `"\uFFFD"` — different
  // declarations — hashed to the same digest, under a prefix counting units
  // that the encoder did not agree with. Writing the units themselves is
  // lossless for every `string` and makes the prefix count exactly what
  // follows it, which is the whole injectivity argument. (Unreachable from a
  // repository today — `TextDecoder` never yields a lone surrogate — but a
  // property this module rests on is made true, not argued around.)
  const hash = createHash("sha256");
  const field = (value: string): void => {
    const units = Buffer.from(value, "utf16le");
    hash.update(`${value.length}:`, "latin1");
    hash.update(units);
    hash.update(";", "latin1");
  };
  field(`nml-provider-declaration.v${APPROVAL_RECORD_VERSION}`);
  field(declaration.tool);
  const sources = [...declaration.sources].sort((a, b) =>
    a.folder === b.folder ? a.file.localeCompare(b.file) : a.folder.localeCompare(b.folder)
  );
  for (const source of sources) {
    field(source.folder);
    field(source.file);
    field(source.block);
  }
  return `sha256:${hash.digest("hex")}`;
}

// ── the approval record ──────────────────────────────────────────────────

export interface ApprovalRecord {
  readonly v: number;
  /** [`declarationDigest`] at the time the operator answered. */
  readonly digest: string;
  /** The absolute path the tool name resolved to at the time. */
  readonly command: string;
  readonly decision: "approved" | "declined";
}

export type ApprovalDecision =
  /** Run it: the operator already said yes to THIS declaration at THIS path. */
  | { readonly kind: "approved" }
  /** Use the built-in server without asking: they already said no to it. */
  | { readonly kind: "declined" }
  /** Ask. `because` is why the stored answer does not cover this launch. */
  | { readonly kind: "ask"; readonly because: AskReason };

export type AskReason =
  | "never-asked"
  | "declaration-changed"
  | "path-changed"
  | "record-superseded";

/** Does a stored answer cover this launch? An approval is a statement about
 *  one declaration resolving to one path; either half moving makes it a
 *  different question, exactly as a changed `.envrc` makes `direnv` ask
 *  again. A DECLINE is pinned the same way, so declining is not a permanent
 *  veto on a project that later declares something else. */
export function approvalDecision(
  record: ApprovalRecord | undefined,
  digest: string,
  command: string
): ApprovalDecision {
  if (!record) return { kind: "ask", because: "never-asked" };
  if (record.v !== APPROVAL_RECORD_VERSION) {
    return { kind: "ask", because: "record-superseded" };
  }
  if (record.digest !== digest) return { kind: "ask", because: "declaration-changed" };
  if (record.command !== command) return { kind: "ask", because: "path-changed" };
  return record.decision === "approved" ? { kind: "approved" } : { kind: "declined" };
}

// ── where the binary was found ───────────────────────────────────────────

/** POSIX mode bits + owner of the directory a tool resolved in. */
export interface DirectoryOwnership {
  readonly mode: number;
  readonly uid: number;
}

export type ResolutionDirectoryVerdict =
  /** Nothing to say: only the owner (or root) can put programs here. */
  | { readonly kind: "ok" }
  /** Runnable, but say so in the prompt: a wider set of accounts than the
   *  operator can replace programs here. Homebrew's `/opt/homebrew/bin` is
   *  `drwxrwxr-x` on a stock macOS install (MEASURED on the review host), so
   *  refusing this would refuse the most common way developers install
   *  tools — it is a fact for the operator to weigh, not a refusal. */
  | { readonly kind: "shared-group"; readonly mode: string }
  /** Refused: any local user can put a program here (A4), or the directory
   *  belongs to another account entirely (A3). */
  | {
      readonly kind: "refused";
      readonly reason: "world-writable" | "foreign-owner";
      readonly mode: string;
      readonly uid: number;
    };

/** Judge the directory a `PATH` lookup landed in.
 *
 *  This is the check a content hash cannot make and the one that matches the
 *  threat model: it asks WHO could have put the program there, which is the
 *  question A3/A4 turn on. `sudo`'s `secure_path`, OpenSSH's `StrictModes`
 *  and `git`'s `safe.directory` all reason about exactly this.
 *
 *  Windows has no POSIX mode bits and Node exposes no ACL reader, so there is
 *  nothing honest to check there: the caller passes `platform` and gets `ok`
 *  rather than a check that would be a lie. Said out loud rather than hidden
 *  behind a synthesized mode. */
export function classifyResolutionDirectory(
  ownership: DirectoryOwnership | undefined,
  selfUid: number,
  platform: string
): ResolutionDirectoryVerdict {
  if (platform === "win32" || !ownership) return { kind: "ok" };
  const mode = (ownership.mode & 0o7777).toString(8).padStart(4, "0");
  if ((ownership.mode & 0o002) !== 0) {
    return { kind: "refused", reason: "world-writable", mode, uid: ownership.uid };
  }
  // Root-owned system directories are the normal case for `/usr/bin`; an
  // owner that is neither root nor the operator is somebody else's directory
  // on the operator's `PATH`, which is A3 already in progress.
  if (ownership.uid !== selfUid && ownership.uid !== 0) {
    return { kind: "refused", reason: "foreign-owner", mode, uid: ownership.uid };
  }
  if ((ownership.mode & 0o020) !== 0) return { kind: "shared-group", mode };
  return { kind: "ok" };
}

// ── the environment the program inherits ─────────────────────────────────

/** Environment variables that make a program load or run code it was not
 *  asked to: the dynamic loaders' injection points and the interpreters'
 *  "run this first" hooks.
 *
 *  A DENYLIST, not an allowlist, and the choice is deliberate. An allowlist is
 *  the stronger construction (`sudo`'s `env_reset`, systemd's
 *  `PassEnvironment`, OpenSSH's `AcceptEnv` all use one) and is right when you
 *  own the program's configuration surface. We do not: a provider tool
 *  legitimately reads `HOME`, proxy settings, CA bundles, its own `NML_*`
 *  and `RUST_LOG`-shaped variables. An allowlist would break working setups
 *  in ways the operator cannot diagnose, so this list names the closed,
 *  well-known set the loaders themselves strip for set-user-ID binaries —
 *  glibc's own `unsecvars` (`elf/Makefile`) — plus the interpreters' and
 *  runtimes' documented "run this first" hooks, because the fixed `lsp`
 *  argument means a declared tool name may resolve to an interpreter and
 *  the program is then a SCRIPT, not an ELF binary.
 *
 *  `BASH_FUNC_` is the one that is not a path but a PROGRAM: an exported
 *  shell function arrives as `BASH_FUNC_<name>%%=() { … }`, bash defines it
 *  at startup, and every later call to `<name>` inside a `#!/bin/sh`-shaped
 *  tool runs the attacker's body instead of the command. MEASURED on this
 *  platform (`/bin/bash` 3.2.57, the post-Shellshock format).
 *
 *  Two names are deliberately NOT here. `PATH` is how the tool finds the
 *  programs it legitimately runs, and `TMPDIR` — which glibc does strip —
 *  is a working directory a tool needs; removing it silently relocates a
 *  tool's scratch files to the world-reachable `/tmp`, which is worse on
 *  the shared machine A4 describes. Both are the operator's own session's,
 *  and neither grants execution by itself.
 *
 *  Weight, honestly: the extension host's environment comes from the
 *  operator's session, which a repository does not control — so against A1
 *  this is defense in depth, not a gate. The vector that makes it non-zero is
 *  shell integration (`direnv`, `mise`) exporting a repository's own `.envrc`
 *  into the editor's environment, after which A1 does reach these names —
 *  including, with `export -f`, the `BASH_FUNC_` ones. */
export const SCRUBBED_ENV_PREFIXES: readonly string[] = [
  "BASH_FUNC_",
  "DYLD_",
  "LD_",
  // Lua's "run this first" and module-path variables are VERSIONED:
  // `LUA_INIT_5_4`, `LUA_PATH_5_3`, `LUA_CPATH_5_4` are each checked
  // before the unsuffixed name, so the three exact names this replaces
  // left every interpreter that ships a suffix reachable.
  "LUA_",
];

export const SCRUBBED_ENV_NAMES: readonly string[] = [
  // Shell startup and word-splitting: a tool that is a script reads these
  // before its own first line. `PS4` is expanded — command substitution
  // included — the moment anything runs `set -x`.
  "BASH_ENV",
  "BASHOPTS",
  "CDPATH",
  "ENV",
  "GLOBIGNORE",
  "IFS",
  "KSH_ENV",
  "PROMPT_COMMAND",
  "PS4",
  "SHELLOPTS",
  "ZDOTDIR",
  // glibc's `unsecvars`, minus the `LD_` family (a prefix above) and
  // minus `TMPDIR` (see the note on the prefixes).
  "GCONV_PATH",
  "GETCONF_DIR",
  "GLIBC_TUNABLES",
  "HOSTALIASES",
  "LOCALDOMAIN",
  "LOCPATH",
  "MALLOC_TRACE",
  "NIS_PATH",
  "NLSPATH",
  "RESOLV_HOST_CONF",
  "RES_OPTIONS",
  "TZDIR",
  // git, when the tool shells out to it: each of these names a PROGRAM git
  // will run, or a config file it will obey.
  "GIT_ALTERNATE_OBJECT_DIRECTORIES",
  "GIT_ASKPASS",
  "GIT_CONFIG",
  "GIT_CONFIG_COUNT",
  "GIT_CONFIG_GLOBAL",
  "GIT_CONFIG_SYSTEM",
  // A repository DIRECTORY is a config file: `<GIT_DIR>/config` is
  // git's local configuration, and it names programs
  // (`core.fsmonitor`, `core.sshCommand`, `core.pager`,
  // `diff.external`, `filter.*.clean`). Scrubbing `GIT_CONFIG*` and
  // leaving these two was the same fence with a gate in it.
  "GIT_COMMON_DIR",
  "GIT_DIR",
  "GIT_EDITOR",
  "GIT_EXTERNAL_DIFF",
  "GIT_PAGER",
  "GIT_PROXY_COMMAND",
  "GIT_SSH",
  "GIT_SSH_COMMAND",
  // The hooks `git init` and `git clone` copy into a new repository —
  // programs git runs from then on.
  "GIT_TEMPLATE_DIR",
  // Node, including the one that turns an Electron binary into an
  // interpreter.
  "ELECTRON_RUN_AS_NODE",
  "NODE_OPTIONS",
  "NODE_PATH",
  "NODE_REPL_EXTERNAL_MODULE",
  // The JVM and .NET: each loads and runs code named by the variable.
  "CLASSPATH",
  "DOTNET_STARTUP_HOOKS",
  "JAVA_TOOL_OPTIONS",
  "JDK_JAVA_OPTIONS",
  "_JAVA_OPTIONS",
  // OpenSSL: the configuration file NAMES native modules to load —
  // `[engine] dynamic_path = …` (1.x) and `[provider_sect] module = …`
  // (3.x) — so pointing a tool at a config is the same primitive as
  // `LD_PRELOAD`, and the two directory variables are where those
  // modules are looked up.
  "OPENSSL_CONF",
  "OPENSSL_ENGINES",
  "OPENSSL_MODULES",
  // Perl, Python, Ruby, R, Julia (Lua is a prefix above).
  "JULIA_LOAD_PATH",
  "PERL5DB",
  "PERL5LIB",
  "PERL5OPT",
  "PERLLIB",
  "PYTHONBREAKPOINT",
  "PYTHONEXECUTABLE",
  "PYTHONHOME",
  "PYTHONINSPECT",
  "PYTHONPATH",
  "PYTHONSTARTUP",
  "PYTHONUSERBASE",
  "PYTHONWARNINGS",
  "GEM_HOME",
  "GEM_PATH",
  "RUBYLIB",
  "RUBYOPT",
  "RUBYPATH",
  "R_PROFILE",
  "R_PROFILE_USER",
  // The Rust toolchain's compiler wrappers.
  "RUSTC",
  "RUSTC_WORKSPACE_WRAPPER",
  "RUSTC_WRAPPER",
];

/** Windows environment variables are CASE-INSENSITIVE: a process that reads
 *  `NODE_OPTIONS` reads it out of a block that spells it `Node_Options`, and
 *  `Object.keys(process.env)` hands back the spelling as stored. An
 *  exact-bytes denylist therefore removes `NODE_OPTIONS` and leaves
 *  `Node_Options` in place for the same reader — which is the whole control
 *  bypassed by a change of case. Every name and prefix below is upper case,
 *  so upper-casing the candidate on Windows is the comparison that platform's
 *  own lookup performs. POSIX stays exact, because there the two names really
 *  are two variables. */
export function isScrubbedEnvName(name: string, platform: string = process.platform): boolean {
  const compared = platform === "win32" ? name.toUpperCase() : name;
  return (
    SCRUBBED_ENV_NAMES.includes(compared) ||
    SCRUBBED_ENV_PREFIXES.some((prefix) => compared.startsWith(prefix))
  );
}

/** The overlay handed to the spawn.
 *
 *  `undefined`, not `""`. `vscode-languageclient` builds the child's
 *  environment by copying ALL of `process.env` and then overlaying this
 *  object (`node/main.js` `getEnvironment`), so a name that is merely absent
 *  here is still INHERITED — omitting is not removing. Node's `spawn` drops
 *  keys whose value is `undefined` and keeps keys whose value is `""`, so
 *  `undefined` is the spelling that actually removes the variable — measured
 *  against the language client, and pinned by the spawn test in
 *  `test/unit/clientManager.test.ts`. */
export function scrubbedEnvOverlay(
  env: Readonly<Record<string, string | undefined>>,
  platform: string = process.platform
): Record<string, undefined> {
  const overlay: Record<string, undefined> = {};
  for (const name of Object.keys(env)) {
    if (isScrubbedEnvName(name, platform)) overlay[name] = undefined;
  }
  return overlay;
}

// ── the handshake ────────────────────────────────────────────────────────

export type HandshakeOutcome =
  | { readonly kind: "identified"; readonly version: string | undefined }
  /** A DEFINITE negative: something answered with someone else's name. */
  | { readonly kind: "repudiated"; readonly saw: string }
  /** An LSP server answered and named NOTHING. Every provider built against
   *  an NML from before the server sent `serverInfo` answers this way, so it
   *  is evidence of an out-of-date build, not of an impostor: the program is
   *  stopped, and the operator's approval is left alone. */
  | { readonly kind: "unidentified" }
  /** No answer inside the budget. Says nothing about what the program IS —
   *  a loaded machine looks exactly like this — so it must not be treated as
   *  evidence against the operator's approval. */
  | { readonly kind: "indefinite" };

/** Judge `initialize`'s `serverInfo` (LSP 3.17 §initialize).
 *
 *  The field is optional in the specification, but a client that requires
 *  it of the servers IT chose to launch is making a stricter, well-defined
 *  contract, and `nml-lsp` sends it on both flavors (pinned in
 *  `crates/nml-lsp/tests/harness.rs`). A program that names nothing is
 *  therefore not accepted — but it is not ACCUSED either: only a name that
 *  is someone else's is a definite negative. The check runs after the
 *  spawn and a hostile program can answer any name it likes, so treating
 *  silence as hostility would buy no security and would cost every
 *  operator whose tool predates the field their approval, again, on every
 *  window reload. */
export function judgeServerIdentity(
  serverInfo: { name?: string; version?: string } | undefined,
  expect: string = NML_SERVER_NAME
): HandshakeOutcome {
  const name = serverInfo?.name;
  if (name === expect) {
    return { kind: "identified", version: serverInfo?.version };
  }
  if (name === undefined || name === "") {
    return { kind: "unidentified" };
  }
  return { kind: "repudiated", saw: name };
}

// ── what the operator is told ────────────────────────────────────────────

export interface ProviderConsentRequest {
  readonly tool: string;
  /** The absolute path the tool name resolved to. */
  readonly command: string;
  /** The fixed arguments (`["lsp"]`). */
  readonly args: readonly string[];
  readonly sources: readonly ProviderDeclarationSource[];
  /** A note about the resolution directory, when there is one to make. */
  readonly directory: ResolutionDirectoryVerdict;
}

export interface ProviderConsentPrompt {
  readonly message: string;
  readonly detail: string;
  readonly accept: string;
  readonly decline: string;
}

/** The consent dialog's words.
 *
 *  Pure, so the wording is unit-tested rather than discovered in a release.
 *  The rules it follows, in the order they bind:
 *
 *  · Informed consent means the operator can SEE what they are agreeing to:
 *    the file that asked, the exact program that will run, and that it runs
 *    with their permissions. Progressive disclosure — one question in the
 *    message, the specifics in the detail — rather than a wall of text or a
 *    one-liner that hides the path.
 *  · Both outcomes are stated, so declining is a choice and not a cliff:
 *    NML editing keeps working either way.
 *  · Consent that is remembered says so, and says how to take it back, in
 *    the same breath. Revoking must be as easy as granting.
 *  · No urgency, no blame, no jargon, and no reassurance the extension
 *    cannot give: it does not say the program is safe, because it does not
 *    know. It says who vouched for it (this repository) and what it can do.
 *  · The buttons name their action rather than answering a yes/no the
 *    operator has to re-read the message to parse. */
export function providerConsentPrompt(
  request: ProviderConsentRequest
): ProviderConsentPrompt {
  const commandLine = [request.command, ...request.args].join(" ");
  const asked = request.sources
    .map((s) => `${s.file} (in the folder "${s.folder}")`)
    .join("\n           ");
  const lines = [
    `This project asks the editor to use a program called "${request.tool}"` +
      " for NML editing, instead of the built-in NML server.",
    "",
    `Will run:  ${commandLine}`,
    `Asked by:  ${asked}`,
    "",
    `"${request.tool}" runs with your account's permissions and can read and` +
      " change your files. Accept only if you trust this project and you" +
      " recognise the path above.",
  ];
  if (request.directory.kind === "shared-group") {
    lines.push(
      "",
      `Note: ${directoryOf(request.command)} is group-writable` +
        ` (mode ${request.directory.mode}), so other accounts in its group can` +
        " replace programs there."
    );
  }
  lines.push(
    "",
    "If you accept, the editor remembers it for this workspace until" +
      " nml-project.nml or that path changes. To take it back at any time," +
      ' run "NML: Forget Language Server Approvals".',
    "",
    "If you decline, NML editing keeps working: the editor uses its own" +
      " built-in server, which needs no setup.",
  );
  return {
    message: `Run "${request.tool}" as this project's NML language server?`,
    detail: lines.join("\n"),
    accept: `Use "${request.tool}"`,
    decline: "Keep the built-in server",
  };
}

/** What the operator is told when a program they approved answered the
 *  handshake with SOMEONE ELSE'S name: what stopped, what it claimed to be,
 *  what the extension did about the approval, and that editing still works.
 *  It does not say the program is unsafe, and it does not offer the
 *  out-of-date build's remedy — this is not an out-of-date build. */
export function repudiatedMessage(tool: string, saw: string): string {
  return (
    `NML stopped "${tool}": it answered the editor's handshake as ` +
    `"${saw}", not as an NML language server, so the editor cannot ` +
    "trust it with your files. Its approval has been removed — you will be " +
    "asked again next time — and NML editing has switched to the built-in " +
    "server."
  );
}

/** What the operator is told when the program answered and named NOTHING.
 *
 *  THE case every provider binary built before the server started sending
 *  `serverInfo` lands in: an honest NML server the editor cannot recognise.
 *  So the sentence names that cause and the one action that fixes it, says
 *  the approval stands (no second prompt), and accuses nobody.
 *
 *  It also names WHEN the rebuild takes effect, because the answer is not
 *  the obvious one: this outcome stands the provider down for the session
 *  ([`stoodDownMessage`]), so the rebuilt tool is picked up by the next
 *  WINDOW — restarting the language server inside this one re-resolves
 *  straight past it. A remedy whose timing is left out is a remedy the
 *  operator tries, sees nothing from, and stops believing. */
export function unidentifiedMessage(tool: string): string {
  return (
    `NML stopped "${tool}": it answered the editor's handshake without ` +
    "naming itself, so the editor cannot tell what it is, and NML editing " +
    "has switched to the built-in server for this session. Its approval is " +
    `unchanged. If "${tool}" is this project's own tool, it was most likely ` +
    "built against an NML older than the one that added the handshake: " +
    "rebuild it against a current nml-lsp, then reload the window — it will " +
    "name itself and the editor will use it again."
  );
}

/** Why a declared provider was not even considered — the rungs of the ladder
 *  ABOVE consent, each of which used to return the built-in server in total
 *  silence.
 *
 *  A project that declares a tool and does not get it is a fact about the
 *  session the operator can otherwise learn NOWHERE: not from the status bar
 *  (which names the server that IS running, correctly), not from a prompt
 *  (there is none — that is the point), and not from the log. "Invisible
 *  action" is the anti-pattern; the decline rung (`declinedMessage`) was
 *  taken off it, and these are the rest of the same ladder. */
export type ProviderSkip =
  /** Two folders name two different tools; there is no single answer, and
   *  guessing one of them is the wrong kind of helpful. */
  | { readonly kind: "conflicting"; readonly tools: readonly string[] }
  /** VS Code Workspace Trust is the outer gate: an untrusted workspace never
   *  runs a program its own files named. */
  | { readonly kind: "untrusted" }
  /** Nothing of that name is executable on an absolute `PATH` entry. */
  | { readonly kind: "not-on-path" }
  /** It resolved INSIDE the workspace — the one place a repository could have
   *  supplied the binary itself, which is what the whole gate is for. */
  | { readonly kind: "inside-workspace"; readonly command: string };

/** What the LOG says when a declared provider never reaches the question.
 *
 *  Each sentence names what is running instead (so the operator is not left
 *  wondering whether NML editing works — it does), what happened, and the one
 *  thing that would change it. */
export function skippedProviderMessage(tool: string, skip: ProviderSkip): string {
  const lead = "Using the built-in NML server: ";
  switch (skip.kind) {
    case "conflicting":
      return (
        `${lead}this workspace's folders declare different language servers ` +
        `(${[...skip.tools].sort().map((t) => `"${t}"`).join(", ")}), so none ` +
        "of them is used. Make the nml-project.nml files agree on one tool, " +
        "or open one folder at a time."
      );
    case "untrusted":
      return (
        `${lead}this workspace is not trusted, so the language server it ` +
        `declares ("${tool}") was not run. Trust the workspace to be asked ` +
        "about it. Committed schema is still checked either way."
      );
    case "not-on-path":
      return (
        `${lead}this project declares the language server "${tool}", and no ` +
        `program called "${tool}" is on PATH. Install it and reload the ` +
        "window, or remove the provider declaration from nml-project.nml."
      );
    case "inside-workspace":
      return (
        `${lead}this project declares the language server "${tool}", and the ` +
        `name resolves to ${skip.command} — inside this workspace. A ` +
        "repository does not get to supply the program the editor runs, so it " +
        "is not used. Install it outside the workspace."
      );
  }
}

/** What the LOG says when a stored decline is honoured.
 *
 *  Without it, declining once made the project's tool disappear in
 *  silence: no prompt, no note, and the only way back — a command in the
 *  palette — named in a modal the operator dismissed months ago. A
 *  remembered answer is still an answer the operator is entitled to see,
 *  and revocation has to be as reachable as consent was. */
export function declinedMessage(tool: string): string {
  return (
    `Using the built-in NML server: you declined "${tool}" for this ` +
    'workspace. Run "NML: Forget Language Server Approvals" to be asked ' +
    "again."
  );
}

/** What the LOG says when a provider was stood down earlier in the
 *  session (a handshake unanswered, or answered without a name), so the
 *  ladder does not try it
 *  again until the window is reloaded. */
export function stoodDownMessage(tool: string): string {
  return (
    `Using the built-in NML server: "${tool}" was stood down earlier in ` +
    "this session. Reload the window to try it again."
  );
}

/** What the LOG says before the prompt goes up — which question is being
 *  asked, about what, and WHY it is being asked again. */
export function askingMessage(tool: string, command: string, because: AskReason): string {
  const why: Record<AskReason, string> = {
    "never-asked": "this workspace has not been asked about it",
    "declaration-changed": "nml-project.nml's provider declaration changed",
    "path-changed": "the name now resolves somewhere else",
    "record-superseded": "the stored answer predates this approval format",
  };
  return (
    `Asking about the project's language server "${tool}" (${command}): ` +
    `${why[because]}.`
  );
}

/** What the operator is told when the program never answered. Deliberately
 *  does NOT accuse it: a busy machine looks the same, and the approval is
 *  left alone.
 *
 *  The remedy is the WINDOW, not the language server. This outcome stands the
 *  provider down for the session, and a stood-down provider is skipped by the
 *  next resolution ([`stoodDownMessage`]) — so *NML: Restart Language Server*
 *  silently comes back with the built-in server, which is exactly the
 *  "nothing happened" an operator learns to distrust the editor for. The
 *  sentence therefore names the one action that does what it says. */
export function indefiniteMessage(tool: string, budgetMs: number): string {
  return (
    `NML: "${tool}" did not finish starting within ${Math.round(budgetMs / 1000)} ` +
    "seconds, so NML editing switched to the built-in server for the rest of " +
    "this session. Its approval is unchanged; reload the window to try it again."
  );
}

/** What the log says when the resolution directory is refused. A refusal the
 *  operator never asked for still has to be explainable, so it names the
 *  directory, the mode, and the one thing they can do about it.
 *
 *  It opens the way every OTHER rung of this ladder opens
 *  ([`skippedProviderMessage`], [`declinedMessage`], [`stoodDownMessage`]):
 *  the reader's first question is "is NML editing still working?", the answer
 *  is yes on every one of them, and one lead means the whole class is one
 *  search of the log rather than seven sentences to recognise. */
export function refusedDirectoryMessage(
  tool: string,
  command: string,
  verdict: Extract<ResolutionDirectoryVerdict, { kind: "refused" }>
): string {
  const why =
    verdict.reason === "world-writable"
      ? `any user on this machine can replace programs in ${directoryOf(command)}` +
        ` (mode ${verdict.mode})`
      : `${directoryOf(command)} belongs to another account (uid ${verdict.uid})`;
  return (
    `Using the built-in NML server: not running the project's "${tool}" ` +
    `(${command}) because ${why}, so a program found there is not the one ` +
    "this project meant. Install the tool in a directory no other account " +
    "can write to (~/.cargo/bin, for example) and reload the window. " +
    `nml.server.path cannot stand in for it: that setting runs a neutral ` +
    `nml-lsp with no arguments, never this project's "${tool} lsp".`
  );
}

function directoryOf(command: string): string {
  const cut = Math.max(command.lastIndexOf("/"), command.lastIndexOf("\\"));
  return cut > 0 ? command.slice(0, cut) : command;
}
