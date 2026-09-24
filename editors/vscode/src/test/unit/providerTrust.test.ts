// No `vscode` stub needed: providerTrust is deliberately pure — the trust
// algebra and every word the operator reads are decided here, away from the
// extension host, so both are unit-testable and neither can drift silently.

import * as assert from "node:assert";
import { parseProviderBlock } from "../../contracts/providerProject";
import {
  APPROVAL_RECORD_VERSION,
  ApprovalRecord,
  NML_SERVER_NAME,
  ProviderDeclaration,
  approvalDecision,
  askingMessage,
  classifyResolutionDirectory,
  declarationDigest,
  declinedMessage,
  indefiniteMessage,
  isScrubbedEnvName,
  judgeServerIdentity,
  providerConsentPrompt,
  refusedDirectoryMessage,
  repudiatedMessage,
  unidentifiedMessage,
  scrubbedEnvOverlay,
  skippedProviderMessage,
  stoodDownMessage,
} from "../../providerTrust";
import { INITIALIZE_BUDGET_MS } from "../../serverSession";

const PROJECT = `project Demo:
    provider:
        tool = "nudge"
`;

function declarationOf(text: string, folder = "demo"): ProviderDeclaration {
  const block = parseProviderBlock(text);
  assert.ok(block?.tool, "fixture declares a tool");
  return {
    tool: block.tool,
    sources: [{ folder, file: "nml-project.nml", block: block.canonical }],
  };
}

suite("providerProject/parseProviderBlock", () => {
  test("the canonical block ignores comments, blank lines and the file's indentation", () => {
    const a = parseProviderBlock(PROJECT);
    const b = parseProviderBlock(
      'project Demo:\n\n  // a note\n  provider:\n\n      tool   =   "nudge"   // why\n'
    );
    assert.strictEqual(a?.tool, "nudge");
    assert.strictEqual(b?.tool, "nudge");
    assert.strictEqual(a?.canonical, b?.canonical);
  });

  test("the canonical block changes when the declaration does", () => {
    const base = parseProviderBlock(PROJECT)?.canonical;
    assert.notStrictEqual(
      base,
      parseProviderBlock('project Demo:\n    provider:\n        tool = "sh"\n')?.canonical
    );
    assert.notStrictEqual(
      base,
      parseProviderBlock(
        'project Demo:\n    provider:\n        tool = "nudge"\n        channel = "in-binary"\n'
      )?.canonical,
      "an added key is a different declaration"
    );
  });

  test("the block stops at the first line that leaves it", () => {
    const block = parseProviderBlock(
      'project Demo:\n    provider:\n        tool = "nudge"\n    other:\n        tool = "sh"\n'
    );
    assert.strictEqual(block?.tool, "nudge");
    assert.ok(!block.canonical.includes("sh"));
  });

  test("a file with no provider block declares nothing", () => {
    assert.strictEqual(parseProviderBlock("project Demo:\n    name = 1\n"), undefined);
  });
});

