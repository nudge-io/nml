# Embed the language server

Give your CLI a `<your-tool> lsp` subcommand and your users get
schema-aware editing **against the exact binary they run** — diagnostics
with stable codes and quick-fixes, completion, hover docs, in-editor error
explanations — with zero schema sync, because the schema ships inside your
tool. One call is the whole subcommand:

```rust source=docs/guides/examples/cookbook/examples/embed_lsp.rs
    // The whole body of `<your-tool> lsp`: serves LSP over stdio until the
    // session ends, and says how it ended. `SessionEnd::exit_code()` is the
    // protocol's own mapping (LSP 3.17 §exit: 1 when `exit` arrived with no
    // `shutdown` before it, 0 for the orderly ending AND for a client that
    // simply closed the pipe) — return it from `main`, or map the ending
    // onto your own exit codes, as `tool_exit` below does.
    let ended = nml_lsp::serve(package).await;
```

`SessionEnd` is that answer: `Exited` (`exit` after `shutdown`, the
protocol's orderly ending), `ExitedWithoutShutdown` (`exit` with no
`shutdown` before it — the client's error) and `Disconnected` (the client
closed the pipe without saying anything). If your tool already has a closed
set of exit codes, map the ending onto that set instead of returning
`exit_code()`, and match exhaustively so an ending added later is a compile
error rather than a silent success — this is what `nudge lsp` does:

```rust source=docs/guides/examples/cookbook/examples/embed_lsp.rs
fn tool_exit(ended: nml_lsp::SessionEnd) -> ToolExit {
    match ended {
        nml_lsp::SessionEnd::Exited | nml_lsp::SessionEnd::Disconnected => ToolExit::Ok,
        nml_lsp::SessionEnd::ExitedWithoutShutdown => ToolExit::Refused,
    }
}
```

Full program, both pieces in place:
[`embed_lsp.rs`](examples/cookbook/examples/embed_lsp.rs) —
`cargo run -p nml-cookbook --example embed_lsp` (it serves stdio and exits
on EOF, which is exactly how the docs harness runs it in CI). Everything
else in that file is your tool's own embedded package.

`serve(package)` is the neutral server **plus** your embedded package at
in-binary precedence: files your package's bindings claim validate against
your schemas; everything else behaves exactly like the standalone
`nml-lsp`. Your package's directive vocabulary, modifiers, and strictness
all apply — declared once ([directive vocabulary](directive-vocabulary.md)),
enforced in the editor.

**How editors find it:** users declare the tool in an `nml-project.nml`
beside their config —

```nml fragment
project MyApp:
    provider:
        tool = "<your-tool>"
```

— and the VS Code extension launches `<your-tool> lsp` — only
in trusted workspaces, only from PATH, only after a per-workspace prompt
(the trust model is deliberate: a repository can never redirect the editor
at an arbitrary binary). Untrusted workspaces still get the bundled neutral
server against committed schema files.

## What the editor requires of your tool

Four requirements, all of them about the LAUNCH — miss one and the editor
uses its own bundled server instead, and says why. Four more, about how
your process LIVES and ends, are in the next section; between them they are
everything the editor asks of you, and none of them is about NML.

1. **Answer `initialize` with `serverInfo`.** The client verifies that
   the program it was told to run identifies itself as an NML language
   server (`serverInfo.name = "nml-lsp"`, LSP 3.17). `nml_lsp::serve`
   sends it for you — but only from the release that added it, so **a
   `<your-tool>` built against an older `nml-lsp` answers with no name
   and is stopped at every launch** — the operator keeps their approval
   and is told to ask you for a rebuild, and the editor uses its bundled
   server meanwhile. Rebuilding against a current `nml-lsp` is the whole
   fix; no code of yours changes. (A program that answers with a
   DIFFERENT name is stopped and loses its approval.)
2. **Do not depend on your working directory.** The extension spawns you
   in an empty private directory of its own, never the workspace: the
   fixed `lsp` argument must not be able to resolve to a file a repository
   shipped. Resolve your own paths from the URIs the client sends.
3. **Do not depend on loader or interpreter environment variables.**
   `LD_*`, `DYLD_*`, `NODE_OPTIONS`, `BASH_ENV`, `ENV`, `SHELLOPTS`, the
   `PERL*`, `PYTHON*` and `RUBY*` hooks and the `RUSTC_*` wrappers are
   REMOVED from your environment. `PATH`, `HOME`, proxy and CA settings,
   `RUST_LOG` and your own `NML_*` variables are untouched. A tool that is
   a wrapper script around a runtime must not need the removed ones.
4. **Be installable somewhere only its owner can write.** Before running
   anything the extension judges the `PATH` directory your name resolved
   in: world-writable, or owned by a third account, is refused outright
   (a group-writable prefix such as Homebrew's is disclosed in the prompt,
   not refused). `~/.cargo/bin`, `/usr/local/bin` and `/opt/homebrew/bin`
   are all fine; a shared `/tmp`-style directory is not. The verdict is
   taken again after the operator answers the prompt — a modal is human
   time — so a directory that becomes world-writable while it is up is
   refused even though consent was given.

## How your process is owned, and how it ends

You are not spawned as a child of the extension host. On **POSIX** the
extension runs you through a small supervisor of its own: you are the
supervisor's child, in your OWN process group, on the stdio the editor
created — so the language client reads and writes you directly and nothing
sits in the LSP data path. The supervisor holds a control pipe to the
editor and SIGKILLs your whole process group the moment that pipe reaches
EOF, which the kernel delivers however the editor went away, `kill -9`
included. On **Windows** you are spawned directly and never detached, which
places you in the job object the editor's runtime creates with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.

What follows for you — the other four requirements:

* **Exit on `exit`, and on stdin EOF.** `nml_lsp::serve` does both for
  you: the LSP `exit` notification ends the session, a closed stdin ends it
  as `Disconnected`, and either way `serve` returns the `SessionEnd` your
  `main` turns into a code (above). What is asked of you is that your
  `main` actually return it. The editor's polite stage is the protocol
  handshake when it can be, the closed stdin otherwise; a server that parks
  on a read of an open stdin after `exit` is one the editor has to signal.
* **Anything you fork is ended with you.** A worker you double-fork leaves
  your process TREE but stays in your process GROUP, and the group is what
  is signalled — when you are ended by the editor. (A helper you leave
  behind after exiting on your own is yours to end.) Do not rely on a helper
  outliving the session, and do not put unsaved state in one.
* **Do not create a new process group or session.** `setsid` takes you out
  of the group the editor kills, which is the one way to become the orphan
  this design exists to prevent.
* **Do not trap SIGTERM without exiting.** It is the second stage; SIGKILL
  is the third, and it is not negotiable.

Nothing is asked of your logging: **stderr still reaches the editor's log**,
unchanged, and your exit code or signal is now recorded there too.

The operator's side of this — the prompt, what it discloses, what
declining does, and **NML: Forget Language Server Approvals** — is in the
[extension's README](../../editors/vscode/README.md).
