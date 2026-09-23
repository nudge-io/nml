# Editor Integration

NML ships a language server (`nml-lsp`, native and `wasm32-wasip1`) and a
VS Code extension that bundles it. Any LSP-capable editor gets the same
surfaces from the server; the extension adds the packaging.

## Diagnostics

Full-fidelity validation as you type: every parse, symbol, schema, and
value diagnostic the CLI reports, at the same spans, with the same stable
`NML0000` codes ([error index](../errors/README.md)). Machine-applicable
fixes (did-you-means, syntax migrations like `=>`→`->` and `&&`→`&`)
arrive as quick-fixes. Secondary locations (an unterminated string's
opening quote) arrive as related information.

A denial whose remedy is an operator's edit in ANOTHER file — a
composition denied because the governing binding carries no `layers:`
grant (NML2064) — arrives with that edit as a quick-fix on the
manifest: **Add `layers` under 'tenantFlows' in demo.package.nml**
inserts the block the CLI prints, after the binding's last line,
nested by the manifest's own indentation — the binding's step, the
nearest block's around it, the document's one step, or the canonical
four spaces `nml fmt` writes when the manifest offers none (a file
that nests two ways never gets a third width from the editor) — and
ending its lines as the manifest does. It is offered on the denial and, on the
manifest, at the binding the denial's related location points at.
The edit names the manifest's document version (`documentChanges`,
LSP 3.17 §WorkspaceEdit — `null` for a manifest the editor has not
opened, whose master is the disk), so a client refuses it once the
buffer has moved on; a client that declared no `documentChanges`
receives plain `changes`. A stale denial — the manifest changed since
the diagnostic was reported — offers nothing until the next pull, and
the server asks for that pull: a client that declared
`workspace.diagnostics.refreshSupport` (VS Code does) receives one
`workspace/diagnostic/refresh` whenever a pull rediscovers the universe
(a manifest or project-config buffer opened, edited or closed), so every
open document's report — and the grant action the manifest offers from
the denial's — is current without a refocus; a binding that already
carries a grant is never given a second one. The
CLI's `nml fix` refuses the same insertion on a content file, in
print: the manifest is the operator's to change, there.

Files that CHANGE OUTSIDE the editor reach the server one of two ways.
A client that declares `workspace.didChangeWatchedFiles`
`.dynamicRegistration` (VS Code does) is sent a `**/*.nml` registration
and its events are the contract: the server reads the disk when it is
told to, and never otherwise. A client that declares no such capability —
or that refuses the registration — gets the fallback instead: the server
re-stats a file it has INDEXED before it answers a discovery read from
its copy, and re-reads it under the same 16 MiB bound when the
`(length, mtime)` moved. An OPEN buffer is never re-read either way: the
buffer is the master while it is open (LSP 3.17), so nothing a user is
typing is overwritten and no keystroke pays a `stat`.

## Error explanations (RFC 0010)

Hovering a squiggle shows the diagnostic's **explanation summary** — the
meaning paragraph from the error index, right in the hover:

> **NML2007** — *Missing required field.* Fields are required unless
> marked `?` or given a default; the instance omits one.
>
> *Run `nml explain NML2007` for the full entry.*