suite("providerTrust/declarationDigest", () => {
  test("the same declaration in the same folder digests the same", () => {
    assert.strictEqual(
      declarationDigest(declarationOf(PROJECT)),
      declarationDigest(declarationOf(PROJECT))
    );
  });

  test("a `git pull` that edits the declaration changes the digest", () => {
    // The A2 case, and the reason the pin is the DECLARATION rather than the
    // folder: the operator consented to a sentence, not to a directory.
    assert.notStrictEqual(
      declarationDigest(declarationOf(PROJECT)),
      declarationDigest(
        declarationOf('project Demo:\n    provider:\n        tool = "evil"\n')
      )
    );
  });

  test("the same text in a different folder digests differently", () => {
    assert.notStrictEqual(
      declarationDigest(declarationOf(PROJECT, "demo")),
      declarationDigest(declarationOf(PROJECT, "other"))
    );
  });

  test("folder order does not change the digest, but an added folder does", () => {
    const one = declarationOf(PROJECT, "a").sources[0];
    const two = declarationOf(PROJECT, "b").sources[0];
    assert.strictEqual(
      declarationDigest({ tool: "nudge", sources: [one, two] }),
      declarationDigest({ tool: "nudge", sources: [two, one] })
    );
    assert.notStrictEqual(
      declarationDigest({ tool: "nudge", sources: [one] }),
      declarationDigest({ tool: "nudge", sources: [one, two] })
    );
  });

  test("length-prefixed fields: a folder named like a separator cannot forge a digest", () => {
    const block = parseProviderBlock(PROJECT)?.canonical ?? "";
    assert.notStrictEqual(
      declarationDigest({
        tool: "nudge",
        sources: [{ folder: "a", file: `nml-project.nml;1:x;`, block }],
      }),
      declarationDigest({
        tool: "nudge",
        sources: [{ folder: "a", file: "nml-project.nml", block: `x;${block}` }],
      })
    );
  });

  test("a digest is a labelled sha256", () => {
    assert.match(declarationDigest(declarationOf(PROJECT)), /^sha256:[0-9a-f]{64}$/);
  });

  test("the prefix counts exactly the units that are hashed, so no two declarations frame alike", () => {
    // `String.length` is UTF-16 code units; `hash.update(string)` used to
    // feed UTF-8 bytes, which is LOSSY: a lone surrogate is ONE unit and
    // encodes to the THREE bytes of U+FFFD, so `"\uD800"` and `"\uFFFD"`
    // are different declarations that framed and hashed identically — the
    // length prefix, which is the whole injectivity argument, stopped
    // separating them.
    const block = parseProviderBlock(PROJECT)?.canonical ?? "";
    const digestOfFolder = (folder: string): string =>
      declarationDigest({
        tool: "nudge",
        sources: [{ folder, file: "nml-project.nml", block }],
      });
    assert.notStrictEqual(digestOfFolder("\uD800"), digestOfFolder("\uFFFD"));
    // …and the ordinary non-ASCII case still separates cleanly.
    assert.notStrictEqual(digestOfFolder("é"), digestOfFolder("e"));
    assert.notStrictEqual(digestOfFolder("😀"), digestOfFolder("ab"));
    assert.strictEqual(digestOfFolder("é"), digestOfFolder("é"));
  });
});

suite("providerTrust/approvalDecision", () => {
  const digest = "sha256:aa";
  const command = "/opt/bin/nudge";
  const approved: ApprovalRecord = {
    v: APPROVAL_RECORD_VERSION,
    digest,
    command,
    decision: "approved",
  };

  test("no record asks", () => {
    assert.deepStrictEqual(approvalDecision(undefined, digest, command), {
      kind: "ask",
      because: "never-asked",
    });
  });

  test("the same declaration at the same path runs without asking again", () => {
    assert.deepStrictEqual(approvalDecision(approved, digest, command), { kind: "approved" });
  });

  test("a changed declaration asks again — the direnv rule", () => {
    assert.deepStrictEqual(approvalDecision(approved, "sha256:bb", command), {
      kind: "ask",
      because: "declaration-changed",
    });
  });

  test("the same declaration resolving to a DIFFERENT path asks again", () => {
    // direnv has no equivalent of this, because it does not resolve a name
    // on PATH: here a new PATH entry can shadow the approved binary without
    // any file in the repository changing.
    assert.deepStrictEqual(approvalDecision(approved, digest, "/tmp/bin/nudge"), {
      kind: "ask",
      because: "path-changed",
    });
  });

  test("a record from a superseded shape is not honoured", () => {
    assert.deepStrictEqual(
      approvalDecision({ ...approved, v: APPROVAL_RECORD_VERSION - 1 }, digest, command),
      { kind: "ask", because: "record-superseded" }
    );
  });

  test("a decline is remembered, and is pinned the same way an approval is", () => {
    const declined: ApprovalRecord = { ...approved, decision: "declined" };
    assert.deepStrictEqual(approvalDecision(declined, digest, command), { kind: "declined" });
    // Declining `sh` must not silently veto a later, honest declaration.
    assert.deepStrictEqual(approvalDecision(declined, "sha256:bb", command), {
      kind: "ask",
      because: "declaration-changed",
    });
  });
});

