# Glossary

One word per concept, across four surfaces: the **CLI**'s human output, the
CLI's **`--json`** wire, the **editor** (language server + VS Code
extension), and the **docs**. Every row below is what those surfaces
actually print — if you change a word in one of them, change it here and in
the others, or the concept has two names again.

Contributors: `CONTRIBUTING.md` points at this page. When you add a concept
an operator reads, add it here with the exact string the surface prints.

## Workspace resolution

| Term | What it is | Where you meet it |
|---|---|---|
| **workspace root** | The directory every binding glob anchors under. Given by `--root`, else derived. | CLI `note: workspace root . (derived: …)`; `--json` `root.path`/`root.origin`; editor tooltip `Root: …` |
| **fence** | The `.git` entry the root derivation stops at. Nothing above it is searched. | CLI `no .git fence found`; `--json` `root.fence`; `nml --root --help` |
| **universe** | The set of manifests and files a run resolves against, and the answer to whether anything governs a file. Exactly two values: **closed** (a package manifest was found — only the files a `files` glob claims are governed) and **open** (no manifest within the fence — composition is permitted). | CLI `none — closed universe (2 manifest(s) discovered)` / `none — open universe (no manifest within the fence)`; `--json` `universe: "closed" \| "open"`; editor `SchemaInfo.universe` |
| **budget unit** | A directory subtree with its own walk allowance — the start of a claiming glob's last run of wildcard segments, else the root. A unit that overruns is denied alone; the rest of the universe stands. | NML2089; `nml limits` (`MAX_ENTRIES`, `MAX_LIVE_INPUT_BYTES`); `--json` `truncatedUnits[]` |
| **claim** | What a manifest's `files` glob does to a file. A file may be claimed by two manifests, which is an error (NML2087). | CLI `no files glob claims this file`, `2 manifests claim this file`; editor `…no glob claims this one` |
| **govern** | What the ONE binding that won does. A file is governed by at most one binding, ever. | CLI `no binding governs this file`; editor `No schema package governs this file.` |
| **binding** | A named `[]validator` entry in a manifest: the globs it claims, the schemas it enforces, its strictness and its grant. | CLI `binding tenantFlows`; `--json` `binding.name`; editor tooltip `Binding: tenantFlows (auto-associated)` |
| **grant** | A binding's `layers:` permission to compose (`allowRefs`, `denyRefs`, `maxStackDepth`). Absent means composition is denied (NML2064). | CLI `layers   granted — allowRefs[0] = "tenants/**"` / `none — composition denied (NML2064)`; `--json` `layers`; editor tooltip `Layers:` |
| **pin** | A `schemaPackages` entry in the nearest live project config, choosing between package *names*. A pinned binding's step is `pinned`; otherwise it is `auto-associated`. | CLI `(auto-associated)`; `--json` `binding.step`; editor tooltip `Binding: … (pinned)` |
| **source** (of a schema) | Where the governing package came from: `workspace manifest`, `store current`, `in-binary`, `builtin`. | CLI `binding` row, after the hash; `--json` `binding.class`; editor tooltip `Source: workspace manifest (demo.package.nml)` |
| **inert** | A project config that sits inside content a binding claims: content, not configuration, so its pins and settings are ignored. | NML2080 |

## Editing surfaces

| Term | What it is | Where you meet it |
|---|---|---|
| **language server** | The program that answers LSP. Three kinds: the **built-in** one (bundled as WebAssembly), a **native** `nml-lsp` (`nml.server.path`), and a **project's tool** (`<tool> lsp`). | Extension README; every toast; `NML: Restart Language Server` |
| **provider** | The `provider:` block in `nml-project.nml` by which a project declares its tool, and the ladder that resolves it. In operator-facing text the thing itself is called *the project's language server* or *the project's tool*; "provider" appears only where it names that block. | `nml-project.nml`'s `provider:`; `nml-project.nml's provider declaration changed` |
| **approval** | The operator's remembered answer about one project's tool, for one workspace, valid until the declaration or the resolved path changes. | consent prompt; `NML: Forget Language Server Approvals` |
| **stood down** | A provider the editor stops trying for the rest of the session (it never answered, or answered without naming itself). A window reload clears it; a restart does not. | `"nudge" was stood down earlier in this session. Reload the window to try it again.` |

## Bounds

| Term | What it is | Where you meet it |
|---|---|---|
| **bound** | A published number the toolkit enforces. `nml limits` is the surface; each row names what it bounds, its value, its **reach**, what it **guards** and its **surface**. | `nml limits`; `nml limits --json` `type: "limit"` |
| **reach** | Who can drive a bound: `content` (a file, manifest, glob or tree a tenant commits), `peer` (the process at the other end of the editor's wire), `internal` (a guard no result can reach — never published). `operator` is a class of the scheme with no member in this tree. | `nml limits` legend; `--json` `reach` |
| **limit** | Used for the same numbers where the surface is the `nml limits` verb or a message about one (`--max-findings`, `NML0007 Nesting limit exceeded`). *Bound* is the word the table's prose uses; they are the same thing. | `nml limits`; `note: 5 more finding(s) not shown (limit 1; …)` |
| **cap** and **budget** | `cap` is an implementation word and reaches no user-visible string. `budget` means only a **budget unit**'s allowance, or the finding-printing budget behind `--max-findings`. | — |

## Words this project does not use

* **context** for the open universe. One field (`universe`) carries `open`
  and `closed`; both CLI branches say *universe*.
* **channel** for where a schema came from — that is its **source**. In the
  VS Code extension, *channel* means an output channel (`NML: Show Language
  Server Log`).
* **match** for what a manifest does to a file: a glob *matches*
  (`nml binding`'s `anchor   .   matched files[0] = "tenants/**/*.flow.nml"`),
  a manifest *claims*, a binding *governs*. Everywhere else *matches*
  belongs to values and schemas (NML2017 *Default discriminator matches no
  arm*, NML2032 *No union variant matches*), never to files and manifests.