The summary appears after any regular hover content for the position (or
alone, highlighting the diagnostic's range). Content comes from the same
embedded index as `nml explain` — offline, always in sync with the
binary, covering every stable code.

The **full entry** opens in-editor: every coded diagnostic offers an
**Explain NML2007** code action (the 💡 lightbulb) that renders the
complete index section — meaning, runnable examples, the fix — as a
markdown preview beside your code. The **NML: Explain a Diagnostic
Code** palette command opens the same entries for codes you *can't*
hover (CI output, a teammate's log): it lists every code with its
summary, searchable by either. Explanations always come from the exact
server that produced the diagnostic — a provider tool (`nudge lsp`)
explains with its own binary's index — so error and explanation can
never version-skew, in any channel, offline.

For other LSP clients: the server emits the code action only to clients
that declare a command id in `initializationOptions.explainCommand`, and
serves the content over two custom methods — `nml/explain { code } →
{ markdown } | null` and `nml/explainIndex {} → [{ code, headline, summary }]` (`headline` the bold
lead the entry opens with — the palette's row; `summary` its first
paragraph).
Web links on codes (`codeDescription`) join at publish day — the last
tier of RFC 0010.

## Completion

Schema-driven: block keywords from the resolved schema context, fields
with types and defaults, enum variants, `oneof` discriminators, union
variants in the `as`-type slot, language keywords, and directive names
under a covering package — the language's four merge-policy directives,
then the package's declared vocabulary — each drawn from the same
candidate sets the validator checks.

## Navigation and hover

Go-to-definition for references, keywords, and model fields; hover
documentation from schema definitions and leading `//` comment blocks;
document symbols. A bound document's position `(0,0)` hover shows its
schema-package binding (package, version, content hash, binding).

## Formatting

`nml fmt`'s canonical, comment-preserving formatting as the document
formatter — including canonical `" & "` conjunction spacing and `as`
annotation preservation.

A document that does not parse is never formatted: the formatter
returns no edits (never a lossy rewrite — a block the lowering could
not place is an NML0002 error in the parse band, not a line silently
dropped) and says why once, as a warning in the server log (`NML:
formatting skipped for stray.nml: 5:5: [NML0002] …`), not as a
notification: the parse finding already marks the line, and
format-on-save would otherwise raise a toast on every save.

A new line after a block header (`key:`, Enter) is indented by the
unit the document nests by — the block's own step, the document's, the
canonical four when the document offers none — the same rule every
structural insertion follows, so the cursor lands where a quick fix
would put a nested line. The client's `tabSize` is not consulted: it
is the client's guess at the document, and tabs are never NML
indentation.

## Schema packages

Documents covered by a schema package (nudge RFC 0030 lineage) validate
against the package's composed
schemas automatically; `.model.nml` and `.schema.nml` files — the two
schema-source spellings the kernel admits — feed the workspace registry
in open mode.

A document inside an open workspace folder resolves under that folder —
the folder fixes the universe (the tree under the workspace root, the
world a manifest's `files` globs claim in), as `--root` does on the CLI. A document
outside every folder resolves under the root the kernel derives for it,
exactly as `nml check` does without `--root`: within the `.git` fence
above the file (its own directory when there is no VCS), never a
re-rooting at the file's own parent, under the same fail-closed rules —
a derivation the kernel refuses (a root marker above a fence that is no
directory; no fence within the bound) leaves the document unbound with
one row, `cannot derive a workspace root for this document: … — open its
workspace folder, which fixes the universe`. The `nml/schemaInfo`
request says how the root was fixed: `rootOrigin` is `editor` for a
folder and `derivedVcsFence` or `derivedTargetDir` for a derivation;
for a derived root `rootFence` names the fence entry's kind (`dir`,
`file`, `symlink`, `other`) and `rootShadowed` the entry above the fence
that shadows the universe — another `.git`, or a manifest above a
directory fence — spelled from the root (`../../demo.package.nml`), or
null: what the CLI's `note: workspace root …` line says; the bundled VS
Code extension's status-bar tooltip says it beside the root.

A file two live manifests claim is refused as `nml check` refuses it:
one NML2087 row on the document — the CLI's sentence, naming every
claimant — and no other finding; nothing is validated or composed until
an operator narrows one claim, and `nml/schemaInfo` answers `bound:
false` with that row as its one note. A document whose path no key can
carry (more than 64 components below the root, a component that is not
UTF-8) is refused with the kernel's sentence as its one error row, as
the CLI fails that target. Navigation (document symbols, hover) still
answers on a refused document: what is withheld is the verdict.

The pin and opt-out code actions on an auto-associated document write
into the nearest LIVE `nml-project.nml` at or above it — the file the
kernel resolves pins from, an unsaved buffer at that path included —
never a config inside claimed content (inert, NML2080), and create one
at the binding's anchor when none is live — in canonical form, the file
`nml fmt` writes, since a new file has no indentation to read — for a
client that declared the `create` resource operation; an edit into an
existing config nests by that config's own indentation. Every edit the
server hands out takes
the shape the client declared (`documentChanges` naming the document's
version, else plain `changes`) and is the one inserted hunk, never a
whole-file rewrite.

`nml/schemaInfo` answers `layers` beside the binding — the `--json`
`binding` row's object, one spelling: `{ "granted": false }` for a
binding without a grant, `{ "granted": true, "allowRefs": […],
"denyRefs": […], "maxStackDepth": n | null }` for one with it — so what a
denial's quick fix will produce is readable before it is applied; the
`(0,0)` hover carries the same line (`layers: granted — allowRefs[0] =
"tenants/**"` / `layers: none — composition denied (NML2064)`), and the
VS Code status tooltip prints it as `Layers:`. A manifest that failed to
load is reported AT its first finding on the manifest document (the
glob a grant rule refuses, the property meta-validation refuses, the
token a parse error names) and, on a content file it governs, at the
top with that location as related information; a manifest the server
could not read at all has no location and sits at the top alone.

## VS Code extension

The bundled extension adds editor packaging on top of the neutral server:

- **Status bar** (bottom-right): for the active `.nml` file, shows the
  governing schema package and version. Hover for the content hash
  (`blake3:{hash8}` plus the full hash), the schema's source (`Source:` —
  the kernel's own word for it), binding, and
  server label — the auditable chain from squiggle to store slot described
  in the [shipping tutorial](../tutorial/09-ship-schemas-to-your-users.md).
  A document the server refuses to validate shows `nml: not validated`
  on the error background, the reason and the remedy in the tooltip; a
  warning note, or a manifest above a derived root being ignored
  (`rootShadowed`), colours the item as a warning.
- **NML: Show Language Server Log** / **NML: Show Language Server Trace**:
  palette commands to open the structured log channels.
- **`nml.server.path`** (machine-scoped): absolute path to a native `nml-lsp`
  binary; a leading `~/` expands to your home directory (`~/.cargo/bin/nml-lsp`
  is the documented form). Other relative paths are refused. Paths inside an
  open workspace folder are refused.
- **`nml.trace.server`** (machine-scoped): set to `messages` or `verbose`
  to record LSP protocol traffic in the trace channel (off by default).

Host API compatibility (`engines.vscode` vs `@types/vscode`) is documented in
[`editors/vscode/VSCODE-API.md`](../../editors/vscode/VSCODE-API.md).