suite("providerTrust/classifyResolutionDirectory", () => {
  const me = 501;

  test("a directory only its owner can write to is unremarkable", () => {
    assert.deepStrictEqual(classifyResolutionDirectory({ mode: 0o40755, uid: me }, me, "darwin"), {
      kind: "ok",
    });
  });

  test("a root-owned system directory is unremarkable", () => {
    assert.deepStrictEqual(classifyResolutionDirectory({ mode: 0o40755, uid: 0 }, me, "linux"), {
      kind: "ok",
    });
  });

  test("Homebrew's group-writable prefix is REPORTED, not refused", () => {
    // MEASURED on the review host: /opt/homebrew/bin is drwxrwxr-x, owner
    // the operator, group `admin`. Refusing it would refuse the most common
    // way macOS developers install tools — so it is a fact for the prompt.
    const verdict = classifyResolutionDirectory({ mode: 0o40775, uid: me }, me, "darwin");
    assert.strictEqual(verdict.kind, "shared-group");
    assert.strictEqual(verdict.kind === "shared-group" ? verdict.mode : "", "0775");
  });

  test("a world-writable directory is refused", () => {
    const verdict = classifyResolutionDirectory({ mode: 0o40777, uid: me }, me, "linux");
    assert.strictEqual(verdict.kind, "refused");
    assert.strictEqual(verdict.kind === "refused" ? verdict.reason : "", "world-writable");
  });

  test("a sticky world-writable directory is refused too", () => {
    // /tmp-shaped. Sticky stops REPLACING someone else's file; it does not
    // stop an attacker who got there first from being the one you find.
    const verdict = classifyResolutionDirectory({ mode: 0o41777, uid: 0 }, me, "linux");
    assert.strictEqual(verdict.kind, "refused");
  });

  test("a directory belonging to another account is refused", () => {
    const verdict = classifyResolutionDirectory({ mode: 0o40755, uid: 1234 }, me, "linux");
    assert.strictEqual(verdict.kind, "refused");
    assert.strictEqual(verdict.kind === "refused" ? verdict.reason : "", "foreign-owner");
  });

  test("Windows gets no verdict rather than a made-up one", () => {
    // Node exposes no ACL reader, and a synthesized POSIX mode on NTFS is a
    // lie. Saying so is better than a check that passes for the wrong reason.
    assert.deepStrictEqual(classifyResolutionDirectory({ mode: 0o40777, uid: 0 }, me, "win32"), {
      kind: "ok",
    });
  });

  test("an unstattable directory gets no verdict", () => {
    assert.deepStrictEqual(classifyResolutionDirectory(undefined, me, "linux"), { kind: "ok" });
  });
});

