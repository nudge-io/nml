# NML Language Support

Schema-aware editing for the **NML** configuration language: diagnostics,
completions, hovers, go-to-definition, and duration-aware semantic
highlighting, validated against your project's schema.

## How it works

The extension ships a **neutral language server compiled to WebAssembly** and
runs it sandboxed via the WASI host — no separate binary to install, works
offline, on every desktop platform VS Code runs on. It validates:

- **committed schema** — drop a `<name>.package.nml` (+ its `*.model.nml`
  or `*.schema.nml` sources) in your repo and it is discovered
  automatically, and
- **your tool's schema** — a project may opt in (in `nml-project.nml`) to its
  build tool's own language server (`<tool> lsp`, e.g. `nudge lsp`), launched
  only in a **trusted** workspace, only from `PATH`, and only after you approve
  it once.

The status bar (bottom-right) names the active schema and where it came from.
A document the server refuses to validate — two manifests claim it, a manifest
or project config failed to load, its path fits no key — shows `nml: not
validated` on the error background, with the reason and the remedy in the
tooltip; a warning note, or a manifest above a derived root being ignored,
colours the item as a warning.

## Settings

- `nml.server.path` — absolute path to a native `nml-lsp` binary to use instead
  of the bundled WASM server (machine-scoped: a repository cannot set it).
- `nml.trace.server` — log LSP protocol traffic to the trace output channel.

Host compatibility (`engines.vscode` vs `@types/vscode`) is documented in
[VSCODE-API.md](./VSCODE-API.md).

## Commands

- **NML: Restart Language Server**
- **NML: Forget Language Server Approvals** — forget this workspace's answer
  about its project-declared language server, so you are asked again
- **NML: Explain a Diagnostic Code** — the full error-index entry for any
  code, searchable by code or summary
- **NML: Show Language Server Log** / **NML: Show Language Server Trace**

## Security

NML is data, not code, so the server reads committed schema even in an untrusted
workspace. Launching a project's own tool as a language server is the only
trust-gated action. Before anything runs, the extension requires a trusted
workspace, refuses a binary inside the workspace, refuses a `PATH` directory
other accounts can write to, and asks you — showing the file that declared the
tool, the exact command line, and what accepting means. Your answer is
remembered for that workspace until the declaration or the resolved path
changes, so a `git pull` that rewrites the declaration asks again; **NML:
Forget Language Server Approvals** takes it back.

If a project declares a tool and it is **not** used — because the workspace is
not trusted, because no program of that name is on `PATH`, because the name
resolves inside the workspace, because two folders name different tools, or
because you declined it — **NML: Show Language Server Log** says which of those
happened and what would change it. NML editing keeps working throughout, on the
built-in server.

What runs is contained: an empty private working directory (the declared tool
name may resolve to an interpreter, for which `lsp` would otherwise be a script
path a repository can supply), and an environment with the loader- and
interpreter-injection variables removed. Once it is running it must answer the
`initialize` handshake as an NML language server; if it answers as something
else, the extension stops it, forgets the approval, tells you, and falls back
to the bundled server.

**A tool built before this requirement existed answers the handshake with no
name at all**, and is stopped for that reason — it is out of date, not an
impostor, so your approval is kept, the message says so and asks for a rebuild
against a current `nml-lsp`, and the rebuilt tool simply starts in the next
window. Until then NML editing uses the bundled server.

**Every language server the extension starts ends when the editor does.** It
is run in its own process group behind a small supervisor that the editor
holds a pipe to, and that group is ended the moment the pipe closes — which
happens however the editor goes away, a crash or `kill -9` included. Stopping
one is staged and bounded: close its input, then signal the group, then force
it. What you are told is what was OBSERVED — a program that could not be ended
is named with its process id rather than reported as stopped — and its exit
code goes to the log.

What that reaches is the server and everything it forked. A program that
deliberately puts a helper of its own into a NEW process group (`setsid`) puts
it outside what any parent can signal on these systems, so this is a guarantee
about a server that plays by the rules, not a sandbox around one that does
not — which is why the prompt above asks you to approve the program itself.

Licensed MIT OR Apache-2.0.