suite("providerTrust/scrubbedEnvOverlay", () => {
  test("the loader and interpreter injection points are named", () => {
    for (const name of [
      "LD_PRELOAD",
      "LD_LIBRARY_PATH",
      "LD_AUDIT",
      "DYLD_INSERT_LIBRARIES",
      "DYLD_LIBRARY_PATH",
      "NODE_OPTIONS",
      "BASH_ENV",
      "PERL5OPT",
      "PYTHONSTARTUP",
      "RUBYOPT",
      "RUSTC_WRAPPER",
    ]) {
      assert.ok(isScrubbedEnvName(name), `${name} must be scrubbed`);
    }
  });

  test("an exported shell FUNCTION is removed — it is a program, not a path", () => {
    // `export -f id` reaches a child as `BASH_FUNC_id%%=() { … }`; bash
    // defines it at startup, so every `id` a `#!/bin/sh`-shaped tool runs is
    // the attacker's body. MEASURED on this platform before it was scrubbed.
    for (const name of ["BASH_FUNC_id%%", "BASH_FUNC_git%%", "BASH_FUNC_ls()"]) {
      assert.ok(isScrubbedEnvName(name), `${name} must be scrubbed`);
    }
    const overlay = scrubbedEnvOverlay({ "BASH_FUNC_id%%": "() { evil; }", PATH: "/usr/bin" });
    assert.deepStrictEqual(Object.keys(overlay), ["BASH_FUNC_id%%"]);
  });

  test("glibc's own unsecvars are covered, minus the two kept on purpose", () => {
    // The list this denylist claims to name: what the dynamic loader itself
    // strips for a set-user-ID binary (glibc `elf/Makefile`, `unsecvars`).
    // `TMPDIR` is the one deliberate omission (removing it relocates a
    // tool's scratch files to the world-reachable `/tmp`), and it is pinned
    // as surviving in the test below.
    const unsecvars = [
      "GCONV_PATH", "GETCONF_DIR", "GLIBC_TUNABLES", "HOSTALIASES", "LD_AUDIT",
      "LD_DEBUG", "LD_DEBUG_OUTPUT", "LD_DYNAMIC_WEAK", "LD_HWCAP_MASK",
      "LD_LIBRARY_PATH", "LD_ORIGIN_PATH", "LD_PRELOAD", "LD_PROFILE",
      "LD_SHOW_AUXV", "LD_USE_LOAD_BIAS", "LOCALDOMAIN", "LOCPATH",
      "MALLOC_TRACE", "NIS_PATH", "NLSPATH", "RESOLV_HOST_CONF", "RES_OPTIONS",
      "TZDIR",
    ];
    for (const name of unsecvars) {
      assert.ok(isScrubbedEnvName(name), `glibc strips ${name}; so must we`);
    }
  });

  test("every interpreter's `run this first` hook is removed", () => {
    // The fixed `lsp` argument means a declared tool name may resolve to an
    // interpreter, and the program is then a SCRIPT: each of these names
    // code that runtime loads or executes before the script's first line.
    for (const name of [
      "ZDOTDIR", "KSH_ENV", "PS4", "PROMPT_COMMAND", "CDPATH", "IFS",
      "PYTHONBREAKPOINT", "PYTHONINSPECT", "PYTHONUSERBASE", "PERL5DB",
      "GEM_HOME", "RUBYPATH", "LUA_INIT", "R_PROFILE", "JULIA_LOAD_PATH",
      "NODE_PATH", "ELECTRON_RUN_AS_NODE", "JAVA_TOOL_OPTIONS",
      "_JAVA_OPTIONS", "JDK_JAVA_OPTIONS", "CLASSPATH", "DOTNET_STARTUP_HOOKS",
      "GIT_SSH_COMMAND", "GIT_EXTERNAL_DIFF", "GIT_PAGER", "GIT_CONFIG_GLOBAL",
    ]) {
      assert.ok(isScrubbedEnvName(name), `${name} must be scrubbed`);
    }
  });

  test("a CONFIG FILE that names a program is removed like the program would be", () => {
    // Three families where the variable names no program itself, and the
    // FILE or DIRECTORY it names does:
    //   · OpenSSL's config declares native modules to load — `[engine]
    //     dynamic_path` (1.x), `[provider_sect] module` (3.x) — which is
    //     `LD_PRELOAD` spelled as a config;
    //   · `<GIT_DIR>/config` is git's local configuration, and it names
    //     `core.fsmonitor`, `core.sshCommand`, `core.pager`,
    //     `diff.external`, `filter.*.clean`; scrubbing `GIT_CONFIG*` and
    //     leaving `GIT_DIR` was the same fence with a gate in it;
    //   · `GIT_TEMPLATE_DIR` seeds a new repository's HOOKS.
    for (const name of [
      "OPENSSL_CONF", "OPENSSL_ENGINES", "OPENSSL_MODULES",
      "GIT_DIR", "GIT_COMMON_DIR", "GIT_TEMPLATE_DIR",
    ]) {
      assert.ok(isScrubbedEnvName(name), `${name} names code to load; it must be scrubbed`);
    }
  });

  test("an interpreter's VERSIONED hook is removed too", () => {
    // Lua checks `LUA_INIT_5_4` before `LUA_INIT` (and the same for
    // `LUA_PATH`/`LUA_CPATH`), so three exact names left every
    // interpreter that ships a suffix reachable. The prefix is the rule.
    for (const name of [
      "LUA_INIT", "LUA_INIT_5_4", "LUA_PATH_5_3", "LUA_CPATH_5_4", "LUA_PATH",
    ]) {
      assert.ok(isScrubbedEnvName(name), `${name} must be scrubbed`);
    }
  });

  test("a tool's own configuration is left alone", () => {
    for (const name of ["PATH", "HOME", "LANG", "TMPDIR", "NML_SCHEMA_STORE_DIR", "RUST_LOG"]) {
      assert.ok(!isScrubbedEnvName(name), `${name} must survive`);
    }
  });

  test("the overlay REMOVES rather than blanks, and only what is there", () => {
    const overlay = scrubbedEnvOverlay({ LD_PRELOAD: "/evil.so", PATH: "/usr/bin" });
    assert.deepStrictEqual(Object.keys(overlay), ["LD_PRELOAD"]);
    assert.strictEqual(overlay.LD_PRELOAD, undefined);
  });

  test("on Windows the case of the NAME does not decide whether it is removed", () => {
    // Windows looks environment variables up case-insensitively: a runtime
    // that reads `NODE_OPTIONS` reads it out of a block spelling it
    // `Node_Options`, and `Object.keys(process.env)` hands back the stored
    // spelling. An exact-bytes denylist would remove one and leave the other
    // in place for the same reader, which is the control bypassed by holding
    // down shift.
    for (const name of ["Node_Options", "node_options", "PyThOnStArTuP", "Ld_Preload"]) {
      assert.ok(isScrubbedEnvName(name, "win32"), `${name} must be scrubbed on Windows`);
      assert.ok(!isScrubbedEnvName(name, "linux"), `${name} is a DIFFERENT variable on POSIX`);
    }
    assert.deepStrictEqual(
      Object.keys(scrubbedEnvOverlay({ Node_Options: "--require C:\\x.js", Path: "C:\\bin" }, "win32")),
      ["Node_Options"]
    );
    // POSIX is exact on purpose: there the two spellings really are two
    // variables, and removing a name nobody set is not the same as removing
    // one somebody did.
    assert.deepStrictEqual(
      Object.keys(scrubbedEnvOverlay({ Node_Options: "x", NODE_OPTIONS: "y" }, "linux")),
      ["NODE_OPTIONS"]
    );
  });
});

suite("providerTrust/judgeServerIdentity", () => {
  test("the NML server identifies itself", () => {
    assert.deepStrictEqual(
      judgeServerIdentity({ name: NML_SERVER_NAME, version: "0.1.0" }),
      { kind: "identified", version: "0.1.0" }
    );
  });

  test("a different name is a definite negative", () => {
    assert.deepStrictEqual(judgeServerIdentity({ name: "rust-analyzer" }), {
      kind: "repudiated",
      saw: "rust-analyzer",
    });
  });

  test("no name is not accepted, and is not an accusation either", () => {
    // LSP 3.17 makes the field optional for servers in general; a client
    // that LAUNCHED the program on a repository's say-so is entitled to a
    // stricter contract, so silence is never `identified`. But every
    // provider built before the server sent `serverInfo` answers exactly
    // this way, so silence is not `repudiated` — the outcome that costs the
    // operator their approval — either.
    for (const silent of [undefined, {}, { name: "" }, { version: "0.1.0" }]) {
      assert.deepStrictEqual(judgeServerIdentity(silent), { kind: "unidentified" });
    }
  });

  test("the budget is generous enough that expiry means something", () => {
    // A 2 s budget was proposed; this project has measured its own
    // `initialize` missing a 20 s watchdog on a loaded host. A check that
    // cries wolf is worse than no check.
    assert.ok(INITIALIZE_BUDGET_MS >= 10_000, "at least 10 s");
  });
});

suite("providerTrust/what the operator reads", () => {
  const prompt = providerConsentPrompt({
    tool: "nudge",
    command: "/opt/homebrew/bin/nudge",
    args: ["lsp"],
    sources: [{ folder: "demo", file: "nml-project.nml", block: "provider:" }],
    directory: { kind: "ok" },
  });

  test("the question is one line and names the tool", () => {
    assert.strictEqual(prompt.message, 'Run "nudge" as this project\'s NML language server?');
    assert.ok(!prompt.message.includes("\n"), "the message is the question, not the briefing");
  });

  test("the detail shows the exact command line that will run", () => {
    // The residual r106 could not close structurally: `<tool> lsp` is a
    // subcommand for a real tool and a script path for an interpreter, and
    // no portable argv convention distinguishes them. The operator seeing
    // `/bin/sh lsp` written out is the mitigation.
    assert.match(prompt.detail, /Will run: {2}\/opt\/homebrew\/bin\/nudge lsp/);
  });

  test("the detail names the file that asked, and the folder it is in", () => {
    assert.match(prompt.detail, /Asked by: {2}nml-project\.nml \(in the folder "demo"\)/);
  });

  test("the detail says what the program can do, without claiming it is safe", () => {
    assert.match(prompt.detail, /runs with your account's permissions/);
    assert.match(prompt.detail, /read and change your files/);
    assert.ok(!/\bsafe\b/i.test(prompt.detail), "the extension cannot vouch for it");
  });

  test("the detail says the answer is remembered AND how to take it back", () => {
    assert.match(prompt.detail, /remembers it for this workspace/);
    assert.match(prompt.detail, /until nml-project\.nml or that path changes/);
    assert.match(prompt.detail, /NML: Forget Language Server Approvals/);
  });

  test("declining is not a cliff: the detail says editing keeps working", () => {
    assert.match(prompt.detail, /NML editing keeps working/);
    assert.match(prompt.detail, /built-in server, which needs no setup/);
  });

  test("the buttons name their action rather than answering a yes/no", () => {
    assert.strictEqual(prompt.accept, 'Use "nudge"');
    assert.strictEqual(prompt.decline, "Keep the built-in server");
  });

  test("a group-writable directory is disclosed in the prompt, not hidden", () => {
    const shared = providerConsentPrompt({
      tool: "nudge",
      command: "/opt/homebrew/bin/nudge",
      args: ["lsp"],
      sources: [{ folder: "demo", file: "nml-project.nml", block: "provider:" }],
      directory: { kind: "shared-group", mode: "0775" },
    });
    assert.match(shared.detail, /\/opt\/homebrew\/bin is group-writable \(mode 0775\)/);
    assert.ok(!prompt.detail.includes("group-writable"), "silent when there is nothing to say");
  });

  test("every declaring folder is listed, not just the first", () => {
    const multi = providerConsentPrompt({
      tool: "nudge",
      command: "/opt/bin/nudge",
      args: ["lsp"],
      sources: [
        { folder: "api", file: "nml-project.nml", block: "provider:" },
        { folder: "web", file: "nml-project.nml", block: "provider:" },
      ],
      directory: { kind: "ok" },
    });
    assert.match(multi.detail, /in the folder "api"/);
    assert.match(multi.detail, /in the folder "web"/);
  });

  test("a repudiation says what stopped, what it claimed, and what happened to the approval", () => {
    const m = repudiatedMessage("nudge", "sh");
    assert.match(m, /NML stopped "nudge"/);
    assert.match(m, /answered the editor's handshake as "sh"/);
    assert.match(m, /not as an NML language server/);
    assert.match(m, /approval has been removed/);
    assert.match(m, /switched to the built-in server/);
    // A program that named SOMEONE ELSE is not an out-of-date build, so it
    // must not be handed the out-of-date build's remedy.
    assert.doesNotMatch(m, /rebuild/i);
  });

  test("a provider that named NOTHING is told the likely cause and the one action that fixes it", () => {
    // THE case every provider binary built before the server started
    // sending `serverInfo` lands in — an honest NML server the editor
    // cannot recognise. A message that only accuses leaves that operator
    // re-approving the same tool on every window reload.
    const m = unidentifiedMessage("nudge");
    assert.match(m, /NML stopped "nudge"/);
    assert.match(m, /without naming itself/);
    assert.match(m, /cannot tell what it is/);
    assert.match(m, /built against an NML older than the one that added the handshake/);
    assert.match(m, /rebuild it against a current nml-lsp/);
    // The approval STANDS: withdrawing it re-prompts on every window reload
    // for a tool whose only fault is its build date, and the rebuilt tool
    // must simply work in the next window.
    assert.match(m, /approval is unchanged/);
    assert.doesNotMatch(m, /approval has been removed/);
    assert.doesNotMatch(m, /cannot trust/);
    assert.match(m, /switched to the built-in server for this session/);
    // The sentence must READ: the two clauses used to collide into "it did
    // not identify itself instead of an NML language server".
    assert.doesNotMatch(m, /itself instead of/);
    // …and it must say WHEN the rebuild lands. This outcome stands the
    // provider down for the session, so the rebuilt tool is picked up by
    // the next WINDOW; a remedy with no timing is one the operator tries,
    // sees nothing from, and stops believing.
    assert.match(m, /reload the window/);
  });

  test("every rung above the question says what it did, and what would change it", () => {
    // The ladder used to return the built-in server in silence four times
    // over. A project that declares a tool and does not get it is a fact the
    // operator can learn NOWHERE ELSE — the status bar names the server that
    // IS running, and there is no prompt, because not reaching the prompt is
    // what happened.
    const lines = [
      skippedProviderMessage("", { kind: "conflicting", tools: ["nudge", "other"] }),
      skippedProviderMessage("nudge", { kind: "untrusted" }),
      skippedProviderMessage("nudge", { kind: "not-on-path" }),
      skippedProviderMessage("nudge", { kind: "inside-workspace", command: "/ws/bin/nudge" }),
    ];
    for (const line of lines) {
      assert.match(line, /^Using the built-in NML server: /, line);
      // A fact with no lever is a fact the reader can do nothing with.
      assert.ok(
        /Trust the workspace|Install it|Make the nml-project\.nml files agree/.test(line),
        `no way forward in: ${line}`
      );
    }
    assert.match(lines[0], /declare different language servers \("nudge", "other"\)/);
    assert.match(lines[1], /this workspace is not trusted/);
    assert.match(lines[1], /Committed schema is still checked/);
    assert.match(lines[2], /no program called "nudge" is on PATH/);
    assert.match(lines[3], /resolves to \/ws\/bin\/nudge — inside this workspace/);
  });

  test("a remembered decline is said out loud, with the way back", () => {
    const m = declinedMessage("nudge");
    assert.match(m, /you declined "nudge" for this workspace/);
    assert.match(m, /NML: Forget Language Server Approvals/);
  });

  test("a stand-down says it is for this session only", () => {
    const m = stoodDownMessage("nudge");
    assert.match(m, /stood down earlier in this session/);
    assert.match(m, /Reload the window to try it again/);
  });

  test("the log says WHY the question is being asked, in words, not a tag", () => {
    const asked = (because: Parameters<typeof askingMessage>[2]): string =>
      askingMessage("nudge", "/opt/bin/nudge", because);
    assert.match(asked("never-asked"), /has not been asked about it/);
    assert.match(asked("declaration-changed"), /provider declaration changed/);
    assert.match(asked("path-changed"), /resolves somewhere else/);
    assert.match(asked("record-superseded"), /predates this approval format/);
    for (const because of [
      "never-asked",
      "declaration-changed",
      "path-changed",
      "record-superseded",
    ] as const) {
      // The raw tag is the code's vocabulary, not the reader's.
      assert.doesNotMatch(asked(because), new RegExp(`: ${because}\\.$`));
      assert.match(asked(because), /^Asking about the project's language server "nudge" \(\/opt\/bin\/nudge\): /);
    }
  });

  test("an unanswered handshake accuses nobody and says the approval is intact", () => {
    const m = indefiniteMessage("nudge", 15_000);
    assert.match(m, /did not finish starting within 15 seconds/);
    assert.match(m, /approval is unchanged/);
    assert.ok(!/not an NML/i.test(m), "silence is not evidence");
  });

  test("an unanswered handshake names the action that actually retries it", () => {
    // MEASURED, not read: this outcome calls `identity.standDown`, and a
    // stood-down provider is skipped by the NEXT resolution — which is what
    // `NML: Restart Language Server` performs. So "restart the language
    // server to try it again" named an action that silently came back with
    // the built-in server. The window reload is the one that clears the
    // session's stand-downs, and it is what `stoodDownMessage` already says.
    const m = indefiniteMessage("nudge", 15_000);
    assert.match(m, /reload the window to try it again/);
    assert.doesNotMatch(m, /restart the language server/i);
    // The two sentences about ONE stand-down must not disagree.
    assert.match(stoodDownMessage("nudge"), /Reload the window to try it again/);
  });

  test("a refused directory explains the refusal and the way out", () => {
    const m = refusedDirectoryMessage("nudge", "/tmp/bin/nudge", {
      kind: "refused",
      reason: "world-writable",
      mode: "0777",
      uid: 0,
    });
    assert.match(m, /any user on this machine can replace programs in \/tmp\/bin/);
    assert.match(m, /Using the built-in NML server/);
    assert.match(m, /Install the tool in a directory no other account can write to/);
    // The second half used to offer `nml.server.path` as a peer remedy. It is
    // not one: `resolveNeutralServer` spawns that path with NO arguments
    // (serverAcquisition.ts), while a provider is spawned as `<tool> lsp`
    // (providerDiscovery.ts) — so pointing the setting at the project's tool
    // runs a binary that does not speak LSP. The sentence must say so rather
    // than send the operator there.
    assert.match(m, /nml\.server\.path cannot stand in for it/);
    assert.doesNotMatch(m, /point nml\.server\.path at/);
  });

  test("every line about a project's tool NOT being used reads as one voice", () => {
    // Seven sentences, one class of event, one log channel. The reader's
    // first question on all of them is "is NML editing still working?" — it
    // is — so they all answer it first, and the class is one search of the
    // log rather than seven shapes to recognise.
    const lines = [
      skippedProviderMessage("", { kind: "conflicting", tools: ["nudge", "other"] }),
      skippedProviderMessage("nudge", { kind: "untrusted" }),
      skippedProviderMessage("nudge", { kind: "not-on-path" }),
      skippedProviderMessage("nudge", { kind: "inside-workspace", command: "/ws/bin/nudge" }),
      declinedMessage("nudge"),
      stoodDownMessage("nudge"),
      refusedDirectoryMessage("nudge", "/tmp/bin/nudge", {
        kind: "refused",
        reason: "world-writable",
        mode: "0777",
        uid: 0,
      }),
    ];
    for (const line of lines) {
      assert.match(line, /^Using the built-in NML server: /, `off the shared lead: ${line}`);
    }
  });
});
