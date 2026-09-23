# Changelog

## [0.1.0] - Unreleased

**Upgrading — the four changes that ask something of you**, worst first.
Everything else below is an improvement that arrives on its own.

1. **A schema-provider tool built before this release is stopped each
   time the updated VS Code extension starts it**, until it is rebuilt.
   Your approval of it is kept. The extension now requires `initialize`
   to name an NML language server (`serverInfo.name = "nml-lsp"`), and a
   `<tool> lsp`
   built against an NML from before that field existed answers with no
   name at all — so the extension stops it, says so, and falls back to the
   bundled server. Nothing is lost permanently and no file is touched, but
   schema-aware editing against that tool stops until it is rebuilt.
   *If you ship such a tool:* rebuild it against a current `nml-lsp` and
   release it — no code of yours changes. *If you use one:* update it, or
   stay on the built-in server, which validates committed schema and needs
   no setup. Two further requirements land with it: a provider is launched
   in an empty private working directory, and with `LD_*`, `DYLD_*`,
   `NODE_OPTIONS`, `BASH_ENV`, `PERL*`, `PYTHON*`, `RUBY*` and the
   `RUSTC_*` wrappers REMOVED from its environment — a wrapper script that
   relied on either will break, and has to resolve its paths from the URIs
   the client sends instead.
   [What the editor requires of a provider](docs/guides/embed-the-lsp.md).

2. **`nml fmt` writes a different file than it used to.** It preserves
   what the author wrote — blank lines, the line break before a value,
   the string delimiter, an aligned `->` column, CRLF — and it no longer
   aligns arrows for you. *Action:* run `nml fmt --root . .` once over
   your tree and commit that diff BEFORE putting `nml fmt --check` in CI.
   The diff is layout only: never structure, never a value. The style is
   now written down, normatively, in [`spec/style.md`](spec/style.md),
   whose §9 is that adoption note and whose §4 says plainly what the
   no-alignment rule costs (the formatter will not repair an alignment
   your edit broke).

3. **`nml_lsp::serve` and `serve_stdio` now return `nml_lsp::SessionEnd`**
   (they returned nothing): how the session ended — `Exited` (after
   `shutdown`), `ExitedWithoutShutdown`, or `Disconnected` (the client
   closed the pipe). *Action:* end your process with it. `exit_code()`
   is the code the protocol prescribes for the `exit` notification (0, 1,
   0 respectively), for a `main` that returns `ExitCode`; a tool with its
   own closed set of exit codes maps the ending onto that set instead
   ([the recipe](docs/guides/embed-the-lsp.md) shows both). Only the
   process's own `main` can deliver the code to the operating system —
   which is why this one is above the removals below: **nothing in your
   build will tell you.** Ignoring the value still compiles, and your
   `<tool> lsp` goes on reporting success after an `exit` that never had a
   `shutdown` before it, where the protocol prescribes 1.
4. **Three library entry points are gone:** `nml_fmt::formatter::format`
   and `format_with_comments`, and `nml_core::cst::parse_with_comments`
   with its `Comment` type. *Action:* call
   `nml_fmt::formatter::format_source` (source text in, canonical source
   text out, comments preserved) and `nml_core::cst::parse_checked` (the
   checked lossless tree). The whole public surface is now recorded item
   by item in [`docs/api/`](docs/api/), so the exact change between two
   revisions is a diff of those files;
   [`docs/stability.md`](docs/stability.md#the-public-rust-api-record)
   explains the record, its stamp and its ledger.

**How this version is laid out.** The four groups directly below are what the
review rounds landed. Two collapsed sections follow them: the round-by-round
engineering log behind those groups, and — 0.1.0 being the first release —
the original feature list, which nothing above replaces. Release notes for
0.1.0 are both: these groups say what changed, that last section says what
the release IS.

Two ledgers run through the groups below, each one entry per version and
the latest first. The WIRE's (`--json formatVersion 1, revision N`)
closes the FIRST Added group; the public Rust API's (`public API apiVersion A,
revision R`) closes the FIRST Changed group, where a break belongs. Both stamps
move once per reviewed step, not once per release, so within one
unreleased cycle they read as a work log — what an upgrading consumer
actually faces is the summary above and the diff of the record. The
Security group lists every change that made a silent outcome a refusal.

### Added

- **The wasm editor survives a real workspace.** The bundled language
  server went permanently silent on any workspace it listed twice: VS
  Code's WASI host returns `EBADF` from `fd_close` for a MOUNT ROOT's
  descriptor — the root is opened by the relative path `"."`, which the
  host's node table hands back without taking a reference while the close
  releases one — and std's `ReadDir` panics in `Drop` when `closedir`
  fails, which with `panic=abort` kills the guest. Every workspace folder
  is a mount root and every discovery lists one, so the server answered a
  few requests and died; every downstream symptom (diagnostic timeouts)
  pointed away from the cause. Listings now go through `rustix::fs::Dir`,
  whose `Drop` ignores `closedir`'s result while the descriptor is still
  released — nothing leaks and nothing aborts — and a source ratchet holds
  every listing in the crate to that one wrapper. REMOVED with it: the
  handle counter, the documented leak budget and the `mem::forget` they
  bounded, which made the leak smaller but never finite. A per-operation
  listing memo replaces the budget's other job: within one operation a
  directory is listed at most once, so a walk cannot see a torn tree, and
  across operations a directory is re-listed only when its size or mtime
  moved. `docs/upstream/wasm-wasi-core-fd-close-ebadf.md` carries the
  upstream report.

- **The first file opened after the editor starts shows its findings.**
  The language server sweeps the workspace in its `initialized` handler,
  off the handshake's critical path, and tower-lsp runs handlers
  concurrently — so the file an editor opens the moment it starts was
  pulled against the unindexed universe (an instance beside its sibling
  model: no findings), and a pull client pulls again only on an edit, a
  focus or a `workspace/diagnostic/refresh`. In VS Code the first `.nml`
  file stayed clean until it was re-shown. Once the sweep stands, a
  client that declared `refreshSupport` is asked to pull again — only
  when a buffer was open during the sweep, once — and the re-pull is
  judged under the index.

- **The VS Code extension's first run tells the truth, and its words are
  the CLI's.** The walkthrough opened with "Install the language server"
  and `nml.server.path` said it "defaults to ~/.cargo/bin/nml-lsp", while
  the extension ships the neutral server as WebAssembly and starts it with
  nothing installed (that path is the fallback of a build that bundled no
  server). The walkthrough now opens on "Open an NML file", a native binary
  is the optional last step, and the setting says what empty means. The
  status-bar tooltip spelled a derived root in the words the CLI's `note:
  workspace root …` line deliberately avoids ("the file's own directory is
  the universe"); it now says what the CLI says ("… is the workspace root —
  open the workspace folder to choose one"). A server that fails to start
  names the remedy for ITS kind — the WASI host extension for the bundled
  server, the path for `nml.server.path`, the project's declaration for a
  provider tool — with a Show Log button, where every kind was told "set
  nml.server.path, or install one". `nml explain --list` printed one line
  at 82 columns (NML2092): the headline pin now bounds the printed line,
  and the entry's lead fits it. The `nml_validate::workspace` example
  continues past the verdict to the validation and its `suggestions`, so
  the library path needs no source reading.

- **`nml fix` names the remedy `nml check` really shows.** Refused at
  the door of a manifest that failed to load, or refusing an edit that
  lies in another file, it said "paste the `help:` block `nml check`
  prints" — but that block prints only for an insertion; the door's
  usual case, a did-you-mean in the manifest, is a replacement rendered
  inline, and `nml check` printed no block to paste. Both sentences now
  say "take the did-you-mean or paste the `help:` block `nml check`
  shows there, or apply the editor's quick fix". An unknown command
  ("chekc") reached stderr without the `error:` prefix every other usage
  error carries; it carries it now. A dry-run diff of an absolute path
  labelled itself `a//var/…`; the leading separator is dropped, as git
  spells one.

- **`nml explain --list` fits a terminal.** Each line carried its
  code's whole first paragraph, Markdown emphasis included: 136 of the
  137 lines ran past 80 columns and the longest (NML2088) was 2,235
  characters — 28 wrapped lines for one entry on an 80-column terminal.
  The list now prints each code's one-line headline, the bold lead every
  index entry opens with (`nml_core::diagnostic::explain_headline`; a
  unit test holds every entry to that shape). The `--json` row is
  unchanged — `summary` is still the paragraph, `document` the full
  entry — and the editor's `nml/explainIndex` gains `headline` beside
  `summary`, which the VS Code palette now shows per row (the paragraph
  stays searchable as the row's detail).

- **`nml fmt` takes the shape of every sibling verb.** It formatted one
  file, named a directory "is a directory" (exit 1) while `check`, `fix`
  and `validate` walked it, had no `--check` — the one standard CI
  workflow with no answer — and printed its success line under `-q`
  against `-q`'s own page. It now takes `<path>...`; a bare file is
  formatted at its leaf as before (no tree is walked — a file beside an
  unlistable neighbour still formats, as every formatter in the state of
  the art formats a named file), while a directory target or `--root`
  walks the tree through the one workspace door the other verbs use (a
  path a closed binding rejects is never opened; a closed universe's
  write lands at the key through the parent descriptor, as `fix`'s
  does); it prints a unified diff under `--dry-run`, and gates under
  `--check` — nothing
  written, exit 1 on a file not in canonical style or on `.nml` content
  the walk skipped (NML2090) — spelled as rustfmt, black, prettier and
  gofmt spell it. A set run closes with one tally (`formatted K of N
  file(s)`); a single file's own line is its verdict. The `--json` `fmt`
  row is unchanged (one per file); the closing row carries `dryRun`, an
  existing property. `--dry-run` and `--check` say what THIS verb's dry
  run shows (`Spec::edit`): the flags' help no longer speaks of fixes on
  the formatter's page.

- **The editor keeps up with the disk without a file watcher.** Freshness
  rested entirely on the client's `didChangeWatchedFiles` registration —
  whose result the server discarded, and whose capability it never read —
  so a client that cannot watch was stale forever: a manifest repaired
  outside the editor kept its NML2088 on every file it governs, and a
  cross-file quick fix minted on the old text would have spliced at
  offsets the disk no longer had. The registration's answer is honoured
  now, and without a watching client the server re-stats an INDEXED file
  before answering a discovery read from its copy, re-reading it under
  the same bound when it moved. An open buffer is never re-read (it is
  the master while it is open) and a watching client pays nothing.

- **`nml fix` says its verdict at the door.** Refused because the universe
  cannot be trusted — a manifest that failed to load, a truncated walk —
  it printed the finding and stopped: no tally, no "nothing was written",
  and no word that the edit the finding's own did-you-mean names is one
  this run will not make. It now closes with that verdict and with where
  the pending edit is, as it does after a round; `--json` carried the
  number already, and `-q` stays silent.

- **A `--schema <dir>` that holds no schema source says so.** The
  directory listed, nothing in it was spelled `*.model.nml` or
  `*.schema.nml`, and the run printed `ok` — a green gate over a
  validation that loaded nothing. It is not a mistake by itself (a
  self-validating file carries its own `model`), so it is DISCLOSED, not
  refused: one `note:` line before any target on the human run, and
  `schemaSources` on the closing `--json` row. Exit codes are unchanged on
  every path.

- **A declared schema source is spelled as one, or the manifest is
  refused.** A `[]schema` entry's `file` must end in `.model.nml` or
  `.schema.nml` — the one admission the walk, the `--schema` directory
  scan and the editor already shared. A source declared under any other
  name loaded: `nml check` judged its directives while the editor, which
  gates its registry, its schema passes, completion and hover on the
  spelling, opened no pass and showed no row on the same buffer. Refused
  at load now, at the `file` value, under **NML2105** (the manifest's
  NML2088 row carries it as its `cause`), so every reader agrees on what
  a schema source is.

- **The directive vocabulary is the kernel's, and both front ends ask it.**
  The language's four merge-policy directives (`#sealed`, `#identity`,
  `#append`, `#overlay` — RFC 0019) are the base of every directive
  vocabulary; a package's `[]directive` entries extend it and never replace
  it (`nml_validate::directives::Vocabulary`,
  `nml_core::layers::BUILTIN_DIRECTIVES`). The editor used to flag
  `#sealed` as NML5000 under any declared vocabulary while `nml check`
  checked no directive at all; now one kernel judge answers both:
  `nml check` and `nml validate` report NML5000/5001/5002 (and the
  NML5003 note) on a schema source a package covers — the same rows, the
  same sentences, the same did-you-mean as the editor, whose private pass
  is gone — and the editor's completion after `#` and its directive hover
  render the builtins beside the declared entries. Which package covers a
  source is the kernel's answer too (`Discovery::vocabulary_for`). A
  manifest that redeclares a language directive is refused at load
  (**NML2082**, at the entry), and the row that says so where a file
  under it is checked rides **NML2082** itself — the rule's own code,
  as the grant's NML2081 rides — so a consumer filtering by code lands
  on the rule and not on the generic "failed to load". The editor admits a schema source by the
  kernel's spelling too (`*.model.nml` and `*.schema.nml`,
  `nml_validate::workspace::is_schema_source_name` and
  `schema_source_stem`): a `.schema.nml` buffer feeds the registry,
  gets the schema passes, directive completion and hover exactly as a
  `.model.nml` does — it used to be judged by `nml check` and not by
  the editor.
- **Inserted blocks nest by the nearest indentation the file offers.** A
  structural insertion (the grant quick fix, the pin and opt-out actions)
  nests its lines by the block's own step, else the nearest block's
  around it, else the document's one step, else the canonical four
  spaces — a two-space block beside a four-space one keeps nesting by
  two, and a file that nests two ways never gets a third width. A
  config the editor creates is in canonical form (the formatter's fixed
  point), and a new line after `key:` is indented by the same rule.
  `nml_core::cst::INDENT_UNIT` is the one spelling of the canonical unit
  (`nml fmt` reads it too).
- **NML2093 — a repeated entry name in one body is an error**, in either
  spelling (`version` twice; a block `files:` beside an inline
  `files = […]`), at the later entry with the first as a `note:`
  (`relatedInformation` in the editor); no fix is offered — which entry is
  meant is unknowable. Every body: instance bodies, model bodies (a field
  defined twice), manifests, and every body beneath. The rule is judged
  ONCE, beside every parse — in the kernel's one text→tree pipeline — so
  no consumer can skip it: `check`, `validate` and `fix` report it as a
  parse finding and stop there, `fmt` writes nothing, the editor shows it
  in the parse band, a package's schema sources carry it into the load,
  a manifest is refused where it is parsed, and an embedder's
  `nml_core::parse` returns it — a document whose meaning is ambiguous is
  never judged further. An overlay redefining a base property stays
  composition; a repeat inside the overlay's own body is the error. A
  manifest's `[]schema` sources and `[]validator` bindings are named once
  as well (the `[]schema` refusal is located now), and a second
  `[]validator` array or `package` block is refused instead of silently
  replacing the first.
- **NML1000 is the same rule at the file scope**, judged in the same pass:
  a second top-level declaration under one name — block, array, `const`,
  `template`, `oneof`, whatever the keywords — is an error at the later
  NAME (it was the whole declaration) with the first as a `note:`, and a
  parse finding on every front end (it was re-derived by `check`,
  `validate`, `fix` and the editor each on their own, in hash order, and
  by none of `fmt`, the manifest loader or an embedder). A manifest's
  second `package demo:` is this error.
- **NML2054 — an arm field named like the discriminator is an error at
  schema load.** An instance's property of that name is always read as
  the discriminator, so the field can never be set: required, it made
  every instance fail with a missing-field error on a property the
  instance states; optional, it was a declaration nothing could fill —
  and it warned. Now the schema does not load (a binding's file is
  NML2091 with this finding as its note), the row sits at the FIELD in
  the file that declares it, the union's declaration as a note, and it
  carries the field's deletion as a machine-applicable fix — `nml fix`
  applies it, the editor offers it. A required `#sealed` field of that
  name is the same shape wearing the seal's directive (it was exempt,
  and unsatisfiable): refused, with the `?` that makes it the sanctioned
  optional spelling as its fix. A field reaching the arm through `is` is
  refused at the arm's `is` reference, naming where it is declared, with
  no fix — it may be live in the mixin's other users. The optional seal
  (`kind string? #sealed`) and the modifier form (`|kind`) stay. With
  it, every field-anchored finding renders at the field, not at its
  indentation.
- **`nml fix` never manufactures a repeated name.** A did-you-mean into a
  name the body already declares would leave text that does not parse;
  the re-check gate's first clause discards the round and the file is
  untouched.
- **One derivation for every place in a manifest.** A `note:` beneath a
  universe row (the first `files` of a repeated entry, NML2091's failing
  line in a manifest) is located by the kernel over the text the verdict
  read — a loaded package's, or a failed manifest's kept for this — the
  same derivation that locates the row; the CLI no longer re-reads the
  file for it, and an edit into a manifest resolves against those bytes.
- **The editor fixes a denied composition from the denial.** NML2064's
  missing `layers:` grant rides the finding as a structural suggestion
  (`kind: insert`, the manifest as its `source`) and becomes a quick-fix
  that inserts the block under the binding in the MANIFEST — offered on
  the denial and on the manifest at the binding — as a versioned edit
  (`documentChanges` naming the manifest's document version; plain
  `changes` for a client that declared none); a stale denial offers
  nothing and a granted binding is never doubled. The block nests by
  the manifest's own indentation unit and ends its lines as the manifest
  does.
- **The editor asks for a refresh when the universe changes.** A pull
  that rediscovers the universe — a manifest or project-config buffer
  opened, edited or closed — sends a client that declared
  `workspace.diagnostics.refreshSupport` one `workspace/diagnostic/refresh`
  (LSP 3.17), so the other open documents' reports, and the actions
  offered from them (the grant on the manifest), are current without a
  refocus; a client without the capability heals on its next pull as
  before.
- **`nml/schemaInfo.layers`** — the binding's grant in the `--json`
  `binding` row's one spelling (`granted`, `allowRefs`, `denyRefs`,
  `maxStackDepth`); the `(0,0)` hover and the VS Code status tooltip
  carry the same line.
- **An unloadable manifest's row lands at its first finding** on the
  manifest document, and a content file it governs carries that location
  as related information.
- **The formatter says why it declined.** A document that does not parse
  gets no edits and one warning log line naming the file, line and code —
  never a lossy rewrite, never a toast on every format-on-save.
- **`nml check`, `validate`, `fix` and `binding` resolve files through the
  workspace.** A `<name>.package.nml` at the root claims files by glob; each
  file validates under the binding that claims it, with that binding's own
  strictness, in the CLI and the editor alike. Pass `--root <dir>` in CI;
  without it the root is derived from the file — the outermost manifest or
  `nml-project.nml` between it and its `.git` fence, else the file's own
  directory — and any derivation you could not infer is disclosed once on
  stderr (`note: workspace root …`).
- **`nml binding <file>`** — who governs a file and why: the key, the root and
  how it was fixed, the matched glob, the composition grant and every inert
  config on the file's path. The universe's own word (a manifest that failed
  to load, a truncated walk, a budget-unit gap) is stated once per run before
  the first block, exactly as `check` states it. Exit 0 bound, 1 unbound or
  ambiguous, 2 error.
- **Directory targets.** `nml check --root . tenants/` checks every `.nml`
  file the workspace walk sees under `tenants/`; what the walk skipped (links,
  dot-directories, FIFOs, `node_modules`, `target`, an entry whose name no key
  can carry, a directory at the 64-component bound) is listed on the closing
  row, and `.nml` content among it fails the run (NML2090) so a green gate
  means the whole tree was looked at — never silently less.
- **A closing line on a run over a set.** A run that named a directory, or
  more than one path, ends with `checked N file(s): N ok` (`validated …`)
  when every file passed, and with `error: K of N file(s) failed` when any
  did not; a single file's own `ok` line is its whole verdict.
- **A copyable remedy beneath a finding, and on the wire.** A finding may
  carry the edit that remedies it — rustc's structured suggestion, the
  kernel's machine-applicable edit in the file it belongs to; NML2064
  (composition not permitted, the binding grants nothing) is the first:
  the `layers:` block to add under the binding in its manifest, its
  `allowRefs` entry the key the clause reaches. The CLI prints it beneath
  the row as `help:`, resolved against the manifest — at the binding
  body's own indentation, naming the line it goes after — so a terminal
  copy pastes as printed (the ASCII fold spells a key's own glyphs as the
  `\u{…}` escapes the manifest reads back, never as the tool's
  typography); `--json` diagnostic rows carry `suggestions` (`[{kind,
  source, edits[{line, col, endLine, endCol, lines}]}]` — every
  machine-applicable edit a finding carries, did-you-means and fixes
  included, resolved against the files as the run read them; an addition
  at `formatVersion` 1, in the JSON Schema); the editor offers the block
  as a quick-fix on the manifest. `nml fix` edits the file it was given,
  only: an edit that lies in another file is refused, naming it, and
  counted as `routed` — on the per-file `fix` row, on the closing row
  beside `remaining`, in the tally and in the `--check` gate's reason —
  so a CI consumer sees an operator-side change pending rather than a
  finding "no fix repairs". The
  denial's located `note:` names the key to admit and no longer points at
  `nml explain` for the block. Under a manifest nobody edits here — the
  store's current copy of a package, a package embedded in the binary,
  nml's builtin — the denial's one line says where the manifest lives
  and what change it needs (republish the package, rebuild the embedder,
  write the manifest without `uses`) instead of calling it "an operator
  change".
- **Eleven new codes for the workspace**, each with an `nml explain` entry and
  an executed example: NML2064 composition not permitted, NML2065 layer
  reference denied, NML2080 inert config, NML2081 layer grant rule,
  NML2083 symlinked path rejected, NML2087 ambiguously claimed
  file, NML2088 manifest failed to load, NML2089 universe not enumerable,
  NML2090 content skipped unjudged, NML2091 binding cannot build its
  validator, NML2092 budget-unit gap.
- **`budgetUnits`** in the package manifest declares which subtrees are one
  tenant's blast radius (`budgetUnits = ["tenants/*"]`), so one tenant's flood
  denies that tenant alone; a glob whose shape leaves a gap gets NML2092 with
  the declaration spelled both ways.
- **`--json` on every verb**, line-delimited, opening with a `contract`
  row (`{formatVersion, revision, nmlVersion}` — the format's number,
  the additions within it, the binary; the same three ride the closing
  `summary` row) so a consumer reads what it is about to parse before
  the first finding and a strict validator fails on that row, never on
  a finding; the closing `summary` row carries the exit code, the root
  and exact counts; the contract is a published JSON Schema
  (`docs/json/nml-ndjson-v1.schema.json`) whose `revision` is a constant
  on both rows, `stability.md` states the consumer rule (pin
  `formatVersion`, validate strictly only at a `revision` you know, else
  ignore unknown keys), columns are stated as byte columns, and a
  `skipped` row now says `unkeyableName` (with `entry`, the name) or
  `componentBound` where the walk could not name or descend an entry.
- **A `--json` row that wraps another finding carries it as `cause`.** A
  manifest that fails to load (NML2088) and a binding that cannot build
  its validator (NML2091) report another finding's refusal under a code
  of their own; the row now carries that finding as `cause` — `{code,
  source, line, col, message}`, its code never null, its place in its
  own file — so a CI consumer tells a repeated name (NML2093, NML1000)
  from an unknown property (NML2001) or a parse finding without parsing
  the sentence (`cause?.code ?? code` is the code to act on). A row
  reported under the inner finding's own code (NML2081, NML2082) wraps
  nothing and carries none; a loader rule of the manifest's own shape rides as
  the cause under a code of its own too (NML2094–NML2104, next). The human lines
  are unchanged. `binding --json`'s `notes[]` rows now carry their
  `related`, `suggestions` and `cause` as `check`'s rows do (they rode
  empty). For embedders: `Diagnostic::cause`, `Diagnostic::caused_by`
  (the one rule) and `Cause` in `nml_core::diagnostic`.
- **Eleven new codes for the manifest loader's own rules**, each with an
  `nml explain` entry and an executed example, so a manifest that fails
  to load names the rule as the NML2088 row's `cause` and a consumer
  acts on `cause.code` without parsing the sentence: NML2094 repeated
  declaration (a second `[]validator` array), NML2095 missing
  declaration (no `package` block, no `[]schema` source), NML2096
  invalid package name, NML2097 unnamed entry, NML2098 empty binding,
  NML2099 undeclared schema, NML2100 invalid binding glob, NML2101
  budget unit rule, NML2102 unsupported format version, NML2103
  multiple manifests (a store slot), NML2104 template string in a list
  (the Security entry below). The loader's backstops for the
  meta-schema's required entries and enums report under the
  meta-schema's codes (NML2007, NML2000). New code VALUES only: the
  wire's shape and revision are as they were, the human lines unchanged.
  For embedders: `PackageError::gate_finding`, the formatVersion gate as
  a coded finding.
- **A wrapped finding's remedy rides the wrapping row.** A manifest
  that fails to load over a near-miss (`versio`) used to lose the
  did-you-mean: the NML2088 row carried the sentence and nothing to
  apply. The row now carries the finding's machine edit in the
  manifest's file — `suggestions[]` with `source` on the wire (a field
  since revision 1; no shape moves), the `(did you mean "version"?)`
  hint on the human line, `routed` on `nml fix`'s closing row (the
  edit is pending in the manifest; a run under a manifest that failed
  to load still rewrites nothing), and a quick fix in the editor on the
  manifest document and on every file it would govern (an action that
  edits another file names it in its title, whatever its kind).
  NML2091 carries its declared source's remedies the same way. For
  embedders: `Diagnostic::caused_by` stamps each carried suggestion with
  its file (its own, else the wrapped finding's, else the source given,
  else the row's).

- **`--json` formatVersion 1, revision 3** — the closing `summary` row
  gains `schemaSources`: how many schema sources a `--schema <dir>`
  invocation contributed, `null` when the run was given no `--schema`.
  `0` says the directory listed fine and held none, so every target was
  validated against its own definitions alone — a run that certified
  nothing used to be indistinguishable from one that validated everything.
- **`--json` formatVersion 1, revision 2** — the `diagnostic` row gains
  an optional `cause` object (`$defs/cause`: `code`, `source`, `line`,
  `col`, `message`), present on a row that reports another finding's
  refusal (NML2088, NML2091) and absent from every other row.
- **`--json` formatVersion 1, revision 1** — the contract 0.1.0 ships,
  whole: the twelve row types `docs/json/nml-ndjson-v1.schema.json`
  states (`contract`, `diagnostic`, `error`, `result`, `summary`,
  `binding`, `fix`, `parse`, `fmt`, `explain`, `limit`, `version`).
  Nothing shipped before it, so there is no earlier revision to name
  additions against. Every later revision has one entry of this shape
  naming what it added, and the docs gate holds the writer's number,
  the schema's constant, the generated shape record
  (`docs/json/nml-ndjson-v1.shape.txt`) and these entries to one value —
  a schema change without a bump, or a bump without one, fails the
  build.
- **`nml limits`** prints every bound the toolkit enforces — value, who can
  reach it, what it guards — the two input caps included
  (`MAX_MANIFEST_BYTES` 256 KiB, `MAX_SOURCE_BYTES` 4 MiB); `nml explain`
  takes several codes and `--list`.
- **`--max-findings <n>`** (default 512, fair per code; `0` prints all) and
  **`-q`/`--quiet`** on every verb; the counts and the exit code stay exact
  either way.
- **Colour on a terminal.** The severity prefixes — `error[…]`, `warning[…]`,
  `note:`, `help:`, `error:` — are painted in rustc's palette when stderr is
  a terminal; a pipe, a file or a CI log gets the same bytes as before.
  `NO_COLOR` turns it off anywhere, `CLICOLOR_FORCE` turns it on anywhere,
  `NO_COLOR` winning (no-color.org); `--json` is never coloured.
- **The editor resolves through the same core**: a document in a workspace
  folder resolves under that folder; one outside every folder resolves under
  the root `nml check` would derive, and says so in its log; `nml/schemaInfo`
  reports `rootOrigin`, `rootFence` and `rootShadowed`, and the VS Code
  status bar shows them beside the root.
- **The manifest `layers:` grant.** A validator binding declares its
  composition grant in the manifest — `layers:` with `allowRefs`,
  `denyRefs?` and `maxStackDepth?` — and both front ends judge `uses` under
  it: a veto is NML2065 by rule index, an allow-miss NML2065 naming the
  binding, an admitted stack composes; `nml binding` prints the rules by the
  same indices and the `--json` `layers` object carries them. The grant's own
  rules are NML2081 at manifest load, located at the item (a glob the matcher
  rejects, a stack cap past 16 or not whole, `denyRefs` beside an empty
  `allowRefs`); the block's shape stays NML2088. A binding without the block
  carries its remedy as a located `note:` at the binding in the manifest
  (`related[]` on the wire, a related location in the editor). Every loader
  finding now names its line and column.

### Changed

- **One word per concept, and a glossary that says which.** Four surfaces
  print the same facts — the CLI, its `--json` wire, the editor and the
  docs — and four words had drifted apart. `nml binding` now says
  `none — open universe (no manifest within the fence)` where it said
  *open context*: the `universe` field carries exactly `open` and
  `closed`, the other branch of the same row already said *closed
  universe*, and the editor reads the same two words. The VS Code status
  tooltip now opens its origin row `Source:` rather than `Channel:` —
  *source* is the word the kernel gives that label
  (`ClaimClass::label`), and *channel* in the extension already means an
  output channel the same tooltip sends you to. `nml limits`' legend
  names the classes the table actually prints (`content`, `peer`) and the
  one its closing line counts (`internal`), instead of naming `operator`,
  a class of the scheme with no member here. The vocabulary of all four
  surfaces is now written down in `docs/glossary.md`, which
  `CONTRIBUTING.md` points contributors at.

- **The real-editor suite's log shows which lane ran.** `just gate-ext-e2e`
  drives four VS Code launches over two test files — two on the bundled wasm
  server, two on a native `nml-lsp` — and every suite title read
  `WASM neutral server`, so the log named the wasm backend four times and the
  native lane was invisible in the one place a maintainer reads. Each title
  now names the backend its launch declared, and a source ratchet refuses a
  title that hard-codes one. The assertion that the lane really ran the
  server it declared is unchanged.

- **The status bar says what clicking it does.** The item runs *NML: Restart
  Language Server* on a click, and no state the operator reads said so —
  only the install-time walkthrough did. Every tooltip that does not already
  name the restart command now ends with it, pinned by a test that fails if a
  state says it twice or not at all.

- **A red gate names the file that made it red.** `just gate` printed one
  detail line under a failing recipe — a VACUITY warning saying "nothing in
  this workspace recompiled … the gate may have run against another tree's
  build artifacts" — and a path to a log. For a recipe that fails BEFORE
  rustc runs, which is every `cargo fmt --check` failure and every missing
  tool, that warning is the only line a contributor gets and it is the wrong
  diagnosis: it accuses their build cache of a fault their own change
  caused. The recompilation guard now runs where it means something (a gate
  that exited 0 and measured nothing), and a red gate prints `why:` — the
  first line of its log that names a cause — beside `log:`. `just
  gate-contract`'s self-test covers both halves over five log shapes.

- **The `--json` header the docs tell you to pin is the one the tool
  emits.** The two pages that teach the consumer rule — [the stability
  policy](docs/stability.md#the---json-stream) and [the CI
  guide](docs/guides/validate-in-ci.md) — showed
  `{"type":"contract","formatVersion":1,"revision":2,…}` while the writer
  was at revision 3, and the CI guide's row table stated the current
  revision as `2`. A consumer copying that header pinned a revision that
  fails on the first row of every real run — the exact failure the rule is
  designed to produce, from the documentation's own example. The
  transcripts already spelled the number through a placeholder; prose was
  held by nothing, and now is: `just gate-docs` reads every wire stamp
  stated in prose and fails on one that is not the writer's.

- **A refused `PATH` directory no longer points at a setting that cannot
  do the job.** When the editor refuses the directory a project's tool
  resolved in (world-writable, or owned by a third account), the log line
  used to end *"or point nml.server.path at a specific binary"*. That
  setting runs its binary with NO arguments, while a project's tool is run
  as `<tool> lsp` — so following the advice produced a program that does
  not speak LSP. The sentence now names the one remedy that works
  (install the tool where no other account can write) and says plainly
  that the setting is not a substitute.

- **Every `cargo install` in the documentation is one that works today.**
  The crate READMEs, the VS Code walkthrough and the extension's own
  "this build bundles no server" message said `cargo install nml-lsp` /
  `cargo install nml-cli`; neither crate is published yet (the top-level
  README says so), so that command fails with *could not find in
  registry*. All of them now use the `--git` form the README and the
  tutorial already used, and `just gate-docs` reads every `cargo install` of
  a crate in this workspace — in the guides, the crate READMEs, the
  extension's manifest, its walkthroughs and its TypeScript — and fails one
  that names no source. The check names itself as the thing to delete on the
  day the crates are published.

- **`nml fmt` keeps what the author wrote, and the language now has a
  written style.** The formatter re-emitted from the semantic AST with a
  side list of comments, and the AST does not hold the thing being
  formatted: it has no blank lines, no record of whether a value sits on
  its `=`'s line or the next one, and no idea where a comment sits
  relative to either. So `nml fmt --check` failed 18 of 18 tutorial
  chapters and 3 of 5 specification examples — a language whose own
  specification fails its own formatter has no canonical style. The
  causes, measured across every `.nml` file in the repository: 123
  deleted blank lines, 26 lines where `const X =` plus a triple-quoted
  string on the next line — the form `spec/syntax.md` teaches — was
  joined onto one, 8 hand-aligned trailing-comment columns collapsed, 8
  blank lines inserted where nobody asked, and 5 arms padded to a column
  their author had not chosen.

  The formatter is now a printer over the lossless CST: every significant
  token is COPIED from the tree and all layout is COMPUTED, with the
  author's own trivia in hand at each decision. What that buys, beyond
  the corpus: a comment can no longer be reordered or moved across a
  blank line (it is a token in the same stream as the code, where it used
  to be a side list re-interleaved by byte offset — a comment closing a
  block drifted below the blank line and onto the next declaration); a
  CRLF file stays a CRLF file, as `nml fix`'s insertions already did; a
  `template T:` body no longer re-indents one level deeper than the
  specification shows it, on every run; and a grammar production nobody
  taught the formatter about can no longer be silently DELETED from the
  author's file, because there is no longer a match on node types for a
  new feature to be missing from.

  The style is written down: [`spec/style.md`](spec/style.md), normative,
  eight sections and eight guarantees. Blank lines are preserved, capped
  at two between top-level declarations and one inside a block, and never
  invented. The author's line break between `=` (or `:`) and its value
  stands, both ways, and so does the author's string delimiter — a `"""`
  value that happens to fit one line is no longer collapsed into an
  escape-heavy `"…"`. Two columns belong to the author and the formatter
  neither invents them nor destroys them: the gap before an arm's `->`
  and the gap before a trailing comment. Mandatory arrow alignment is
  GONE — it coupled lines that have nothing to do with each other, so one
  new arm with a long selector rewrote every line around it; an alignment
  the author chose is now preserved instead, which is the behaviour
  `rustfmt`, `prettier`, `black` and `zig fmt` all approximate by
  refusing alignment outright. What that costs, said in the specification
  and here: the formatter will not REPAIR an alignment either. Add an arm
  wider than the column and the table stays ragged — `nml fmt` leaves it
  and `nml fmt --check` calls it canonical — until somebody widens the
  column by hand. A team that kept its arrow tables aligned by running
  the formatter now keeps them aligned in the same commit that widens
  them. A type expression is no longer
  restructured: `set<(a | b)>` keeps its parentheses, because removing
  redundant parentheses is a simplification, not a formatting.

  Every `.nml` file the repository tracks is now held to `nml fmt
  --check` on every documentation run: 199 in canonical style, 13 refused
  because they are invalid by design (a formatter does not write a guess
  back over a document that does not parse, which is how `gofmt` and
  `rustfmt` behave). Seven files were brought to canonical style to get
  there: two specification examples opened a block with a blank line,
  three generated perf fixtures ended with one, one wrote a digit
  separator the specification already says `nml fmt` canonicalizes away,
  and one wrote 35 significant digits where the value has 34.

  REMOVED with the re-emitter it belonged to: `nml_fmt::formatter::format`
  (the AST-only entry point, which had no caller and one guarantee — that
  it dropped every comment) and `format_with_comments`, and in nml-core
  the comment side channel `cst::parse_with_comments` and its `Comment`
  type. `cst::parse_checked` replaces them: the checked lossless tree,
  refusing exactly what `parse_to_ast` refuses.

- **`nml_validate::workspace` has ONE surface.** Its seven submodules
  were public beside sixty flat re-exports, so the same item had two
  paths (`workspace::discover::MAX_ENTRIES` and, for most, `workspace::…`)
  and a caller could reach any helper a submodule happened to mark `pub`.
  Every submodule is private now and the facade re-exports exactly what
  a front end names — the walk's bounds (`MAX_ENTRIES`, `MAX_COMPONENTS`,
  …), the gate's row makers (`skipped`, `skipped_under`,
  `audit_incomplete`, `path_finding_typed`, `ambiguous_claim_summary`),
  the oracle's `listing`/`DirEntryLike`, `ReadText`, `Built`, and the
  mock oracle under `test-support`. `nml limits` keys a bound by its FILE
  (`declaredIn`), so the wire is untouched. The file-NAME vocabulary
  (`is_manifest_name`, `PROJECT_CONFIG_NAME`, `SCHEMA_SOURCE_SUFFIXES`,
  `schema_source_stem`, `is_schema_source_name`, and the new
  `is_nml_name`) lives in a crate leaf the loader and the kernel both
  read DOWNWARD — the loader had reached up into `workspace::paths` for
  it, the one upward arrow in the crate — and the editor's last two
  copies of a suffix (`.package.nml`, `nml-project.nml`) read the leaf
  too. A module-arrow ratchet (`crates/nml-validate/tests/module_arrows.rs`)
  pins every dependency arrow between the crate's modules, so a new
  cycle fails the build.

- **A load error says how many findings it has before it says the first
  one.** `manifest failed validation (finding 1 of 2): unknown property
  'versio' …`, where it used to read `… unknown property 'versio' (and 1
  more)`. A did-you-mean is appended by the renderer, after the message,
  so a count sitting at the end read as the thing the hint repaired; ahead
  of the colon it qualifies the failure and the hint closes the finding it
  repairs. NML2088 and NML2091 and the library's own two sentences now
  share ONE builder for it, which two of them used to keep a copy of.

- **The lowering from CST to AST is the kernel's own.**
  `nml_core::cst::lower` (`to_ast`, `to_ast_with_errors`) is crate-private:
  an AST is produced only through the module's parse funnel
  (`cst::parse_to_ast`, `cst::parse_to_ast_all`, `cst::extract_schema` and
  their siblings), so no caller can build one that skips the rules emitted
  beside it — the repeated-name rule (NML1000, NML2093) and the span
  invariant are judged in the one text→tree pipeline, and now that is true
  by visibility and not by convention. An embedder that called
  `cst::lower::to_ast` calls `cst::parse_to_ast` instead and gets the same
  AST with its findings.
- **One finding, one squiggle on a manifest the editor cannot load.** The
  manifest document used to carry the finding twice at the same place —
  its own row, and the universe's NML2088 wrapper restating it. The
  document's own row is now the only one there — the row a user acts on,
  with its quick fix and its notes — and it gains the wrapper's context as
  a related location, "the manifest fails to load here (NML2088)". A file
  the manifest would GOVERN is unchanged: NML2088 at its top, the
  manifest's finding a jump away. A loader rule the document's own pass
  does not report (a template string in a list, NML2104) has no twin to
  fold into and stands as itself, at the element.
- **A quick fix that inserts says so**: an action whose edit adds text
  where there was none is titled ``Insert `?` `` rather than
  ``Apply fix: `?` ``; a replacement keeps its title, and an edit in
  another file still names that file.
- **Every span an AST node carries ends at its last content byte.** A
  block's, a list's and a definition's `span.end` used to run past the
  last line (the zero-width layout marker after a block counted as
  content), and a directive's or a facet's `span.start` sat at the
  indentation before it. An embedder that computed an insertion point
  from a declaration's `span.end` expecting the trailing newline now
  inserts one line earlier; one that sliced a definition's text by its
  span gets the definition alone. The invariant is stated and held —
  `Span::is_content_in`, its exact token-aligned form
  `Parse::token_boundaries`, and the whole-tree walks
  `nml_core::ast::for_each_span` and `nml_core::schema::for_each_span`;
  a multiline `"""` template expression's span stays approximate until
  the offset map. The diagnostics this moved are listed under Fixed.
- **Edits the editor hands out take the shape the client declared.** A
  client that declared `workspace.workspaceEdit.documentChanges` gets
  every edit as a versioned `documentChanges` entry; one that declared
  no `create` resource operation is offered no action that creates a
  file (the pin and opt-out on a root with no live config). The pin and
  opt-out edits are the one inserted hunk, not a whole-file rewrite.
- **A structural insertion adopts the file's own indentation unit and
  line terminator**: a `- name` pinned into a two-space `nml-project.nml`
  nests by two, and an entry added to a CRLF file ends with `\r\n`.
- **Every usage error exits 2, in every verb** — an unknown command or flag
  (`-x` included), a missing target, an empty `--root=`, a `--schema`
  directory the run cannot read in full (the directory, an entry, a schema
  source), `--strict` with nothing to enforce, a target outside `--root`, a
  root the tool refuses to derive — so a script can tell "the file is wrong"
  (1) from "the command is wrong" (2). A `--schema` failure is said once,
  before any target runs; it used to fail per target, or shrink the schema
  universe silently.
- **`-q`/`--quiet` is "no output on success"**: the per-file `ok` lines, the
  closing tally, `fix`'s per-file and closing lines, warnings, infos and the
  explain hint are silent; findings, a verb's answer (a `binding` block, a
  diff, an `explain` entry, the `--json` rows), exit codes and the closing
  row's counts are untouched. (It used to keep the `ok` lines.)
- **`nml binding` states the universe once.** Under `--json` the universe's
  rows come as `diagnostic` rows before the first `binding` row, and a
  `binding` row's `notes[]` carries what bears on that file alone; they used
  to be repeated inside every block and counted per block. The run's
  explain hint (`for more information, run: nml explain …`) names the
  first coded row, as every verb's does.
- **`--strict` no longer applies to a manifest-governed file**: the binding's
  own `strict` is the file's strictness everywhere; the run says so once and
  names the binding to set it on.
- **Findings are streamed and bounded**, in the CLI and the editor: a flood
  costs the tranche, never memory, and what was withheld is said per code on
  one `note:` line.
- **The root is named once per run** (the closing row, `nml binding`'s `root`
  line, a `note:` when it was derived in a way you could not see — a `.git`
  file fence, a shadow above the fence, or no fence at all), never inside a
  per-file sentence; the CLI's own lines spell paths from where you stand,
  and a refused derivation names the `--root` that checks under the manifest.
- **The two input-cap rows of `nml limits`** are now census rows named
  `nml-validate::workspace::discover::MAX_MANIFEST_BYTES` and
  `…::MAX_SOURCE_BYTES` (their `--json` `name` values change; the values do
  not), and the CLI's own target cap is `nml-cli::workspace::MAX_TARGET_BYTES`
  (the module that resolves the workspace is `workspace.rs`; its `name` and
  `declaredIn` values change, the value does not).
- **Human output under a non-UTF-8 locale is ASCII** (`—` folds to `--`, `…`
  to `...`, the error index's own glyphs with them; other characters are
  `\u{XXXX}`-escaped); `NML_UNICODE=0|1` overrides.
- **`nml help help` no longer crashes**, `nml version` is a verb like every
  other, every help page — the top-level one included — wraps its option
  and exit-code rows at 80 columns (a usage line or an example runs its
  own length) and lists its exit codes.
- **Nested budget units.** A declared `budgetUnits` pattern nesting inside
  another without pinning one of the outer pattern's wildcards to a literal
  (`tenants/*` beside `tenants/*/*`) is refused at load, both units named —
  every directory the outer unit delegates would mint units of its own
  beneath it, multiplying its share of the walk's budget up to the
  universe-wide backstop; a pinned nesting (`*` beside `tenants/*`) stands.
  Under inference the same layout is NML2092 in a nested form naming the one
  declaration the loader accepts.
- **The editor refuses an ambiguously claimed file as the CLI does.** A file
  two live manifests claim gets one row in the editor — NML2087, the CLI's
  sentence — and is neither validated nor composed until a claim is narrowed;
  a document whose path no key can carry is refused with the kernel's
  sentence as one error row. Navigation still answers: what is withheld is
  the verdict.
- **Pin and opt-out actions write into the nearest LIVE project config** —
  the `nml-project.nml` the kernel resolves pins from, an unsaved buffer at
  that path included — and create one at the binding's anchor otherwise; a
  disk walk used to pick an inert tenant-committed config and could hide the
  opt-out action.
- **The editor states the universe by the kernel's one rule.** A manifest
  document carries the unit-layout lint (NML2092) only while the universe
  stands: under a manifest that failed to load it carries the universe's
  error and no lint, as `nml check` prints none; a universe row located in
  the document it sits on (NML2081 at its item) sits at that item.

- **public API apiVersion 5, revision 1** — BREAKING: the record moved
  with the pinned nightly rustdoc (1.100.0-nightly, 2026-09-22). Auto-trait
  impl lines now carry `&'a mut S` / `&'a F` where-clauses
  (`nml_core::diagnostic::Filtered`, `nml_validate::workspace::OverlayFs`,
  and the same family on `OverlayFs`'s `Send`/`Sync`/`Unpin` lines); many
  inherent methods now spell their return type as `Self` instead of the
  crate path. No intentional API removal — the gate treats any changed line
  as a break.

- **public API apiVersion 4, revision 1** — BREAKING: two signatures
  changed, four items added. `nml_lsp::serve` and `nml_lsp::serve_stdio`
  return `nml_lsp::SessionEnd` — how the session ended, for the embedder to
  map onto its own exit codes (they returned `()`). Added:
  `nml_lsp::SessionEnd` (with `exit_code()`, the LSP 3.17 §exit mapping,
  and `From<SessionEnd> for ExitCode`), `nml_lsp::ExitSignal` (the ending,
  decided by the service, with `ending()` and `ended()`),
  `nml_lsp::NmlService::exit_signal` and `nml_lsp::NmlService::ending`.

- **public API apiVersion 3, revision 1** — BREAKING, three removals and
  one addition, each named by the record's gate when the changes met.
  The formatter prints from the lossless tree, so its AST entry points
  and the comment side channel they needed are gone:
  `nml_fmt::formatter::format` and `format_with_comments`, and
  `nml_core::cst::parse_with_comments` with its `Comment` struct —
  `nml_fmt::formatter::format_source` is the one entry point, and
  `nml_core::cst::parse_checked` yields the checked lossless tree.
  `nml_validate::workspace::open_regular` is gone: the by-path open it
  served was the reader's `wasm32-wasip1` arm, and every front end now
  reads through the one chain (`read_beneath`, `read_leaf`). It was added
  and removed inside this same unreleased cycle, so no consumer that
  pinned a released revision ever saw it — it is named because the stamp
  it moved is a stamp a path-pinned consumer may have built against, not
  because anybody must migrate off it. Added:
  `nml_lsp::server::SERVER_NAME`, the `serverInfo.name` the language
  server answers `initialize` with.

- **public API apiVersion 2, revision 1** — BREAKING, and named here
  because the record's own gate refused it silently: `nml_lsp::packages::
  Resolved` gains a public `universe: Option<UniverseState>` field, which
  stops every exhaustive struct literal and pattern over it downstream.
  Added with it: `nml_validate::workspace::UniverseState`, the ONE owner
  of the two words `open` and `closed` that the `--json` `binding` row,
  `nml binding`'s line and `nml/schemaInfo` all print. They were two
  string literals in `nml-cli` and a bare `bool` in the editor, which is
  how the status bar came to offer the OPEN remedy — *commit a
  `<name>.package.nml`* — over a CLOSED universe, where a manifest
  already exists and the fix is a `files` glob. `nml/schemaInfo` gains
  `universe` (additive, per that payload's grow-by-adding rule).

- **public API apiVersion 1, revision 1** — the library crates' public
  surface is recorded (`docs/api/<crate>.api.txt`, generated by `cargo
  public-api --simplified`) and gated: an ADDITION needs `revision` + 1, a
  REMOVAL or a reshaped item needs `apiVersion` + 1, and either needs an
  entry here. `nml-core`, `nml-validate`, `nml-fmt` and `nml-lsp` are
  consumed by path and by pinned rev, so cargo's own version resolution
  guards nothing; the record is what makes a change to them visible in the
  review that makes it, as the `--json` shape record does for the wire.

### Fixed

- **The language server honours the protocol's `exit` notification.**
  LSP 3.17: "The server should exit with success code 0 if the shutdown
  request has been received before; otherwise with error code 1."
  Measured on the native binary before this change: `exit` did not end
  the process while stdin stayed open — tower-lsp's transport reads on
  until EOF, so a client that kept the pipe open kept the server — and
  the process ended with 0 whether or not `shutdown` had come. The
  service now records `shutdown` and decides the ending at `exit`
  (`nml_lsp::ExitSignal`, `nml_lsp::SessionEnd`), both transports stop
  reading there and return it, and `serve` / `serve_stdio` hand it to
  `main`, whose exit code is `SessionEnd::exit_code()`. A client that
  simply closes stdin without `exit` still ends the server, as
  `Disconnected` (code 0): that is not a protocol ending, and an editor
  that ends a server it could never talk to must not read the code as the
  server's fault. Pinned on the binary with stdin held open (both codes)
  and on the wasm pump.

- **`exit` decides the exit code even when the client closes stdin in the
  same breath.** A client that sends `exit` and then closes the pipe
  without waiting leaves two endings ready at once — the protocol's, and
  the transport's EOF — and the `select!` between them chose at random:
  measured on the binary, 15 of 60 such runs exited 0 where LSP 3.17 §exit
  fixes the code at 1. The EOF arm now answers with the ending the service
  already recorded, if there is one, so the code is the protocol's whatever
  order the two arrive in. Pinned on the binary over repeated runs (the
  two pins beside it hold stdin open and never saw this).

- **A failed provider no longer takes the working server down with it.**
  When a program a repository asked for never answered `initialize`, the
  editor fell back to the bundled server — and then the abandoned
  program's connection closed, seconds later, and the language client
  called back on the ABANDONED client. The extension's handlers acted on
  "the current client", which by then was the healthy fallback: it was
  stopped, its process ended, and a second, false "failed to start"
  message was shown, leaving the editor with no language server at all
  (MEASURED, on both `errorHandler.closed` and
  `initializationFailedHandler`). Each launch attempt is now one object
  that owns its own client, process and callbacks, and every callback it
  hands out is inert once the attempt is retired — by construction, not by
  a check per handler.

- **Stop, restart and closing the window no longer wait for a server that
  is not answering.** All three queued behind whatever was in flight,
  including the 15-second wait for a provider's `initialize`. VS Code
  gives `deactivate()` 5000 ms in total, so a window closed during that
  wait simply left the server running. An intent now CANCELS the attempt
  in flight instead of queueing behind it, and teardown at deactivation
  runs on a profile whose worst case is asserted, from the constants, to
  fit inside the 5000 ms.

- **A burst of restarts is one restart.** Five call sites asked for a
  restart through an operation queue — the command, a changed
  `nml.server.path`, a changed trace setting, workspace trust being
  granted, approvals being forgotten — and a settings edit that fires
  several configuration events queued several restarts, each able to wait
  out a handshake budget. The manager holds the state it WANTS instead,
  and one reconciler drives the current server toward it: superseded
  intents never run, and two servers are still never starting at once.

- **The server's exit code reaches the log.** Handing the process to the
  language client meant the library owned it and logged nothing about how
  it ended; the extension now records the exit code or the signal for
  every server it starts, and says so in its own words when one goes away
  unexpectedly (the client's generic "connection got closed" notice is
  marked handled, so one event no longer produces two notifications).

- **A one-line change no longer prints as a whole-file diff.** The
  `--dry-run` diff ran a quadratic LCS with a cell budget
  ([`MAX_CELLS`](nml-cli/src/fix.rs)), and any pair past it fell back to
  one whole-file replacement — so a single trailing blank line in a
  3,296-line fixture printed 6,591 lines of "diff", and the change itself
  was invisible in it. The common prefix and suffix are matched off
  before the LCS runs, as every real diff does, and only what lies
  between them can reach the budget. The same fixture now prints eight
  lines. This is the diff `nml fmt --dry-run` and `nml fix --dry-run`
  both show.

- **A fallback chain ends at its line.** A `|` that ended a line used to
  take the NEXT line's entry as its arm: `host = $ENV.HOST |` over
  `port = 3000` swallowed `port` — the entry was gone from the tree,
  `check` passed, and `fmt` rewrote the file without it. A chain is one
  line now: the dangling pipe is reported once, AT the pipe
  (``expected a value after `|`, found a line break``, NML0002; ``found
  end of file`` at the end of the file), and the next line is parsed as
  its own entry. In list position the chain is still one NML0021 and
  the next item stands. A `|` at the START of a line was already a modifier,
  never a continuation — that is unchanged, and it is why the two rules
  are one rule: a chain never crosses a line break in either direction.
- **Every span a node carries is a content span.** A diagnostic anchored
  on a definition, an enum, a `oneof`, a directive or a facet used to
  start at the indentation before it or run past its last line (the
  zero-width layout marker after a block counted as content), so rows
  rendered at column 1 and block spans covered the blank lines after the
  block; a template expression's span was one byte early and drifted by
  every escape before it, so NML5004's did-you-mean would have replaced
  the wrong bytes — and the whole `{{…}}` rather than the namespace. One
  rule in the kernel now computes every node span (first through last
  significant token), template expressions are segmented on the raw
  string so their spans are exact and an escaped `\u{7B}\u{7B}` beside a
  real expression stays literal, and the invariant is pinned over the
  whole corpus and held by the `document` fuzz target for every input
  (`nml_core::ast::for_each_span`, `nml_core::schema::for_each_span`,
  `Span::is_content_in`, and the exact token-aligned form
  `Parse::token_boundaries`).
- **A `key:` block dedented to a list body's item column is NML0002,
  never dropped.** The lowered tree had no place for a nested block, a
  field definition or a routing arm in an array body and left it out
  silently: `parse` showed a tree without it, `check` passed, and `fmt`
  rewrote the file WITHOUT the lines — the shape a remedy's block takes
  when pasted at the indentation it was printed with. Every verb — and
  the editor's parse band — now reports `expected a list item, a
  property, a modifier or a shared property in an array body, found a
  nested block` at the line, and `fmt` leaves the file untouched. Two fuzz invariants guard the class:
  lowering is total on a clean document (every CST entry lowers), and
  formatting preserves the lowered tree (spans aside).
- **A `|modifier` block after an item's body parses as a modifier.** A
  zero-width layout marker consumed the line break before it, so a `|`
  (or a `#directive`) after a `Dedent` read as same-line: `|deny:` after
  an item was NML0021 (a fallback chain) with the block eaten as legs.
- **Inline arrays in a manifest are read.** `files = ["…"]`,
  `schemas = […]`, `budgetUnits = ["tenants/*"]`, `allowRefs = […]` —
  every list-valued field, in the inline spelling the meta-schema
  already accepted — loaded and was ignored (`files` reported EMPTY).
  The loader reads every list through nml-core's one accessor
  (`BlockQuery::string_list`, both spellings, per-element spans), and
  NML2092's remedy prints the one-line spelling (`declare budgetUnits =
  ["tenants/*"]`), which pastes.
- A directory the universe walk did not enter — reached through a link an
  open universe follows (`link/subdir`), or the link itself typed as the
  target (`link`) — is refused by every verb with one sentence (`is a
  directory the universe walk did not enter …`, exit 1); `fix` read it as a
  file and printed the OS's `Is a directory`, and a typed link did so in
  every verb. `nml binding` refuses both as it refuses any directory
  (`is a directory — nml binding takes files`, exit 2); the typed link used
  to get a `binding none` block.
- A `--schema` directory that cannot be read says why in the tool's words
  (`no such directory`, `permission denied`), never `(os error N)`; a FIFO,
  socket or device named as a target says what it is and the remedy; `nml
  binding` on an ambiguously claimed file cites the code that denies it
  (NML2087), not NML2064.
- Tutorial 09's claimed output hash was wrong and is now checked by the docs
  test.
- **The VS Code status bar hid every refusal.** An `error` note dropped the
  whole notes list and the bar said "No schema package governs this file";
  a refused document now shows `nml: not validated` on the error background
  with the reason and remedy, and a manifest above a derived root being
  ignored (`rootShadowed`) colours the item as a warning.

- **Three things the extension told the operator to do, that would not have
  worked.** A provider whose handshake expired, or that answered without
  naming itself, is stood down for the SESSION — so *NML: Restart Language
  Server*, which the expiry message named, re-resolved straight past it and
  came back with the built-in server, silently; both messages now name the
  window reload, which is what clears a stand-down, and the "rebuild it"
  remedy says when the rebuilt tool is picked up. A server that could not be
  ended names the process the operator must end **in a command their shell
  has**: `taskkill /F /PID N` on Windows, where `kill -9` is not a program
  (the forced stage there is already a tree kill, so the sentence was the
  last POSIX-only thing left in that path). And `nml: no server` — the one
  status-bar state with no next step, and the one the bar holds while
  nothing is running — now names *NML: Restart Language Server* and
  *NML: Show Language Server Log*, as every other state does.

- **A project whose declared language server is not used now says so.** Four
  rungs of the discovery ladder returned the built-in server in complete
  silence: two workspace folders naming two different tools, an untrusted
  workspace, a tool name that is on no `PATH` entry, and a name that resolves
  to a program inside the workspace (which a repository does not get to
  supply). None of them prompts — not reaching the prompt is what happened —
  and the status bar correctly names the server that IS running, so the
  operator could learn nowhere that the `provider:` block they committed was
  being passed over. Each rung now writes one line to the NML Language Server
  log naming what is running, what happened and the one thing that would
  change it, as the remembered decline and the refused directory already did.

### Security

- **A manifest's shadow analysis is bounded across the pairs it compares,
  not only within one.** Every binding's globs are checked against every
  EARLIER binding's, so the comparisons are QUADRATIC in a count the
  manifest itself declares — and the 256 KiB a manifest is read under
  admits thousands of declarations. Only one comparison was bounded
  (`MAX_SUBSUMES_STATES`), so a `*.package.nml` a repository wrote, inside
  every published bound, cost 108 s for 447 bindings in a 64 KiB manifest
  and 127 s for ONE `textDocument/diagnostic` on it — in the language
  server, which runs the analysis per pull, with no cancellation. The
  analysis now draws on a single budget (`MAX_SHADOW_WORK`, a published
  bound) whose currency is state pairs TIMES automaton size, so a maximal
  pattern cannot buy itself a full per-comparison budget at a thousand
  times the price per state; the same manifests now answer in about a
  second. Past the budget a pair reads as incomparable, which withholds an
  authoring advisory and decides nothing the walk, the binding or the
  validation turn on.

- **The extension reads a repository's `nml-project.nml` under the
  kernel's own bound, and only when it is a regular file.** The extension
  reads that file itself, in the extension host, before the language
  server exists and before the Workspace Trust gate — and it read it
  whole, with no bound and no kind check, while the kernel refused the
  same file past 256 KiB and refused a link, a FIFO or a device at the
  open. A `nml-project.nml` that is a symlink to `/dev/zero` was 7.5 GB
  of the extension host's heap in 8 seconds and never returned (MEASURED
  on this platform's Node); a FIFO blocked forever. Both checks are now
  the read's, and a file that is refused is SAID in the log, with what
  stops being used because of it.

- **The provider environment scrub covers a config file that names a
  program.** Three families reached the spawn: OpenSSL's `OPENSSL_CONF`,
  `OPENSSL_ENGINES` and `OPENSSL_MODULES`, whose configuration DECLARES
  native modules to load (`[engine] dynamic_path`, `[provider_sect]
  module`) — `LD_PRELOAD` spelled as a config; `GIT_DIR` and
  `GIT_COMMON_DIR`, whose `config` names `core.fsmonitor`,
  `core.sshCommand`, `core.pager`, `diff.external` and `filter.*.clean`
  (the scrub already removed every `GIT_CONFIG*` for exactly that reason);
  and `GIT_TEMPLATE_DIR`, which seeds a new repository's HOOKS. Lua's
  hooks are now a PREFIX (`LUA_`) rather than three exact names, because
  `LUA_INIT_5_4` is checked before `LUA_INIT` and the versioned spellings
  were all reachable.

- **A language server the editor started stops existing when the editor
  does.** A stdio language server is ended by closing its stdin — but only
  if something is alive to close it. The extension handed its servers to
  `vscode-languageclient`, whose own teardown is a delayed `pgrep -P` tree
  walk with `kill -9`: it MISSES a double-forked worker (re-parented to
  pid 1, but still in the server's process group), it never runs at all
  when the extension host is SIGKILLed, and it is the wrong party to run
  it — a program that ignores stdin EOF, SIGTERM, SIGHUP and SIGINT
  survived the editor and kept the operator's workspace open indefinitely
  (MEASURED; so did the advice "reload the window to end it"). The
  extension now owns every server process it starts. On POSIX each one is
  launched through a small supervisor that holds a control pipe to the
  editor, runs the server in its OWN process group on inherited stdio (no
  LSP byte passes through it) and SIGKILLs that group the moment the pipe
  reaches EOF — which the kernel delivers however the editor died. On
  Windows the server is spawned non-detached, which puts it in the job
  object libuv creates with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and the
  forced stage is `taskkill /T /F` so its own subprocesses go too.
  Teardown is one staged ladder — close stdin, SIGTERM the group, SIGKILL
  the group — every stage bounded, and the OPERATOR'S MESSAGE IS WRITTEN
  FROM WHAT WAS OBSERVED: a program that could not be ended is now named
  with its pid instead of being reported as stopped.

- **The Windows teardown no longer runs a program out of the editor's
  working directory.** The forced stage ran `taskkill` as a bare name, and
  libuv resolves a bare name by looking in the process's CURRENT DIRECTORY
  before it scans `PATH` (`src/win/process.c`, `search_path`, which also
  tries `.com` before `.exe`). The extension host's working directory is
  inherited from whatever launched the editor — `code .` in a repository
  makes that repository the first place looked — so a file a repository
  ships could be run in place of the system tool, with the extension's
  privileges, whenever a native server was stopped. It is now
  `%SystemRoot%\System32\taskkill.exe`, absolute, with a `%SystemRoot%`
  that is not an absolute path falling back to the documented default, and
  `windowsHide` set. The same hazard through the other door — a relative
  `PATH` entry — was already refused when a provider is resolved.

- **The launch's control channel is parsed as untrusted data.** fd 3
  carries the supervisor's reports to the editor, and a `pid` read from it
  becomes `kill(-pid)`, where `1` is every process the account may signal
  and `0` is the editor's own process group. A provider does not reach that
  descriptor — it inherits the three stdio pipes and nothing else — but
  that is a property of the Node runtime rather than of this code, so it is
  now PINNED by a real process that tries the write and must fail, and the
  reader no longer trusts what arrives: a pid must be a whole number naming
  a process, and a line that never ends is dropped instead of growing the
  extension host's heap without bound.

- **The environment scrub is case-insensitive on Windows.** Windows looks
  environment variables up without regard to case, so a runtime reading
  `NODE_OPTIONS` reads it out of a block that spells it `Node_Options` —
  and an exact-bytes denylist removed one spelling while leaving the other
  in place for the same reader. The comparison now matches the platform's
  own; POSIX stays exact, where the two spellings really are two variables.

- **A directory on `PATH` named like a provider tool no longer ends the
  search.** `access(X_OK)` succeeds on a directory — there the execute bit
  is the search bit — so a directory could be shown in the consent prompt,
  approved, and then fail to spawn, while the real program further along
  `PATH` was never reached. The hit has to be a file.

- **A long fallback chain can no longer crash the parser's callers.**
  `a | b | c` is flat in the source but lowers to one nested value per arm,
  and everything that walks a value recurses through it. Nothing bounded
  the arms, so a single 400 KB line of 60 000 of them — a tenth of the
  source cap — ended `nml check` in a stack overflow, 20 000 ended
  `nml fmt`, and the same file ended the language server on open: an
  abort, which no caller can catch, from content alone. A chain is now
  held to the nesting bound it always was in effect (64 arms); one past it
  is `NML0007`, reported once, the rest of the line is kept in the tree as
  an error and never lowered, and the next entry parses as its own. The
  pin runs a 100 000-arm chain on a 1 MiB stack — the wasm guest's.
  `nml limits` and `nml explain NML0007` say so.

- **A repository's language server is approved against what the repository
  ASKED FOR, runs in an empty private directory with the loader-injection
  variables removed, and has to identify itself once it is running.** An
  approval used to be remembered as `(tool name → resolved path)`, granted
  against a prompt that named neither the file that asked nor the command
  line that would run, and honoured forever — so a later `git pull` that
  rewrote the `provider:` block ran the new declaration with no prompt at
  all. Consent is now pinned to a digest of the declaring `provider:` blocks
  themselves plus the path the tool name resolved to, so editing the
  declaration, or the same name resolving somewhere else, asks again (the
  rule `direnv allow`, `mise trust` and VS Code's Workspace Trust all
  settle on). A decline is remembered the same way, and
  **NML: Forget Language Server Approvals** takes any of it back.
  The prompt is modal and says the declaring file, the exact command line,
  that the program runs with the operator's permissions, that the answer is
  remembered and how to revoke it, and that declining keeps NML editing
  working. Before the spawn, the directory the tool resolved in is judged:
  world-writable, or owned by another account, is refused outright with the
  reason in the log (a group-writable prefix — Homebrew's is `drwxrwxr-x` —
  is reported in the prompt, not refused). The spawn itself gets an empty
  private working directory under the extension's storage, remade each
  activation, and an environment with `LD_*`, `DYLD_*`, `NODE_OPTIONS`,
  `BASH_ENV`, `PERL5OPT`, `PYTHONSTARTUP`, `RUBYOPT` and the `RUSTC_*`
  wrappers REMOVED rather than blanked. After the spawn, `initialize` must
  answer `serverInfo.name = "nml-lsp"` within 15 seconds. Three outcomes,
  and only one touches the approval: a DIFFERENT name withdraws it, stops
  the process and says so; NO name stops the process for the session and
  leaves the approval alone; an expired budget does the same, because a
  loaded machine looks exactly like a program that will not answer.
  **Every provider tool built before this release answers with no
  `serverInfo` and is therefore stopped under the updated extension until
  it is rebuilt** — it is an honest NML server the client cannot yet
  recognise, not an impostor, so it is told apart from one: its message
  says the likely cause (a build older than the handshake) and the action
  (rebuild against a current `nml-lsp`), its approval stands so the rebuilt
  tool starts in the next window with no second prompt, and the message for
  a program that named something else offers neither. The check runs
  after the spawn and a hostile program can answer any name, so treating
  silence as hostility would have bought nothing. The Upgrading note at the head of this release states it for
  operators and tool authors both. Deliberately NOT part of the identity: a hash of the
  resolved binary. `rustup`'s proxies give `cargo`, `rustc` and
  `rust-analyzer` one identical hash that never moves when the toolchain
  behind them does, `pyenv`/`asdf`/`mise` shims are stable wrappers in front
  of a moving target, and Homebrew/npm hits are symlinks whose hash moves on
  every routine upgrade — the same mechanism is at once vacuous and noisy,
  and against an attacker who can rewrite a binary on `PATH` it buys nothing.

- **The language server identifies itself in the `initialize` handshake.**
  LSP 3.17 `serverInfo` was not sent at all, so a client had no
  protocol-level way to tell an NML language server from whatever else it
  had just started. The neutral server and every schema-provider tool that
  embeds `nml_lsp::serve` now answer `nml-lsp` with the crate version.

- **The VS Code extension spawns every process-backed language server in
  the operator's home directory, never the workspace.** `vscode-languageclient`
  defaults an executable's working directory to the first workspace folder,
  and a project's declared tool name (`provider: tool = "…"`) may resolve
  to an interpreter — `sh`, `node`, `python3` all satisfy the name rule — for
  which the fixed `lsp` argument is a script path resolved against that
  directory: a repository shipping a file named `lsp` beside its
  `nml-project.nml` ran it the moment the operator accepted the prompt. One
  constructor (`processServer`) now stamps `cwd` on every process resolution
  (a provider tool, an `nml.server.path` override, the native default) and
  the client passes it; no server reads its working directory.
- **The editor's request ratchet now scans its own door.** The source
  ratchet that keeps every server→client request inside `ClientDoor::ask`
  excepted `ask.rs` itself, so a request the door sent outside `ask_over`
  compiled clean and passed every test; the method ratchet now reads the
  door too (its one legal spelling there names no method), while the
  naming ratchet still skips it, where the raw client lives by design.
- **A schema package whose `version` spells a path can no longer be
  published outside the store.** `Store::publish` named its slot
  directory `<version>+<hash8>` and joined it under the package's store
  directory unchecked, while nothing at parse constrains a version: a
  manifest with `version = "../../../x"` staged its files — named and
  bodied by the manifest — and `rename`d the staging tree to wherever
  the version pointed, then wrote a pointer the read side refuses as
  corrupt. The write side now applies the read side's rule
  (`plain_slot`: a plain entry name that does not begin with `.`, the
  shape `gc` deletes as a stale temp artifact, and holds no control
  character — a `version` with a newline in it published a slot whose
  pointer spans three lines, and every read of that package was
  `Corrupt` from then on; an empty `version`, whose `+<hash8>` slot
  loads but lists as `?`, is refused with it) before any path is
  touched, and refuses as a `Write` error naming the version. The only
  publisher today is a tool's own compiled-in package, so nothing
  shipped could reach this; the guard exists so that nothing ever can.

- **The editor and the CLI read every file through ONE reader; the
  editor never blocks on opening an input, never reads a non-regular
  one, and never reads through a directory swapped for a link.** The
  editor's reads (discovery's, the index's, a quick fix's target, a
  related note's file) opened with a plain `File::open`: a FIFO named
  like an input — planted, or swapped in for a file between the walk's
  `lstat` and the read — parked the server thread inside `open(2)` until
  a writer appeared (never), and every request after it timed out; and a
  directory the walk had classified, swapped for a symlink before the
  read, was followed — the editor minted claims from outside content
  where `nml check` refused the same file typed, through its
  `openat(O_NOFOLLOW)` chain. Both front ends now read through the
  kernel's one reader (`workspace::read_beneath` under a root,
  `read_leaf` at a file's own parent, `read_input` for a discovery
  input): the race-free chain, `O_NONBLOCK`, `fstat` must say regular
  file, one byte cap, one sentence — a FIFO, device, socket, directory
  or swapped link is refused in the kernel's words within the moment,
  identically in the editor and the CLI; a source ratchet keeps every
  editor disk read there. (On wasm the editor's reads stay by path: its
  host is unprobed for the chain — the arm's doc has the record.) A
  non-UTF-8 `check`/`fix`/`parse`/`fmt` target is refused as `not
  UTF-8`, the word the discovery reader and the editor already used for
  the same file, where it was `invalid utf-8 sequence of N bytes from
  index M`.

- **A directory that cannot be LISTED is no longer a directory with no
  workspace manifest in it.** Deriving the workspace root asked each
  directory between the target and its `.git` fence — and each one above
  the fence, for the shadow check — whether it held a `*.package.nml` or
  an `nml-project.nml`, and read a REFUSED listing as "no marker". A
  directory that is searchable but not listable (mode `0111`, a hardening
  an operator may choose, or any EACCES on a shared runner) therefore
  hid the operator's manifest: the universe shrank to the target's own
  directory — an open context that governs nothing — and a `nml check`
  that had been exit 1 under the binding's `strict` became `ok`, exit 0.
  The same on an ancestor ABOVE a `.git` entry that is no directory
  turned the `Shadowed` refusal (exit 2, "a `.git` file below a workspace
  manifest cannot shrink its universe") into a green run. The listing's
  failure is now the derivation's on both: it refuses, in the oracle's
  own words, and names `--root`. This is the rule the shadow check
  already applied to its own work bound and the walk applies to a single
  unreadable directory entry.

  Above a `.git` DIRECTORY fence it is NOT: nothing that listing could
  have found there can deny a universe — a marker above a directory
  fence is the disclosed `root.shadowed`, and a `.git` above it is found
  by a LOOKUP, which mode `0111` still answers — so a blinded listing
  costs that disclosure and the derivation stands. Refusing there too
  would have put every ordinary run behind the mode of every directory
  between the fence and `/`: one hardened parent, one privacy-gated
  folder, one other account's home anywhere up the chain and `nml check`
  exits 2 for a sentence it could not have printed.

- **One glob segment's length is bounded, and a refused glob no longer
  rides the row whole.** A manifest glob was capped at 64 SEGMENTS and,
  through the manifest, at 256 KiB in total — but one segment could be
  all of it, and the matcher tries each segment against every path
  component of every file it judges. Measured: 200 two-line files under
  `files = ["**/<250 KB segment>"]` at depth 59 cost 48.9 s wall and
  32.7 s of CPU for `nml check`, against 0.2 s for the same tree under
  `tenants/**` — and the editor runs the same matcher on every keystroke
  over a workspace whose manifests an author may commit. A segment past
  **1 KiB** (`MAX_PATTERN_SEGMENT_BYTES`, published by `nml limits`) is
  now refused at load under NML2081 in every glob vocabulary — `files`,
  `allowRefs`, `denyRefs`, `budgetUnits` — because past it the matcher
  answers "no match" for every path, which is fail-closed for `files` and
  an allow rule and FAIL-OPEN for a `denyRefs` veto; the matcher keeps
  the same fence of its own. The same run's finding used to carry the
  offending glob verbatim onto the terminal and the `--json` wire, twice
  (a `message` and a `cause.message`): a glob past 160 bytes
  (`MAX_GLOB_ECHO_BYTES`) is now elided around its exact byte count, cut
  on character boundaries. And the matcher no longer rebuilds a pattern
  segment's characters once per DP cell — once per match, as the path's
  are.

- **Every shape of the read-through's race harness now proves it saw the
  race.** `tests/beneath_race.rs` flips a parent or a leaf between a
  directory, a file, a link to a file outside the root and a FIFO while
  `open_beneath` / `write_beneath` hammer it. Five of its six shapes
  asserted only that no run READ or WROTE outside — true whatever the
  guard does when the flipper never wins — and printed the refusal count
  for a human to read; the sixth already refused to pass without one. A
  run in which the flip is never observed (a single-core runner, a
  loaded container) was therefore green and silent, and would have
  survived removing `O_NOFOLLOW` from the chain's directory opens. All
  six now retry up to three times the lane's count while the flip has
  not been seen and then FAIL if it never was.

- **A did-you-mean over an unterminated string literal can no longer
  address half a character.** A machine-applicable replacement of a
  string value targets the bytes INSIDE the literal's delimiters, and the
  window was derived from the value's span by stripping one byte at each
  end — right for `"…"`, wrong for a literal whose token ends at the line
  break with no closing quote: when its last character was multi-byte the
  span fell inside it. Both appliers refuse such a span, so the effect was
  a lost fix rather than a damaged file, but an embedder splicing
  `suggestions[]` (or the `--json` `endCol`) by byte offset would have
  corrupted one. The window is now minted where the token is read and
  CARRIED by the AST, so no consumer derives it; the content-span
  invariant is checked over the whole corpus and by the `document` fuzz
  target for arbitrary input, unterminated literals included.

- **Two packages that could cover an undeclared schema source made its
  directives silently unjudged.** With one package in the root a stray
  `*.model.nml` beside it is judged under that package's vocabulary
  (NML5000 for an unknown directive); with two, the kernel answered
  "opaque" and every directive was accepted with no row — adding a
  second package switched every undeclared source from judged to
  accept-anything in silence. The kernel now answers `Ambiguous`
  naming the packages, and `nml check`, `nml validate` and the editor
  say so in one info line at the top of the file (`package coverage
  ambiguous: 2 packages could cover this schema source (demo, other)
  …`); the truncated universe's note is the same kernel sentence in
  both front ends. For embedders: `VocabularyOutcome::Ambiguous` and
  `VocabularyOutcome::note`; `Undetermined` lost its never-filled
  `candidates` field.
- **A template string in a manifest list was dropped silently.** The
  meta-schema admits `"tenants/{{x}}/**"` as a string; the loader's list
  reader set it aside, so `files` claimed less than it said, a
  `budgetUnits` unit vanished and a `denyRefs` veto loaded and never
  fired (fail-open). Every list-valued manifest entry — `files`,
  `schemas`, `allowRefs`, `denyRefs`, `budgetUnits`, `rootMarkers`,
  `modifiers`, `memberKeywords`, `builtinRefs` — now refuses a template
  element at the element (a loader rule, NML2104, the `cause` of the
  universe's NML2088 row). The
  grant remedy spelled a key that contains `{{` raw, so the block `nml
  check` printed for a file named `a{{b}}.flow.nml` loaded and granted
  nothing; keys, the formatter's literals and the editor's quoted labels
  and snippets now spell through the language's one string speller
  (`nml_core::source_policy::string_literal`), and the pasted block
  grants what it names.
- **Two `[]directive` entries with one name loaded silently** (the first
  answered every lookup); refused at the later entry with the first as a
  note (NML2093 at the manifest), as `[]schema` and `[]validator` names
  already were.
- The editor's quick-fix target is read as a workspace key
  (`SourceKey::checked`) before it is joined under the root — the CLI's
  foreign-read rule — so a name that is no key yields no action rather
  than a lexically contained path.
- **A manifest could carry two `files` entries.** The second loaded
  silently and claimed nothing — readers picked the first, or the last
  (`version` took the last, `formatVersion`'s gate the first) — a way to
  make a manifest look like it claims what it does not. Refused now
  (NML2093 where the manifest is parsed, the universe closed-denied under
  NML2088 with the first entry as the row's note), before any glob is read.
- **The gate's completeness promise had two silent holes**, now closed: an
  entry whose name no key can carry (a `\` in a unix name, a non-UTF-8 name)
  and a directory at the 64-component bound were dropped by the walk, and a
  tenant's `.nml` content hidden under either passed `nml check` with exit 0.
  Both are reported skips now (NML2090, error), the closing row names them
  (`unkeyableName`, `componentBound`), and a fuzz target holds the invariant:
  every entry the walk meets is judged or reported, never nothing — over
  names that are not UTF-8 too, and over the hidden audit of every skipped
  dot-directory (each `.nml` entry beneath it counted, or the audit says
  where it stopped short).
- The library's kernel-only helpers are crate-private (`SourceKey::{dir_contains,
  dir_is_strict_ancestor_of, child_dir}`, `Grant::of`, `Discovery::{configs,
  load_errors}`, `Universe::configs`, `WasiFs.list`, `MockFs`'s fields):
  every reader outside the kernel goes through one door.
- **A manifest glob segment no key component can equal is refused at load**
  — empty, `.`, `..` or `\`-bearing, in `files`, `allowRefs`, `denyRefs`
  (NML2081) and `budgetUnits`. Such a segment matches no key: a `files`
  glob or an allow rule that silently claimed nothing, and a `denyRefs`
  veto (`vendor\secret\**`, `vendor/./secret/**`) that loaded and never
  fired.
- **`--schema` sources are opened as targets are** (`O_NOFOLLOW |
  O_NONBLOCK`, then `fstat`): a FIFO, socket or device named `*.model.nml`
  is refused as the invocation's mistake (exit 2, before any target) —
  the by-path read blocked the run on it forever.
- **A separator is refused where the key is minted.** A path component
  bearing a separator (`ev\il` on unix — a legal name git tracks) is refused
  as the key is minted, not one step later at the read, so a key never
  carries a separator by construction and the editor never binds such a
  buffer; the CLI and the editor key a path lexically by one rule
  (`SourceKey::under`).

The engineering detail behind every line — which round, which pin, the
kernel's type names — is RFC 0019's errata and RFC 0026
(`docs/rfcs/0019-instance-layers-and-sealed-fields.md` and
`docs/rfcs/0026-item0-closure-gate-completeness-grant-and-parity.md`),
and the round-by-round log below.

<details>
<summary>Engineering log — the round-by-round record behind every line above (RFC 0019 errata; kernel type names, pins, memos)</summary>

### Added

- **One lexical keying; a separator is refused where the key is minted
  (RFC 0026 B-8, B-10).** A path component bearing a separator (`ev\il`
  on unix — a legal name git tracks, the one the walk reports as
  `unkeyableName`) is refused where the key is MINTED
  (`PathError::NotPlain`, the read's own sentence `` `ev\il` is not a
  plain path component `` one step earlier) — every minted, halted,
  respelled or lexical key is assembled through one component rule
  (`paths::component`: UTF-8 and plain), so a key never carries a
  separator by construction; the editor no longer binds such a buffer.
  `SourceKey::under(root, path, fs)` is the ONE lexical keying (the
  operator-side prefix folded through the fs, `.` dropped, `..` popped as
  `mint` pops it), and the editor's private twin — which refused `..`
  and turned a unix `ev\il` into two components — is gone (library:
  `SourceKey::lexical` renamed and made public). The walk and the
  hidden audit read every listed name through one classifier, and the
  unkeyable entry's name lives inside its reason
  (`Skip::UnkeyableName { kind, name }`; `Skipped::entry()` is the wire's
  `entry`; `Skip` is no longer `Copy`) — library, the wire unchanged.
  `discover` takes the validator table it shares
  (`discover(root, fs, read, extra, validators)`) and a `Discovery` owns
  the root it walked, so `Discovery::universe()` takes no root — a
  discovery read under another root, or a validator table swapped after
  construction, is unrepresentable (library, pre-1.0).
- **Nested budget units (RFC 0026 B-3).** A declared `budgetUnits`
  pattern nesting inside another without pinning one of the outer
  pattern's wildcards to a literal (`tenants/*` beside `tenants/*/*`)
  is refused at load, both units named at the block — every directory
  the outer unit delegates would mint units of its own beneath it,
  multiplying its share of the walk's budget up to the universe-wide
  backstop; a pinned nesting (`*` beside `tenants/*`, the root
  catch-all beside the tenants — E38's designed layout) stands. Under
  inference the same nesting is always a gap on the inner glob (its
  last wildcard run starts past a literal that follows the outer unit's
  wildcard), so NML2092 takes a NESTED form there: both globs and both
  units named, the one declaration the loader accepts offered and the
  inferred boundary withdrawn.
- **The manifest `layers:` grant (RFC 0019 plan item 4, brought forward
  by RFC 0026 B-1).** A validator binding declares its composition
  grant in the manifest — `layers:` with `allowRefs`, `denyRefs?` and
  `maxStackDepth?`, ordinary schema on the builtin meta-package — and
  both front ends judge `uses` under it: a veto is NML2065 by rule
  index, an allow-miss NML2065 naming the binding, an admitted stack
  composes; `nml binding` prints the rules by the same indices and the
  `--json` `layers` object carries them. The grant's own rules are the
  new **NML2081** at manifest load, located at the item (a glob the
  matcher rejects, a stack cap past 16, `denyRefs` beside an empty
  `allowRefs`); the block's shape — `maxStackDepth` whole and at least
  1 included (`number(min = 1, multipleOf = 1)`) — stays the
  meta-schema's NML2088. NML2064's no-grant form carries its remedy as a LOCATED
  `note:` at the binding in the manifest (`related[]` on the wire, a
  related location in the editor) naming the exact key to admit. Every
  loader rule's finding is now located: the NML2088 row for a refused
  `budgetUnits` declaration, a malformed `files` glob, a meta-validation
  finding or a parse error is located AT the finding, as every located
  finding is (`demo.package.nml:5:5: error[NML2088]: …`; `line`/`col` on
  the `--json` wire; the editor squiggles it in the manifest and points
  at it from every file under it) — the sentence names no line, and
  `PackageError::Inconsistent` is folded into the located `Manifest`
  variant (library; `SchemaPackage::from_dir`'s sentence keeps `at
  <file>:<line>:<col>`, the one place with no row). An older reader
  refuses a manifest carrying `layers:` at load (closed vocabulary), as
  for `budgetUnits`.
- **Explicit budget units and the gap lint (RFC 0019 item 4, E38).** A
  package manifest declares its budget units — `budgetUnits = ["tenants/*"]`
  under the package block: anchor-relative directory patterns, `*` within
  a segment, no `**` — and the declaration REPLACES the unit inference
  from its binding globs, so the three loud layouts (`tenants/*/flows/**`,
  `tenants/**/flows/**`, `**/tenants/*/**`) isolate each tenant by one
  line. The loader refuses a declaration narrower than the inference
  (content a glob reaches would be the root unit's) at the `budgetUnits`
  block, naming the glob and the unit to declare. Under inference, a glob
  whose first wildcard directory run is not its last is the new
  **NML2092** (warning): once per run on the CLI, located in the manifest
  at the glob (`demo.package.nml:12:15:`), on the manifest document in the
  editor, for the operator-level manifest only (a tenant's manifest inside
  claimed content mints no unit); silenced by the declaration, which the
  sentence spells both ways. An older reader refuses a manifest carrying
  the key at load (closed vocabulary), never silently a different unit
  shape — an addition under the package-format policy, no `formatVersion`
  bump. Library: `PackageManifest::{budget_units, budget_unit_gaps}`,
  `ValidatorBinding::file_spans`, `glob::unit_gap`,
  `ClaimOrigin::Workspace::operator_level`, `Discovery::layout_notes`.

- **Workspace resolution — one core for the CLI and the editor (RFC
  0019 item 0, steps 0a–0f).** `nml check`, `nml validate`, `nml fix`
  and the new `nml binding <file>...` resolve which manifest binding
  governs a file through `nml_validate::workspace`, and the editor
  resolves through the SAME core: one workspace root per invocation
  (`--root <dir>`, else derived within the `.git` fence — never through
  an author's symlink, whose target is never resolved; with no VCS the
  file's own directory; in the editor, the workspace folder containing
  the file, else — for a document outside every folder — that same
  derivation, never a re-rooting at the file's own parent),
  canonical workspace-relative keys, one glob matcher and one selection
  rule, so the CLI and the editor give one verdict, code and sentence
  for an unbound file in a closed universe (NML2064), a path reached
  through a symlink under a closed binding (NML2083 — byte-identical
  whether or not the link's target exists, a `..` after an absent
  component included; a `..` that pops the root and re-enters it names
  a file inside the root), an ambiguously claimed file (NML2087, an
  error: it validates under no binding), a live input that failed to
  load (NML2088: unreadable, not UTF-8, malformed, past its cap — 256
  KiB for a manifest or project config, 4 MiB for a declared source —
  a stem that is not its declared `name`, an absent, linked or oversized
  declared source), a universe the walk could not enumerate (NML2089:
  65,536 entries or 64 MiB of live inputs per tenant-shaped budget
  unit, 1,048,576 entries or 1 GiB in all, an unlistable directory —
  every file under a spent unit is denied and nothing else is; a
  truncated universe is closed-denied in full, never parse-only), inert
  resolution inputs (NML2080: a tenant's manifest or config inside
  claimed content is never loaded, so it cannot fail or re-root the
  operator's checks), a binding's `strict`, and live grant enforcement
  (NML2064/NML2065). Every discovery read is byte-capped in both front
  ends; in a closed universe every read of the checked file, every
  discovery input and the fixer's rewrite go through a race-free chain
  (`openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` on Linux, a
  per-component `openat(O_NOFOLLOW)` chain elsewhere on unix) anchored
  at the root — the key you minted is the bytes you read — and a FIFO
  or device named as the target is refused before any open. A directory
  argument to `check`, `validate` or `fix` expands to
  the `.nml` files the universe walk itself saw under it — one
  enumeration, the kernel's: the root or a real directory reached
  through no link at or below the root expands at its key
  (`proj/nope/../vendor` enumerates `vendor`), a symlink, a FIFO, a
  dot-file and the policy-skipped `node_modules`, `target` and
  dot-directories are never among the files, a directory under a denied
  unit stays a file candidate so its NML2089 prints, a file reached
  through two spellings is taken once, targets report in argument order
  with each directory's files sorted — so `nml check --root . tenants/`
  is the CI line and no verb can disagree with another about what a
  directory names; `binding` takes files and says so on a directory.
  The editor's index is that same enumeration (its second walker,
  10,000-file cap and second resolver are gone): a root the walk cannot
  enumerate indexes NOTHING and the editor says so once, as a
  `window/logMessage` (NML2089, per root and per spent unit; NML2088
  named too), a file past the 16 MiB `MAX_INDEX_BYTES` — the CLI's own
  target bound — is not indexed and named, and a truncation heals on
  the pull after its cause is gone; unsaved buffers are live resolution
  inputs, a store package auto-associates by the manifest's marker
  directories (never a tenant-committed config), and a per-root
  universe is cached until something it read, a buffer's document-store
  stamp, a stop directory or a store pointer changes. Every verb shares
  one argument parser: `nml <verb> --help`/`-h` (stdout, exit 0, before
  any filesystem access; `nml help <verb>` is the same page, `nml help help`
  the top-level page, and a flag where the command goes a usage error naming
  where flags go — every page wraps at 80 columns and
  lists its exit codes and leads with examples), `--flag=value` and
  `--flag value`, `--` to end the flags, an unknown flag rejected (`-x` included, and an
  empty `--root=`; a `--schema` directory that cannot be read is a usage error in `check` and `fix` (`check` failed per target with exit 1, `fix` degraded to the file alone and exited 0); `--strict` with nothing to enforce is a usage error
  too), and
  two standard switches — `--json` (line-delimited JSON on stdout,
  stderr silent, exit codes unchanged: `diagnostic`, `result`,
  `binding`, `fix`, `parse`, `fmt`, `explain`, `limit` and
  `error` rows, then ONE closing `summary` row on every path carrying
  `formatVersion` (1), `nmlVersion`, the run's `exit`, exact `errors`/
  `warnings`, `root{path, origin}`, `universe`, `closure`, `manifests`,
  `truncatedUnits` and `withheld`; every `error` row a `kind` —
  `usage`, `target` or `run`) and `-q`/`--quiet` (errors only: warnings,
  infos and the explain hint are not printed, the counts stay exact).
  Findings PRINTED per run are capped at 512 by default
  (`--max-findings <n>`, `0` lifts it), allocated fairly per code and
  order-insensitively, with the counts and the exit exact and the
  withheld tally named per code; stderr is buffered on a pipe and
  flushed on exit and on panic. `nml explain` takes `NML2087`,
  `nml2087` or `2087`, `--list`, and `--json` (one `explain` row per
  code); `nml limits` publishes every bound on three axes (`reach`,
  `guards`, `surface`), each constant's doc comment carrying the
  `LIMIT:` line a census checks, every published bound named by a test.
  `nml fix --check` is the CI gate (exit 1 on a pending edit or an
  error no fix repairs, nothing written); `--dry-run` shows the diff and
  exits 0. One explain hint per run names the first error's code, else
  the first warning's; a universe note prints once per run; every path
  and message prints through the sanitizer. `nml-validate`, `nml-fmt`,
  `nml-lsp` and `nml-cli` forbid `unsafe` code. Docs: the error index's
  NML2080/2083/2087/2088/2089 entries carry transcripts `docs_test.py`
  EXECUTES against fixtures (a `sparse=`/`flood=` tag prepares a
  temporary copy for the byte caps and the entry bound; `${ROOT}` and
  `${VERSION}` stand for the run's root and the binary's version), the
  CI guide opens with the four-line gate and documents every `--json`
  row and value with an executed example, and the binding chapter
  tables what a tenant flood can deny per glob shape. Tests: a syn
  source ratchet, a 318-row link matrix with reviewed goldens, the
  `paths`/`paths_oracle` fuzz targets, and the perf tier's wall-clock
  gates (the read-through's race harness, `beneath_race`, runs its
  700-flip forms on the CI perf lane).
- **The `--json` contract as a JSON Schema (RFC 0019 item 0).**
  `docs/json/nml-ndjson-v1.schema.json` is the published contract for
  `formatVersion: 1`: every row type and value vocabulary, every row
  closed (`additionalProperties: false`), `formatVersion` a constant —
  and `scripts/docs_test.py` validates every executed transcript's
  `--json` rows against it with a dependency-free validator that first
  proves it bites. At `formatVersion: 1` the `binding` row gains
  `absent` (the `(absent)` tag's wire twin — an addition); `nml explain`
  takes many codes; `nml limits` publishes `MAX_AUDIT_EXAMPLES` (8), the
  example keys a hidden-directory NML2090 row names beside its exact
  count. `nml version` is a verb like every other: `--help` is a page
  (`nml help version` the same), `--json` is one `version` row then the
  closing row (its `verb` is `version` for `--version` and `-V` too),
  and a surplus argument is refused (it printed the version for any
  argument, had no page and ignored `--json`).
- **Normalize-on-merge (RFC 0025, phases 0–4)** — layer composition now
  runs ONE walk: the merge decides at each level over the raw,
  array-ref-inlined supplies, normalizes only the survivors under the
  decided variant, diagnoses discarded bodies by subtraction under
  their own readings, and folds an identity-item group once — before
  its token materializes, into the lowest surviving body only. The
  composition plan (`build_arm_plan` and its per-position decision
  traces), the per-layer whole-body normalization pass, and the
  token-restatement strip are deleted; one normalizer with a depth
  policy (`ThisLevel`/`Deep`) serves the merge and the seal backstop
  (defaulting keeps the shared positionalizer, `apply_positional`),
  and every compose finding is emitted through a sink
  ordered by a total key (stack position, source, span, code, message).
  The plan-authority and boundary-assertion narrative of earlier
  releases is superseded: the boundary NML2086 assertion stays and is
  now proven live by a fold-tamper seam. Behavior changes, each pinned:
  an identity item's `+` token and its zero-item verdicts now read the
  COMPOSED arm (previously the item's own default arm injected a
  foreign token silently); an NML2043-invalid token never doubles as a
  stated discriminator; an authored equal-value restatement of a sealed
  `+` field is NML2060 (previously silently stripped); rejected and
  switch-displaced bodies — root and dotted — are diagnosed under
  their OWN stated arms, nested positions included; an array-spelled
  base item's composed body now carries its identity token; a gather-
  dropped item is diagnosed through the same token-first reading its
  surviving twin gets — its own token's arm, never the schema default
  (no swallowed and no fabricated zero-item verdicts, a list-typed `+`
  field's empty-array token included). Diagnostic
  order within one layer follows span order (cross-layer stack order is
  unchanged); pairwise-intermediate provenance rows (duplicates, and
  rows for later-displaced winners) collapse to one row per composed
  field. Phase 4 closes the arc with a pure move of `layers.rs`
  into the `layers/` module directory (grants, instances, policy,
  linearize, entries, decide, seal, normalize, `merge/{mod,oneof,union,
  items}` and a per-battery `tests/` tree) — public surface, test
  counts, composed output and timings unchanged.

- **In-string machine repairs for raw policy characters
  (NML0017/NML0018)** — a control or invisible character inside a
  string literal now carries its value-preserving repair(s), the way
  NML0016's in-string CR already carried `\r`. Where one reading is
  provable the fix is singular and `nml fix` auto-applies it (a C0 or
  unmapped-C1 byte becomes its escape); where intent is genuinely
  ambiguous the alternatives are enumerated and NEVER auto-applied —
  the editor offers each as its own action, and the CLI counts the
  finding as not auto-fixable: NEL offers `\n` | `\u{85}` | `…` (the
  Windows-1252 double-decode reading — NEL's message hint teaches all
  three, mirroring its repair arms), the other 26 CP-1252-mapped C1 bytes
  offer escape | mojibake repair, LS/PS offer `\n` | escape, and bidi
  controls, interior BOMs, and tag characters offer remove | escape —
  with the remove arm itself judged: it is offered only where deletion
  provably removes just that character (a sentinel-marked decode
  comparison, plus a relex proving the deletion leaves the string ONE
  clean token — a FEFF holding two quote runs apart, or sitting
  between a CR and an LF, has no sound removal), and where removal is
  refused the set collapses to the singular escape fix, which then
  auto-applies. (`ParseErrorKind::InvisibleCharacter` gains a
  `remove_sound` field — technically breaking for external code
  matching that variant with named fields; `..` patterns are
  unaffected.)
  `nml fix` also converges capped floods in one run: when more
  same-message findings exist than the 128-diagnostic bound renders,
  the convergence gate now charges instances that re-surface from
  behind the cap against the truncation marker's exact reported count
  (previously such a round was read as failed and the run crawled
  one fix per round into its budget); the fix summary's
  "not auto-fixable" count no longer includes advisory info rows, and
  when findings were suppressed past the diagnostic limit the summary
  discloses them beside the count ("(N more suppressed past the
  diagnostic limit)") — hidden findings are unknowns, never folded
  into the classified count.
  Token-position characters still carry no repair (any rewrite is a
  structural guess), and every in-string repair is gated on decode
  itself: the escape is offered only where splicing it leaves the
  decoded value byte-identical, so a character in a multiline string's
  blank edge line, in a blank line whose blankness holds min-indent up,
  in the opening line's dropped padding, or glued to a preceding
  backslash refuses — the exact geometries where an applied escape
  silently changed the value. The judgment itself is bounded: a string
  token past 64 KiB is never decode-judged, so its policy diagnostics
  stand with no machine repair (fail-closed; hostile-only in practice —
  a PEM-sized blob is ~4 KB). Under the hood the fix-vs-did-you-mean textual
  heuristic is gone: each error kind now declares its repair class
  (`Repairs` — none, did-you-mean, singular fix, or alternatives), and
  diagnostic messages, repair text, and formatter output all share one
  escape-spelling function, so the advised and emitted spellings can
  never drift.

- **RFC 0019: instance layer composition (`uses`) and sealed fields** —
  slice 1 (the language kernel). An instance block may declare
  `uses <ref>, …` in its header; the stack linearizes by C3 (in NML's
  reversed orientation — an inconsistent order is NML2077, never a
  heuristic) and composes bottom-up under schema-declared merge
  policies: `#sealed` (write-once from the bottom, equal-value
  restatement included, with the seal backstop binding all three
  variant forms equally: oneof arm switches, union `as` switches
  (RFC 0015 — lowest supplying layer establishes the variant, authored
  or shape-inferred with a synthesized annotation on the resolved body;
  un-annotated upper bodies never switch), and arm-set wholesale
  replacement, judged through the one decision-trace fold),
  `#identity` (merge list items by the four-kind identity pair),
  `#append` (additions only), and the sanctioned `#identity #append`
  pair. `nml check` composes same-file stacks before validation
  (default on; overlays validate their *resolved* body) under the open
  developer context; binding-governed layer grants land with the shared
  resolver core. Schema load now validates policy declarations
  (NML2068) and warns on seals that cannot engage (NML2076, three
  shapes). New diagnostics NML2059–NML2068, NML2076–NML2077, NML2079,
  NML2084, each with an `nml explain` entry; `nml fmt` emits the new
  clause with round-trip coverage. `nml validate` covers `uses` clause
  refs under its unresolved-references contract (same NML2059 wording
  as `check`); compose-blind deserialization of a `uses`-bearing block
  fails closed (the raw body is not the effective config; the
  document-level entry included); and the 16-instance stack cap rejects
  over-wide clauses *before* the linearization merge, in linear time.
  One field is one field across its spellings: a modifier-declared
  field's property/block spellings merge (and seal) together with it.
  `nml fix` composes, so compose-side machine fixes (the equal-value
  NML2060 deletion, NML2077's remove-the-ref) actually apply; `nml
  validate` also owns NML2062's schema-definition form. The editor
  composes too: LSP diagnostics validate the RESOLVED body, so overlays
  show `check`'s findings, not phantom missing-required errors. Arm
  decisions have ONE authority — the pre-pass fold (seal backstop
  included, judged over displaced-arm-normalized bodies) records a
  per-layer trace the merge replays positionally, nested positions
  planned over the surviving parent group — and oneof-typed list
  elements merge arm-aware (seal enforcement inside identity items,
  through the modifier spelling too, with `+` tokens materializing
  through the item's arm). Item identity scopes pair by full identity
  (kind + token). Field and item lookups are mapped/bucketed, keeping
  compose near-linear in body width and list length (numeric item keys
  bucket by canonical value, not by type); the editor caps its
  per-keystroke diagnostic stream with a summary row. Object-typed
  fields always deep-merge their nested contributions, so no scalar or
  modifier spelling can discard a sealed nested body (schema-less and
  dangling-target nested groups still deep-merge structurally); the
  plan keys each path by the same first-wins field the merge resolves;
  and the seal backstop judges a write through the one shared predicate
  across every spelling — one coherent first-wins policy per duplicate
  field name everywhere. Diagnostics honor the denial family's full
  recovery contract (binding + manifest + `nml binding <file>` with the
  real path), NML2077 names order ROTATIONS across three or more
  clauses, NML2061 teaches its fix inline, the discovery-depth NML2066
  reports once with a real span, and the editor composes schema-less
  buffers structurally and carries the schema-load lints for mixed
  buffers. Header clauses never continue across newlines (a trailing
  comma errors and consumes nothing — the next declaration survives);
  `.shared` distributes into modifier-spelled item bodies like every
  other spelling; type-annotation modifiers survive composition; the
  block-form empty modifier draws NML2079 like its sibling spellings;
  clause findings dedup across dependent composes; and every
  `resolve_layers` failure carries a diagnostic. The seal backstop's
  "at any depth" contract is total: the scan descends union-typed
  interiors (every variant the group could establish, fail-closed),
  arm-set inline arm bodies, and oneof-TARGETED arm sets (each
  displaced arm judged under its own discriminator's arm model), and
  union list elements route identity-matched item groups through the
  union authority (seal enforcement, establishment, annotation
  synthesis) instead of merging model-less. Structural (scalar/list)
  union variants are first-class in compose: the lowest supplying
  layer establishes them too, an authored `as` switches away from
  them, and a contribution that can neither merge into the established
  variant nor switch it — a whole-value spelling over a named
  establishment, or an un-annotated body over a structural one — is
  discarded LOUDLY as the new NML2085 rather than silently dropped. A
  bogus `as` on a dependent layer is reported (NML2051) by the merge
  itself, since composition replaces the annotation before the
  validator can see the authored one. All three NML2060 backstop faces
  share one wording owner (position, switch target, count suffix,
  action tail, "sealed here" note), and the bare-overlay
  seals-cannot-engage lint (NML2076) sees union elements too. The
  structural bucket is per-shape: a scalar value and a list value are
  distinct establishments, a scalar↔list cross is a loud NML2085 (one
  bucket let the winner flip with the base's spelling), every
  union-typed group routes through the union authority regardless of
  spelling, and a switch away from a LIST-variant establishment is
  seal-judged over the displaced item bodies under the list variants'
  element models ("a structural group has no seals" is true only for
  scalars). Ambiguity is fail-closed end to end: a keyed body the D2
  oracle refuses composes model-less and un-annotated — composition
  never guesses a variant by source order nor synthesizes an annotation
  that would silence NML2052. The seal scan reaches union-typed LIST
  elements of displaced variants (item paths as `slot[w].field`);
  `nml check` seeds its validator dedup with the composed diagnostics
  (the merge-emitted NML2051 printed twice with a non-`uses` base);
  NML2076 covers a union's list variant with honest advice
  (`#identity` is not grantable there), and NML2068 gets a
  union-element wording. NML2085 leads with the position and its
  establishment and carries an "established here" related note; its
  switch hint never names a first-wins guess. A switch away from a
  list-variant establishment is judged over the displaced LIST — list-
  level `.shared` writes distributed, each item's identity token
  materialized (a positional `+` field is a write), oneof elements under
  the item's own arm, modifier-spelled item blocks included — and the
  judgment is memoized per unchanged group (N rejected switches over M
  sealed items were N full scans, a super-linear DoS axis). Every
  spelling of a union field reaches the union authority (the
  all-modifier short-circuit no longer bypasses it); zero-item entries
  (`= []`, an empty block) at union positions draw NML2079 and never
  establish; an authored `as` above an oracle-ambiguous group PINS it
  rather than switching (nothing was chosen to switch from); ambiguous
  interiors are seal-scanned under every oracle candidate, never the
  resolver's first-wins pick; normalization no longer guesses a
  variant for ambiguous bodies; NML2085 faces are keyed on the
  (establishment, supply) pair the fold recorded, so a discard before a
  later switch reports once; the NML2060 tail is one clause; NML2076
  leads name the shape; NML2051 names a list variant's element honestly
  (no did-you-mean) through one builder the validator and the merge
  share; `nml explain` never rewrites inline code as a link; and the new
  NML2086 names a violated internal composition invariant instead of
  composing silently wrong. The plan and the merge fold ONE supply set
  (every spelling, through one constructor), so a planned union trace
  always aligns and the local refold never judges bodies already
  normalized under the final variant (a fabricated refusal); a
  type-annotation modifier at a union position is a declaration that
  yields to the values (it never routes a group around the authority,
  never seals); a switch away from a list establishment is judged over
  the bare-list WINNER only; `.shared`-only blocks are zero-item entries
  raw and normalized alike, and an all-zero-item union position
  survives as `= []`; arrays keep their spelling at union positions (an
  empty block reads as an empty object downstream); a pin carries the
  pinning layer's own identifier; every believed-unreachable arm in the
  union faces fails LOUD (NML2086) rather than dropping or last-wins;
  NML2085 names an earlier entry in the same layer when that is the
  establishment, and advises resolving an ambiguous lower body; the
  NML2060 count reads `'field' (and N more)`; NML2079 speaks in union
  terms at union positions; and hover summaries of the long index
  entries are their first sentence. A keyed or annotated body at a `#sealed`
  union position that admits items is a WRITE (one zero-item predicate
  — `= []`, an empty modifier, an entry-less un-annotated block — owns
  NML2079, the seal exemption, the modifier overlay's no-op skip and the
  classifier's `Empty`); declarations (type-annotation modifiers) pass
  through beside the composed value on EVERY route and never desync the
  plan (one value predicate shared by the plan's gather and the merge's
  partition); the plan is the authority for union positions (every
  establishment recorded with the supplies' kinds; a misaligned planned
  position is NML2086, never a silent refold); a sealed union position
  still reports a dependent's bogus `as`; the seal scan dedups by hash
  (a wide judgment was O(hits²)); item-scope discards anchor on the
  item; a list establishment's "established here" follows the effective
  list; set and later list variants are unreachable by shape — neither
  judged nor promised by NML2076; a `[]` on a declared scalar modifier
  reaches the composed view (a type error, not a no-op); the validator
  no longer counts a type-annotation modifier as satisfying a required
  field (errata E12); NML2085 says "as an un-annotated body" and
  "resolve the establishing body"; the RFC 0019 code table gained
  NML2085/NML2086 rows; and hover summaries of NML2051/2052/2077/2086
  are their first sentence too. Nothing under a `#sealed` position is
  planned (its surviving body normalizes under its own variant, never a
  rejected upper layer's — errata E13); the plan strips a oneof's
  discriminator entries exactly as the merge does (a union field named
  like the discriminator no longer desynchronizes them); the zero-item
  predicate is total (`= []` included, one owner, typed or untyped);
  set variants count as list-admitting again (reachable by array
  literal); one seal-hit sink owns every dedup (`assigned_seals_into`);
  declarations are gathered as passthrough entries beside the composed
  value (`merge_field` is single-entry again, its dispatch one
  exhaustive match); the composed annotation's source is one replay
  field; a merged identity item keeps the BASE item's span (a
  three-layer chain's notes point at the base); NML2086 says whether
  the contribution was dropped or composed by a local fold; a
  structural establishment's note reads "in force here"; NML2079 names
  the zero-item entry generically; the editor's compose pass is
  guarded (an internal error degrades to raw-text findings plus
  NML2086, never a dark buffer); and a `.shared` line inside a modifier
  block is a parse error instead of a silent drop. The displaced-list
  seal judgment binds to the union's first `List` variant — the one
  block-shaped items resolve to — never to a `set<T>` variant that
  precedes it (a switch could discard sealed items silently); block-
  shaped items at a union position normalize under that variant's
  element model like a plain list's items (their nested zero-item
  entries are warned, their array-spelled lists re-spelled), under a
  bracketed item path the plan never writes; the plan, normalization
  and the merge resolve a name in ONE order (a model before a oneof of
  the same name — a colliding name was planned under one reading and
  merged under the other); the plan hides a oneof's discriminator from
  its supply gather instead of cloning stripped bodies; the ownership
  order of a field group is a table (`route_of`); declarations precede
  their value in the composed view (declare, then assign); the survivor
  rule, the discriminator-entry predicate and the establishing supply
  each have one owner; the seal sink dedups by (path, file, span); the
  invariant debug assertion sits at the compose boundary (the diagnostic
  and the fail-safe composition are observable in every build); the
  editor's guard anchors its NML2086 at the buffer start (a span-less
  finding was dropped before the editor saw it) and guards its fallback
  too; a non-item line in a modifier block names its own kind ("found a
  shared property") at its own position; the validator checks EVERY
  discriminator-named entry (a dependent's `kind = 5` was laundered
  behind the re-added string discriminator); and the provenance of an
  identity-merged item is its base position (its fields keep their own
  writers' rows); and an `nml fix` deletion takes its whole line, never leaving an
  indentation-only husk behind (later superseded by RFC 0023's
  structural resolver, below)
  — `nml explain NML2079` now covers union positions, and its hover
  summary is its first sentence. Then: the ONE resolution order holds
  at the ROOT too (`SchemaIndex::nameable`, model before oneof, total
  matches at every site — the plan and the merge HAD read a colliding
  root oneof-first while normalization, the positionalizer and the
  validator read the model; no longer); list items resolve their normalization
  vocabulary PER ITEM from the element type (`Vocab::Items`), so oneof-
  and union-element items are peers of model-element items (their
  zero-item entries are warned, their arrays re-spelled) and a block
  modifier's items normalize like a nested block's; machine deletions are STRUCTURAL
  (RFC 0023): one resolver in nml-core computes every fix's bytes for
  `nml fix` AND the editor's quick-fix by token walks over the lossless
  tree — entry rows, `uses` clauses, clause references, the colon rule
  on an emptied clause-carrying header — with every refusal
  per-suggestion and printed (`fix refused: …`), the
  structural-injection guard owned in the same place (the editor's
  quick-fix path had none), editor staleness settled by cache
  membership, and `nml fix`'s round gate a multiset decrement over the
  applied diagnostics (a repair that reveals the next finding lands;
  a failed round retries its first applied candidate alone, and the
  round budget scales with the finding count); a deletion targeting a
  row INSIDE a `.shared` block is refused outright — the row is
  distributed into every named item, and the block-form NML2060 fix
  silently stripped a shared default from every sibling; the injection
  guard's refusal set equals the render-escape set (controls, the
  Trojan-Source bidi controls, U+2028/U+2029), one predicate for the
  guard, the message renderer and the CLI's note lines; a
  composed entry carries the span, name and provenance of the HEAD of
  its surviving group (RFC 0019 E15 — the switching layer after an
  accepted switch, the base otherwise), so two switching dependents'
  findings keep two homes instead of collapsing onto the base's under
  the one-home dedup key, and NML2085's item-scope note lands on the
  establishing item; discriminator stripping is by NAME with non-string
  entries passed through beside the canonical one (E16 — each drawing
  NML2042 at its author's span; `kind = 6` no longer overlays
  `kind = 5`, and the NML2054 shape draws NML2042, not NML2085); the
  NML2060 backstop counts DISTINCT SEALED FIELDS with the assignment
  count when it exceeds the fields (E17 — `(and 1 more field)`,
  `(2 assignments)`), over a hashed structural identity whose scalar
  item keys are never printed or `Debug`-leaked, with one `sealed here`
  note per assignment carrying its own file (`Related.source` lands;
  both renderers locate a note in its own file, falling back loudly); a backstop
  rejection carries one `sealed here` note per discarded assignment
  (the first four); the validator materializes a named item's name into
  a oneof element's arm (`- a: kind = "a"` under `[]oo` with `name
  string+` read as a missing required field on every raw block);
  `FieldRoute` no longer defines identity-equality; plan keys are
  debug-asserted dotted (never an item scope); a test-only tamper seam
  proves the compose boundary's invariant assertion is live; and the
  NML0016 fix for a bare CR INSIDE a string literal is the `\r`
  escape, and a CR in token position has NO machine fix (`nml fix`
  deleted both — a silent value change inside a string, and glued
  lines on a CR-terminated old-Mac file).

### Changed

- **The editor refuses an ambiguously claimed file as the CLI does.** A
  file two live manifests claim gets ONE row in the editor — the kernel's
  NML2087, the CLI's sentence naming every claimant — and is neither
  validated nor composed until a claim is narrowed; it used to publish
  NML2087 beside the ambiguous form of NML2064 and validate the file
  anyway. A document whose path no key can carry (past the 64-component
  bound, a component that is not UTF-8) is refused with the kernel's
  sentence as one error row, as `nml check` fails that target — it used
  to be a warning over a document validated in the open registry mode.
  Navigation (document symbols, hover) still answers on a refused
  document: what is withheld is the verdict.

- **Pin and opt-out actions write into the nearest LIVE project config.**
  The editor's `Pin schema package` and `Disable schema auto-association`
  actions target the `nml-project.nml` the kernel resolves pins from
  (`Universe::nearest_config`, through the overlay: an unsaved config at
  that path is the file), and create one at the binding's anchor
  otherwise. A disk walk used to pick the nearest FILE: a pin could land
  in an inert tenant-committed config (changing nothing), and an opt-out
  already written there hid the action while the file stayed
  auto-associated. Library: `PackageResolver::project_config_path_for`;
  `Binding.source` (`DefinitionSource`, removed) is `Binding.class`
  (the kernel's `ClaimClass`) beside `Binding.manifest`.

- **`nml fix` refuses a directory behind a link in the checker's words.** A
  directory the universe walk did not enter — reached through a link an
  open universe follows (`link/subdir`) — is refused by every per-file
  pipeline with one sentence (`is a directory the universe walk did not
  enter (behind a link, or inside a denied unit) — name the files, or pass
  --root to a tree the walk can list`, exit 1); `fix` used to read it as a
  file and print the OS's `Is a directory`.
- **A target outside the workspace root is refused before the universe
  walk (RFC 0019 item 0).** Every argument is classified against the
  root by the kernel (`SourceKey::classify`, closed trust — never a
  lexical rule: a link followed by `..` is still the walk's NML2083, exit
  1) BEFORE any universe is built, so a wrong invocation is refused with
  nothing walked, nothing listed and nothing printed by `binding`. One
  verdict moves: a broken universe AND an outside target used to exit 1
  with NML2088 and never mention the outside target; it exits 2 with the
  outside sentence. On the wire an outside-root refusal's closing row
  keeps `root` and carries `universe`, `closure`, `manifests`,
  `truncatedUnits` and `skipped` as `null` (no walk ran; the schema
  already allowed it).
- **The CLI's own lines spell the root from the working directory.** The
  `note: workspace root …` line, `binding`'s `root` line, the
  outside-root refusal and the derived-root tag's shadowing entry, marker
  and `--root` advice spell the root as `git status` spells paths — `.`
  at the root, `..` from inside it, `tenants/cu` from above it, the
  canonical path when neither contains the other. Kernel sentences
  (NML2064's closed form, NML2089) keep the canonical root — one text for
  the CLI and the editor — and every `--json` `root.path` stays
  canonical.
- **`nml check` streams its validator findings.** The validator pushes
  each finding into a `DiagnosticSink` as it is derived and the CLI
  reports and tallies it there, never holding a flood whole (a
  980,000-finding file: 1,240 MB → 780 MB, the parse tree itself being
  610 MB); the counts stay exact. Library: `nml_core::diagnostic::
  DiagnosticSink` (object-safe; `Vec<Diagnostic>` is one) and
  `SchemaValidator::validate_into`/`validate_definitions_into` — the
  `Vec`-returning forms are unchanged.
- **The editor holds no more validator findings than it publishes.** Its
  validator pass streams into `nml_core::diagnostic::Bounded` — the first
  `MAX_DIAGNOSTICS` (500) findings kept, every further one counted,
  exactly — through `nml_core::layers::Deduped`, the one deduplication
  rule for every front end, seeded by `ComposedFile::dedup_seed` (the
  composed findings' keys when the file composes, no set at all when it
  does not), so the `N further finding(s) not shown` row is the truth
  and a flood costs the tranche, not the flood: a 1,200-finding document
  publishes 500 and says `700 further`. The load pass's suppression of
  the validator's composition verdicts is applied ahead of the tranche
  (`nml_core::diagnostic::Filtered`), so a suppressed verdict neither
  fills it nor rides the count — 600 owned verdicts beside three errors
  publish the three and no summary row.
- **`nml limits` is generated from the declarations.** Each bound's
  `LIMIT:` doc line carries its human spelling (`shown="16 MiB"`) and an
  unpublished bound its `UNPUBLISHED: <why>`; the table the verb prints
  (`nml-cli/src/limits_table.rs`) is generated from those lines by the
  census, refused when stale, regenerated under `NML_UPDATE_GOLDEN=1`
  and reviewed in the diff. The row set is unchanged; the human order
  follows the declarations (reach, crate, file, line), so ten rows move.
- **A published bound's `shown` is checked against its declaration.**
  `shown=` is the one hand-written field of a row; the census now
  evaluates every declaration its grammar covers (integer literals, `*`,
  `+`, `as`, `<int>::MAX`, a sibling bound by name) and pins each
  `shown` to that value in the table's vocabulary — decimal,
  `human_bytes` (a whole gibibyte prints `1 GiB`), `N bytes`, `~x.ye+N
  <unit>`, `10^k - 1` — naming the rows it cannot evaluate, so a new
  shape is a decision. The one wrong rendering it found is corrected:
  `nml-core::duration::STD_MAX_NANOS` printed `~5.8e29 ns` for a value of
  `~1.8e28 ns`. A stale table fails naming its first differing line
  (`declared:` / `committed:`).
- **A package manifest may declare its budget units.** `budgetUnits:`
  (anchor-relative directory patterns — `tenants/*`, one depth each, no
  `**`) replaces the inference from the binding globs' last wildcard run
  for that manifest, refused at load (NML2088) when it would leave
  content a wildcard glob reaches in the root unit — a declaration never
  narrows isolation. Under `tenants/*/flows/**` one tenant's flood in
  `tenants/<x>/other/` used to truncate the whole universe; declared
  `["tenants/*"]`, it denies that tenant alone. (The gap-layout lint,
  NML2092, is under *Added*.)
- **The root is named once per run, never inside a per-file sentence.**
  NML2064's closed form reads `no binding governs this file in the
  closed universe (2 manifest(s) discovered)` and NML2089's three forms
  `cannot enumerate manifests: the walk stopped at …` / `… the
  live-input budget … was spent reading …` — the absolute root they
  embedded is the run's fact, stated once where each front end states
  its universe: the closing `summary` row's `root.path`, `nml binding`'s
  `root` line, the `note: workspace root …` line for a derivation an
  operator could not see, the editor's `nml/schemaInfo`. One sentence
  for both front ends, spelled the same wherever the run stands (and no
  120-character path on every row of a CI log). Library:
  `UnboundContext::Closed { claims }` (no `root`), `Grant::Unbound {
  closed: Option<usize> }`, `Discovery::{universe_errors, notes_for}`
  and `WorkspaceRoot` (no `label`) follow.

- **Human output under a non-UTF-8 locale is ASCII.** When the locale's
  codeset (`LC_ALL`, else `LC_CTYPE`, else `LANG`) is not UTF-8 — and
  never on a Windows console — the tool's own typography folds at the
  human sinks (`—` to `--`, `…` to `...`, and the other glyphs the
  sentences and the error index use — `·` to `|`, `×` to `x`, `→` to
  `->`) and any other non-ASCII character (a walked name, an echoed
  value) is spelled `\u{XXXX}` rather than respelled (a name that uses
  one of the tool's own glyphs folds with it); `NML_UNICODE=0|1` overrides (cargo's
  `term.unicode` model). The `--json` stream stays UTF-8 and a `fix
  --dry-run` diff stays the file's own bytes. Every sentence, pin and
  transcript keeps its Unicode spelling; the harnesses pin
  `NML_UNICODE=1`, so a developer's `LANG=C` shell changes no verdict.

- **Refusals read like the lines around them.** A `--schema` directory
  that cannot be read says why in the tool's words (`no such directory`,
  `permission denied`) — never `io::Error`'s `(os error N)`; a FIFO,
  socket or device named as a target says what it is and the remedy
  (it said `refused before open`); a refused root derivation spells the
  target as typed and the marker and the fence from the working
  directory, as every other CLI line spells the root, and for a marker
  above the fence names the `--root` that checks under it (`--root .
  checks under that manifest`); the withheld-findings trailer is a
  `note:` like every other advisory line (it was `nml:`); `nml binding`
  on an ambiguously claimed file says which code denies it (`the file
  is denied (NML2087)`, not the NML2064 a composing file never reaches);
  and the top-level page wraps at 80 columns like every verb's. The VS
  Code extension's status-bar tooltip says how the document's universe
  was fixed (`nml/schemaInfo`'s `rootOrigin`, `rootFence`,
  `rootShadowed`) beside the root. Library: `RootError::display_with`
  (the sentence with paths spelled by the caller; `Display` spells them
  canonically).
- **A binding that cannot build its validator is the kernel's verdict
  (NML2091), in both front ends (RFC 0019 item 0).** A live, unambiguous
  binding whose declared schema source fails to load (a parse error, a
  duplicate definition, a cycle) yields one error row on every file it
  governs — `binding 'tenantFlows' of demo.package.nml cannot build its
  validator: declared source `core` failed to load at core.model.nml:3:1:
  … (and N more) — the file validates under no binding until the source
  loads` — with the source's first finding as a `note:` line (the
  editor's `relatedInformation`, a jump to the line); the file validates
  under NOTHING, the manifest's other bindings are unaffected, and the
  source document keeps its own findings. `nml check`, `nml validate`,
  `nml fix` exit 1 (the universe's content, not the invocation — it was
  a usage error, exit 2, in an uncoded sentence); `nml binding` shows the
  row under `notes` and exits 1 (it exited 0, fully bound). The editor
  refuses the file with the same row (it "fell back to basic validation"
  — a registry verdict `nml check` never gives). The kernel builds every
  binding's validator ONCE per universe (`workspace::ValidatorMemo`, held
  by the discovery; `Resolved::validator`) — `nml check tenants/` built
  it once per target, and the editor kept a cache of its own — and a
  store package that fails to load says `the package binds nothing until
  then` (it said `falling back to basic validation`). A bound file's
  strictness is its binding's own in every front end: `--strict` no
  longer applies to a manifest-governed file (it made the CI's verdict
  stricter than the editor's on the same file) and the run says so once,
  naming the binding to set `strict = true` on.
- **The RFC 0025 composition oracle is a committed golden (RFC 0019
  item 0, round 88).** The hidden `compose-dump` verb, its `compose`
  row and the `compose-dump` value of the closing row's `verb`
  vocabulary are removed with `scripts/compose_oracle.py`, its
  allow-list and the `just oracle-layers` recipe — a dev-time surface
  the guide marked hidden and no stability surface, so `formatVersion`
  stays `1`. What the two binaries compared is pinned in-tree instead:
  every layer-battery composition and every layer fixture is composed
  through `compose_file` and its observable (each composing
  declaration's composed body, its provenance table, the rendered
  diagnostics in the sink's order) is checked against
  `crates/nml-core/src/layers/tests/compose.golden` on every `cargo
  test` (the five generated stacks under `tests/fixtures/layers/perf/`
  on the `perf_` tier beside their timing gates); an intended change is
  a golden update (`NML_UPDATE_GOLDEN=1
  cargo test -p nml-core --lib layers`) reviewed in the diff, whose
  diagnostics lines read as the change; `NML_COMPOSE_DUMP=<dir>` writes
  every observable for a two-commit `diff -r`. The oracle's allow-list
  exempted a case whole; a golden line is exact for every case.
- **A document outside every workspace folder resolves under the root
  the kernel derives (RFC 0019 item 0, R1's third rung).** The editor
  calls the derivation `nml check <file>` makes — `WorkspaceRoot::derive`:
  the `.git` fence, the outermost marker within it, the shadow refusal,
  the component cap — for a document no workspace folder contains (VS
  Code's single-file mode, a file opened from another checkout), caches
  one universe per derived root while a buffer sits under it, and says
  so once (`derived a workspace root at `…` (derivedVcsFence) for
  documents outside every workspace folder`); `nml/schemaInfo` carries
  `rootOrigin` (`editor`, `derivedVcsFence`, `derivedTargetDir`). A
  derivation the kernel refuses (a planted `.git` file below the
  operator's marker) is one row — the kernel's sentence, then `open its
  workspace folder` — and the document validates under nothing, as the
  CLI runs nothing (exit 2). A fence that is no directory (a linked
  worktree's, a submodule's or a planted `.git` file) or a shadow above
  it (another `.git`, a root marker) is disclosed as the CLI discloses
  it — the kernel's one fact sentence, as a warning — and
  `nml/schemaInfo` carries `rootFence` and `rootShadowed` beside
  `rootOrigin` (the `--json` root object's vocabulary). Inside a folder
  the folder is the universe, never a derivation. A project config
  beside a folder-less document governs it again (through the kernel's
  nearest live config; the global the editor used to keep did it by
  accident). Workspace folders added or removed while the server runs
  are honoured (`workspace/didChangeWorkspaceFolders`; they were read at
  `initialize` only), and a folder added over a root the kernel had
  derived re-anchors its documents (`editor`). Such a document used to
  be unbound under the embedder default: no findings, whatever `nml
  check` said.
- **An open buffer past 16 MiB is refused in the editor (RFC 0019 item
  0).** `MAX_INDEX_BYTES` bounds every workspace file the editor holds,
  open buffers included: a buffer past it is not stored — nothing parses
  it — and reports one row in the kernel's cap sentence (`too large:
  300 MiB (…) — an open document is read only up to 16 MiB (16777216
  bytes)`), which `nml/schemaInfo` carries too; a change under the bound
  validates as usual, and a refused document is never parsed. An open
  buffer used to be judged at any size: a 300 MiB buffer cost 244 s and
  11 GB (23 GB of NUL bytes) and reported green where `nml check`
  refuses the same file in 24 ms. Closing an indexed document re-reads
  it from disk under the same bound — the LSP's truth after `didClose`
  is the disk's (the last buffer text used to stay in the index, and a
  refused one left the document out of it until a watcher event).
- **The wasm editor's directory listings fail closed (RFC 0019 item
  0).** An entry whose kind cannot be read refuses the whole listing
  (`Truncation::Unreadable`, closed-denied), through the kernel's one
  listing rule (`workspace::fs::listing`) every std-listing backend
  goes through; the wasm shim used to skip such an entry (a manifest
  could vanish from discovery and the universe read as open).
- **One cap sentence, the invocation's own mistakes, one row per hidden
  directory (RFC 0019 item 0).** An input past its byte cap is refused
  in the kernel's one sentence (`too large: 5 MiB (5242880 bytes) — a
  declared schema source is read only up to 4 MiB (4194304 bytes)`) by
  the CLI and the editor alike — the editor's index and its watched-file
  reads included (`… is not indexed: too large: 16384 KiB (16777217
  bytes) — an indexed workspace file is read only up to 16 MiB (16777216
  bytes)`, where the index said `exceeds the 16777216-byte bound`) — and
  the editor caps a buffer-served discovery input as it caps a disk read
  (an indexed 5 MiB source used to load in the editor and judge the
  tenant's file under it while `nml check` refused it); a watched `.nml`
  that appears or grows while the editor is open is read under the same
  16 MiB bound, never whole into the store (+96 MB for a 48 MiB file),
  and its refusal is said through `window/logMessage`, never skipped
  silently. The whole-universe NML2089 row ends in `remove what stopped
  the walk` from the kernel, so the editor shows the remedy too; the CLI
  appends only its `--root` clause. The derived-root tag says what the
  kernel knows — `(derived within the .git fence — pass --root to pin)`,
  and for a marker above a directory fence `…; SHADOWED by the root
  marker `X` above it — pass --root to pin, or --root <X's directory> to
  check under that universe)` — where it said "outermost manifest" in
  open contexts with no manifest at all. **Exit codes:** a target
  outside the workspace root (`--root` or derived) is the invocation's
  mistake — exit 2 in every verb, refused before any target runs (it was
  a per-target failure `check`, `validate` and `fix` continued past,
  exit 1, while `binding` exited 2 for the same sentence); `--strict`
  with nothing to enforce exits 2 (was 1); a target inside an unreadable
  or spent budget unit gets the unit's own NML2089 row and exit 1 in
  `check`, `validate`, `fix` AND `binding`, answered before any probe
  under the unit (it reached the OS's `permission denied on a path
  component` as a bare error — exit 1 in `check`, 2 in `binding` — with
  the unit's row unspoken); an unknown short flag (`-x`, `-qj`) and an
  empty value (`--root=` or `--root ""`, an unset shell variable) are usage
  errors, never a file name or the working directory; an empty argument
  (`""`) is a usage error in every verb (`check ""` was a file candidate
  that failed at the read, exit 1, while `binding ""` exited 2); `nml
  help help`,
  `nml help --help` and a flag where the command goes are the top-level
  page or a usage error (`help help` overflowed the stack, exit 134).
  **Sentences:** NML2088's manifest-validation form names where the
  finding sits — `manifest failed validation at tenant.package.nml:3:1:
  …` — in place of a `(1 error(s))` count (`(and N more)` past one);
  the gate over skipped content reports a hidden directory ONCE — the
  directory, the exact count of `.nml` files beneath it and up to eight
  of their keys (`the walk skipped `tenants/cu/.hidden`: a dot-directory
  it never enters, holding 2001 `.nml` file(s) no verb judged (`…`, and
  1993 more) — …`) — where it minted one error row per file (a tenant's
  300,000 committed hidden files cost `nml check .` 89.5 s and 259 MB,
  ~18 minutes and ~1 GB at the audit bound; now 0.4 s and 26 MB); `nml
  binding` on an absent file says `(absent)` on its `file` line, under
  `-q` too (the exit is unchanged); `nml explain A B …` prints one
  document per code (a blank line between two), one `explain` row each
  under `--json`, and an unknown code among many is its own error row
  with exit 1; `nml limits` prints the two input caps as `256 KiB` /
  `4 MiB`. **The editor:** an inert input's NML2080 note is reported
  ONCE, on the inert input's own document, at its declaration, as
  information — never on every file beneath it (three permanent 1:1
  warnings on every tenant file, for inputs the tenant could not act
  on; the CLI keeps its once-per-run line); under a universe the walk
  could not enumerate, or for a file under a denied unit, the editor
  validates NOTHING — the NML2089 row is the whole report, as `nml
  check` validates nothing (NML2064 used to claim `0 manifest(s)
  discovered` for a walk that did not finish). **Work and memory:** the
  unit question charges a probe only for an operator-level claim whose
  manifest directory contains the listed directory (N such manifests in
  subtrees no glob reaches cost every listed directory N probes — a
  whole-universe denial from 512 committed entries); the printed-note
  dedup is a set (300,000 universe rows took 90 s to deduplicate
  linearly); the fixer's temp file is created at the original's
  permission bits, so a `0600` file's content is never world-readable
  during the write. **The docs gate** caps a transcript's `flood=` at
  2,097,152 files and `sparse=` at 64 MiB.
- **Workspace resolution and the operator surface (RFC 0019 item 0).**
  The 65,536-entry discovery bound, the 64 MiB live-input byte budget
  and an unlistable directory are each charged per tenant-shaped BUDGET
  UNIT — every directory a live workspace claim's glob reaches at the
  start of its last run of wildcard directory segments (`tenants/<x>`
  for `tenants/**/*.flow.nml` and `tenants/*/flows/*.flow.nml`,
  `orgs/<o>/tenants/<t>` for `orgs/*/tenants/**`) — attributed outermost
  across the claims a tenant could commit and innermost among
  operator-level claims, with the root unit and universe-wide backstops
  (1,048,576 entries, 1 GiB of live inputs) keeping the whole-universe
  truncation where it was; a spent unit denies every file under it
  (NML2089 on the file, naming the unit, the stop and the bound) and
  nothing else, nothing under it is kept, and a tenant's manifest in a
  gap of the operator's glob mints no unit. Declared schema sources are
  read once per `(directory, file)` and shared across every manifest
  that declares them. **Exit codes:** a usage error — an unknown command, an unknown flag,
  a missing or surplus argument, a flag without its value, a `--root`
  that is no directory — exits 2 in EVERY verb (`parse`, `fmt`,
  `explain` and `limits` included; it was 1 in all but `binding`), and
  under `--json` is an `error` row of kind `usage`; `fix` exits 1 when
  a path could not be fixed — absent, unreadable, or REFUSED by the
  universe (NML2083, NML2087, NML2089), under `--check` and without it
  alike (a refused path used to exit 0 as "not auto-fixable" while an
  absent one exited 1); `binding` on a directory exits 2 and says so,
  and exits 1 whenever the run reported an error-severity finding — a
  universe error such as NML2088 included — even where a binding still
  stands, so its exit and its closing row agree like every verb's (a
  foreign-stem impostor beside the governing manifest exited 0 under a
  closing row saying `errors: 1`); an argument that is not UTF-8 is a
  usage error (exit 2) rather than a panic; a root the tool refuses to
  derive — a manifest above a `.git` fence that is no directory, no
  fence within 64 directories, a shadow check the bound cut short — is
  a usage error too (exit 2, `pass --root`).
  **The `--json` stream, at `formatVersion: 1`:** one lowerCamel word
  per enumerated value — `root.origin` is `explicit`, `editor`,
  `derivedVcsFence` or `derivedTargetDir` (was `--root`,
  `derived: vcs-fence`, …), `binding.step` is `pinned` or
  `autoAssociated` (was `auto-associated`), the printing budget on the
  `summary` row is `withheld` (was `truncated`, a name it shared with
  the walk's `truncatedUnits`), `summary.targets` counts the files after
  directory expansion; and a workspace file is named by its
  workspace-relative KEY on every `source` and `related[].source`
  field, however the target was typed (step 0f — the spelling the
  `binding` row, the universe notes and the editor already used; the
  human `file:line:col` prefix keeps the path as typed; a `--schema`
  source, no workspace file, keeps its basename; the NML2064 advice
  `run nml binding <file>` names the key); the `root` object carries
  `fence` (the fence entry's kind: `dir`, `file`, `symlink`, `other`)
  and `shadowed` (the entry above the fence that shadows the derived
  universe: another `.git`, or a manifest above a directory fence),
  the closing row `skipped` (`{byWhy, rows, shown, hidden}` — what the
  walk left out of its enumeration by policy, `symlink`, `fifo`,
  `dotDirectory`, `dotFile`, `policyDirectory`, `unkeyableName`,
  `componentBound`: exact counts per
  reason, the rows under the `--max-findings` budget), and
  `root.origin` no longer takes `derivedComponentCap` (that walk
  refuses instead); every row is ONE
  physical line — a character serde leaves raw that a line-splitting
  consumer treats as a line break or that reorders text (NEL, LS, PS,
  bidi controls, U+FEFF, C1 controls) is re-spelled `\uXXXX`, the value
  unchanged (a file name carrying U+2028 split 21 of 34 rows for a
  `splitlines()` consumer). `fmt` reports a file that
  does not parse exactly as `parse` does — every parse error with its
  code — instead of the first, uncoded. **The CI gate reports what the
  walk skipped (r80-sec F4):** under a directory target, `check`,
  `validate` and `fix --check` fail on the `.nml` content the walk left
  out — a symlinked `.nml` (NML2083 under a closed universe, in the
  resolver's words; the new NML2090 in an open one), a `.nml` FIFO, a
  `.nml` dot-file, every `.nml` a dot-directory holds (an audit under
  ONE budget per run, the universe's 1,048,576-entry backstop, `.git`
  excepted), a hidden directory too large to audit — and name a
  symlinked directory as a warning; `nml fix --check --root . .` said
  "0 of 3 file(s)", exit 0, over a planted link that was NML2083 the
  moment it was named. **Derivation is fail-closed (r80-sec F6, F9):** a
  manifest or `nml-project.nml` ABOVE a `.git` fence that is no
  directory refuses the derivation, naming both (a submodule's `.git`
  file or a planted entry below the operator's manifest re-fenced the
  `--root`-less check at the tenant's directory, silently: a file
  crafted valid under the tenant's manifest read `ok`); above a `.git`
  DIRECTORY — a nested checkout, a stray manifest in a parent, shapes
  no commit produces — the fence holds and the marker is reported as
  shadowing the derived root; another `.git` above the fence is
  reported the same way; a `.git` FILE fence or a shadowed root is
  disclosed once on stderr in human mode; a shadow check the 64-
  directory bound cuts short refuses rather than deriving unchecked;
  and a walk that meets no `.git` within 64 directories derives nothing
  (exit 2) instead of an OPEN universe at the target's own directory in
  which a file 70 directories below the operator's manifest validated
  under no binding.
  **The walk's work is budgeted (r80-sec F1):** settlement is linear in
  the live manifests (no per-call sort or clone; claims grouped by name
  once; the operator-level claims kept beside the full vector; a
  32,000-manifest gap layout took 92–102 s per `nml check` of ANY file
  under the root), and every settlement probe is charged to the entry
  budget of the unit being settled — no new bound: a probe is an entry
  — so N sibling manifests whose globs reach nothing over N inputs
  beneath them can no longer cost N² uncharged work; a live manifest
  set that spends the budget on probes truncates its unit exactly as an
  entry flood does. `parse` and `fmt` read through the 16 MiB target
  cap (one 256 MiB zero-file reached 20–22 GB resident); the fixer's
  rewrite keeps the original's permission bits only — never a set-uid,
  set-gid or sticky bit an author set (`4755` → `755`); a tenant
  entry named with a `\` is charged and skipped like a non-UTF-8 one
  (a debug build panicked on it, aborting every check under the root)
  — and REPORTED: an entry whose name no key can carry is the gate's
  NML2090 under the directory holding it (`unkeyableName` on the
  closing row, its row carrying `entry`, the name), naming the entry
  and its kind — a directory the walk never entered, a link, a
  `.nml`-named file or special entry; a tenant's `ev\il/hidden.flow.nml`
  under the operator's glob passed `nml check .` unjudged, exit 0, and
  in a hidden directory such a `.nml` name is now counted and such a
  directory leaves the audit incomplete; and a directory at the
  64-component bound — never listed, nothing beneath it keyable — is
  the gate's NML2090 too (`componentBound`), where it was an exact,
  silent skip: 63 nested directories hid a tenant's content from
  `nml check .`. **The editor** resolves through
  the kernel (its own resolver, claim scan and index walker are
  deleted): a file outside every workspace root is unbound (it was
  re-rooted at its own directory), a workspace `demo` shadows the
  store's `demo` universe-wide by name (rule 3), `nml-project.nml` no
  longer anchors store globs (a manifest's marker directories do), a
  stem/name mismatch closes the universe (NML2088) instead of falling
  through, the ambiguous-claim note is NML2087 at error severity in the
  kernel's sentence, a degraded note carries the kernel row's severity
  and code (`nml/schemaInfo` `notes[].severity` may now be `error`),
  and a root the walk cannot enumerate indexes nothing (loud) where the
  first 10,000 files were indexed and the rest silently dropped; a
  buffer opened THROUGH a linked directory is a buffer at the link in
  the overlay, never at its target (an unsaved manifest behind a link
  used to be a live resolution input that could close the universe);
  and a document reads the kernel's nearest live `nml-project.nml`
  only — a file outside every workspace root reads the embedder
  default tooling config (it read the last indexed or edited root's
  modifiers and namespaces from a global the editor no longer keeps).
  **Sentences:** the NML2089 unit row speaks in one voice (lowercase,
  `;`, no full stop), the truncation advice names both remedies, `fix`
  says "would apply", the absent-directory sentence no longer sells
  `--root` as a fix for a typo. **Library (breaking):**
  `SchemaPackage.sources` is `Vec<(String, Arc<str>)>` and
  `from_parts` is generic over `T: Into<Arc<str>>`; `workspace::
  Truncation` gains `LiveInputBytes { key }` and `TotalLiveInputBytes {
  key }`; `Universe` gains `truncated_units` and `truncated_unit()`;
  `Discovery` gains `truncated_units`, `files`, `nml_files_under(&key)`
  (the one enumeration) and `unit_errors()`; `discover` gains
  `MAX_TOTAL_ENTRIES`, `MAX_LIVE_INPUT_BYTES`, `MAX_TOTAL_LIVE_INPUT_BYTES`;
  `glob::literal_prefix_len` is `glob::unit_prefix_len`;
  `Resolved` gains `rejection: Option<PathError>` and `diag::
  path_finding_typed(err, typed)` renders a rejection with the spelling
  as typed beside the key; `RootOrigin::label` is `RootOrigin::tag`
  (the `--json` value); `RootOrigin::Derived` gains `shadowed:
  Option<Shadow>` (`Shadow::{Git, Marker}`, the entry's path) and is no
  longer `Copy`, `Fence::Vcs` carries the
  fence entry's `kind: EntryKind` (`Fence::entry_tag`), `Fence::
  ComponentCap` is removed, `RootError` gains `ComponentCap { last }`,
  `Shadowed { marker, fence }` and `ShadowUnchecked { fence, last }`;
  `Discovery` gains `skipped:
  Vec<Skipped>` (`Skip::tag`), `discover` gains `audit_hidden` (over an
  `AuditBudget` the caller holds per run) and
  `HiddenAudit`, `UnitBound::Entries` and `Truncation::Entries` may name
  a settled INPUT as the stop; `MockFs::other` (feature
  `test-support`) scripts a FIFO; `BindingStep::tag` and `ClaimClass::tag` are the
  `--json` values beside the human `label`s, `SourceKey::dir_label`
  spells the root directory as `.`; `GrantResolver` is replaced by the
  owned `workspace::Grant` — `Resolved.grant`, the universe's
  composition verdict for the file, copied out of the governing binding
  once where the file is resolved (`Grant::{of, unbound, open}`) and
  handed to `compose_file` by the CLI and the editor alike, so neither
  asks `governing` a second time (nor `fix` once per round); `GrantResolver::
  judge_ref`, `Judged` and `SourceKey::parse_authored` are removed (no
  production caller until RFC 0020 mints target keys; the fuzz drivers
  carry the authored-path gate locally); `diag::{code_for,
  path_finding, inert_input, nested_marker, universe_truncated,
  unit_truncated, input_unloadable, ambiguous_claim}`,
  `Discovery::truncation_error`, `grants::decide_ref` (inside `Grant`),
  the CLI's `pipeline::{source_name, closure_label}` (the key is the name;
  `Closure::tag` and `UnitBound::tag` are the kernel's `--json` words like
  every other tag), its NML2090 sentences (`diag::{skipped, skipped_under,
  audit_incomplete}` — the kernel's one code-minting site, total over
  `Skip`) and its take-cap-plus-one read (`workspace::read_beneath`, the
  one reader — open, cap and UTF-8 — under both front ends);
  `ManifestClaim::new`, `SourceKey::is_strict_ancestor_of`,
  `paths::{is_manifest_name, PROJECT_CONFIG_NAME}` are crate-private and
  `Universe::unloadable` private (no consumer outside the kernel);
  `nml-lsp`'s `DegradedNote { severity, code, span }` replaces `warning:
  bool`, its `Resolved` carries the universe's composition `grant`
  (the kernel's `workspace::Grant`, the value `nml check` composes
  under — no editor-side copy of it) and
  `DiagnosticConfig.grant` hands it to `compose_file` — the editor no
  longer composes every file under `OpenContext`, so NML2064/NML2065
  are the CLI's in the editor too; a document path is canonicalized
  ABOVE its workspace root only — at every site: the resolution, the
  config lookup, the name on its findings (the kernel's key, carried on
  `packages::Resolved::key`) and the `.model.nml` pass (an author's link
  inside the root reaches the kernel as a link: NML2083, as `nml check`
  says, and is named as the link, never as its target); a workspace
  root the editor cannot read is no universe (nothing binds, nothing is
  indexed) rather than a scripted one;
  `WorkspaceView` carries an `OpenDocuments` store (text and a
  per-write stamp) instead of a text closure, `PackageResolver::index`
  is the editor's index, `diagnostics::compute`/`compute_parsed` take
  the buffer's name, and `MAX_DIR_DEPTH`/`MAX_FILE_COUNT`/`packages::
  MAX_DEPTH`/`MAX_ENTRIES` are gone from `nml limits` (`MAX_INDEX_BYTES`
  replaces them); `PackageError::Manifest` gains `at: Option<
  ManifestLocation>` (the first finding's line and column, and the
  manifest's file once `PackageError::in_file` names it);
  `ManifestClaim` carries ONE `origin: ClaimOrigin` (`Workspace {
  manifest, anchor }`, minted by the walk alone, or `External { class:
  ExternalClass, markers }`) in place of its `class`, `manifest` and
  `anchor` fields — three spellings of one fact that had to agree — so
  a workspace claim without its key is unrepresentable (the editor's
  fail-closed arm for it is gone); `class()` and `manifest()` are
  methods and `Anchor` is gone; `discover` takes `Vec<ExternalClaim>`
  (`ExternalClaim::new(package, ExternalClass)` — `Injected`, `Store`,
  `Builtin`) and MINTS every claim itself: `ManifestClaim` has no public
  constructor and no public field but `package` (`origin()`,
  `content_hash()`, `manifest_label()` are methods), so a claim the walk
  did not settle is unrepresentable; `HiddenAudit` is `{ nml, examples, incomplete }` (an
  exact count and at most `MAX_AUDIT_EXAMPLES` keys by depth then name,
  never every key) and `audit_hidden` lists breadth-first;
  `workspace::{human_bytes, too_large}` are the one cap sentence and
  `workspace::{read_beneath, read_leaf, read_input}` (under
  `workspace/fs/`; the input read in `discover`) the one reader every
  front end reads through, `workspace::ReadError` its typed refusal; `nml-lsp`'s `DegradedNote.span`
  is `anchor: NoteAnchor` (`Top`, `Declaration`, `At(span)`) and its
  `Resolution` gains `Refused` (a walk that did not finish: nothing
  validates); the kernel's own helpers are crate-private —
  `SourceKey::{dir_contains, dir_is_strict_ancestor_of, child_dir}`,
  `Grant::of`, `Discovery::{configs, load_errors}`, `Universe::configs`,
  `WasiFs.list` and `MockFs`'s fields — every reader outside the kernel
  goes through `Universe::nearest_config`, `Discovery::universe_errors`,
  `Resolved::grant`/`Grant::unbound` and `wasi_fs_through`.
- **NML0022 — source too large.** A single source past the parser's
  4 GiB bound (token positions are 32-bit, as in the syntax tree)
  yields an empty tree and this one typed finding instead of a panic;
  the CLI's per-input caps refuse long before it (RFC 0019 item 0).
- **Did-you-mean budget for unresolved references.** A file with more
  than 128 unresolved references gets suggestions on the first 128
  findings only; the rest are reported without one.
- `Duration::total_nanos()` is derived from the segments on each read
  rather than cached — the type is 104 bytes instead of 128, still
  `Copy`, every constructor and accessor unchanged.
- **Source-character policy closed over its classes (NML0017/NML0018)**
  — a breaking tightening, landed pre-release. NML0017 now rejects
  every Unicode control character raw (general category Cc: C0, DEL,
  and the previously unpoliced C1 range — including the one-byte CSI
  terminal-escape introducer and NEL); NML0018 adds the U+2028/U+2029
  line/paragraph separators, so every Unicode line-boundary character
  outside LF/CRLF is now diagnosed. The banned set is deliberately a
  strict superset of rustc's (rustc reads LS/PS as whitespace and
  allows raw C1 in literals): NML values are echoed into terminals,
  logs, and generated configs. The implicit bidi marks (LRM/RLM/ALM)
  stay legal — ordinary RTL content. Raw NEL/LS/PS previously parsed
  clean inside strings and comments; the escapes (`\u{85}`,
  `\u{2028}`, `\u{2029}`) and `\n` are the spellings, and the error's
  hint teaches both. `nml fmt` previously re-rendered those escapes as
  RAW bytes (so a formatted file could newly fail) — it now emits the
  escapes, restoring "formatted output never carries a character the
  parser rejects" as a tested invariant. The Unicode tag block
  U+E0000–U+E007F (128 code points — a deprecated invisible mirror of
  ASCII) joins the NML0018 set: a raw tag sequence hides an ASCII
  payload inside what displays as ordinary text, and previously drew
  zero diagnostics anywhere; emoji tag sequences are content and are
  written with escapes (`\u{1F3F4}\u{E0067}…`).

- **Quoted-literal migration teaching extended (NML0001)** — quoted
  numbers (`port = "3000"`) and quoted bools (`admin = "true"`) against
  their typed fields now get the same replaced-syntax teaching error the
  quoted-duration migration ships: the machine-applicable de-quote fix,
  instead of the generic NML2008 mismatch. Fires only when the de-quoted
  text parses as the field's type; `$ENV.*` references are untouched.

- **Docs harness `expect-error` counts findings (multiset)** — the
  bracketed code contract upgraded from set to MULTISET equality:
  repetition is the count syntax (`[NML2057, NML2057]` = exactly two
  findings). The flip immediately surfaced 13 error-index fences whose
  samples demonstrated two same-code findings while declaring one — the
  drift class set-equality structurally hid; all are now declared true.

- **Bare faceted-domain fields carry `PrimitiveFacets::None`** — a bare
  `number`/`duration` field now serializes and matches exactly like a
  bare `string` field ("no facet list authored"); the domain is the type
  name's job. Audited safe in advance: package identity hashes source
  bytes only, extraction is never persisted or wire-crossed, and every
  behavioral consumer routes through `is_none()`. Representation pinned
  by a wire-shape test plus a `None ⟺ no authored list` invariant test.

- **LSP claims invalidation tightened** — watched-file CHANGED events no
  longer clear the package-claims memo (claim verdicts are functions of
  file existence, names, and manifest-hashed globs — never content), and
  invalidation is root-scoped to the changed paths.

- **Inline arm targets (RFC 0007 §6.2)** — `@selector -> Name:` + indented body is a
  first-class arm RHS form, fully validated against `V`, recursed by identity/
  defaults/diff, with LSP descent, document symbols, inline-target completion
  snippets, string/enum arm-selector completion, and a parse-time diagnostic for
  the `-> "name":` mistake.

- **Compound duration literals (RFC 0017 amendment)** — durations now
  support attached compound syntax (`1h30m`, `5m2s`) and inline-spaced
  forms (`1h 30m`). The lexer suffix-run rule tokenizes glued compounds
  as alternating `Number`/`Ident` pairs; duplicate units in source
  diagnose as `NML3007` with a merge fix; dangling magnitudes are
  `NML3008`; the `NML3005` fractional fix now respells at the authored
  granularity (`1.5h` → `1h30m`, not `90m`). Coercion text (`$ENV`)
  silently merges duplicates and decomposes exact fractions. Wire JSON
  now carries a `segments` array (breaking, pre-1.0).

- **VS Code extension tooling** — migrated from npm to a pnpm 11 workspace
  (root `pnpm-lock.yaml`, supply-chain policy in `pnpm-workspace.yaml`).
  Contributors: `corepack enable && pnpm install`, then `just verify-ext`.
  Toolchain gate: `pnpm run check:toolchain` (VS Code API floor, lockfile
  alignment, Node 22+, Corepack-pinned pnpm).

- **Duration facets (RFC 0018 §3 deferral closed)** — `duration` is now
  a legal facet carrier beside `number`: `timeout duration(min = 1s,
  max = 2h, multipleOf = 250ms)`. One generic engine (`Facets<T:
  FacetDomain>`; `NumberFacets`/`DurationFacets` are aliases, and
  `FieldType::Primitive` now carries domain-tagged `PrimitiveFacets`)
  keeps the two families byte-identical in behavior: bounds compare
  semantically (`min = 1000ms` ≡ `min = 1s`; nanos-exact, never
  floats), `multipleOf` is unit-blind nanos divisibility, and defaults,
  config values, unions, and collections all face the same walk on
  every surface (generated default/value parity grid extended over the
  duration rows). Declaration rules (NML2058): bounds must be duration
  LITERALS (`min = 5s` — a unitless bound teaches the shape), a
  duration bound on a `number` field errors symmetrically, ranges are
  judged semantically across units, and `multipleOf = 0s` is rejected
  like its numeric zero sibling.

- **The editor calls the loader** — `.model.nml` buffers now get
  cross-definition schema validation in the editor: unknown/wrong-kind
  `is` targets, inheritance and reference cycles, post-inheritance
  shorthand arity, oneof/enum integrity, reserved type-constructor
  names, cross-source duplicates, and declared-default checks, all by
  running `nml_validate::loader::load_schema` — the same entry every
  CLI verb and embedder uses — over the buffer's covering package
  (`[]schema`, manifest order, buffer-first reads) or its
  directory-mates when uncovered, keeping only the buffer's own
  source-stamped findings. CLI/editor parity is by identity, not by
  test; `nml-lsp`'s separate buffer-scoped default check is deleted as
  redundant. The workspace index walk now also skips `target/`
  (`cargo package` copies crate sources there, `.nml` fixtures
  included).

- **Durations are literals (RFC 0017)** — `30s` parses to a typed
  `Value::Duration` (magnitude + unit, faithful storage, **semantic**
  equality: `30s == 30000ms`, so sets and reload diffs treat the two
  spellings as one value). Units are `h`/`m`/`s`/`ms`/`us`/`ns` — the
  **complete** ladder, closed by construction (`ns` is the value
  domain's own resolution floor; `h` the largest exact unit; calendar
  units are permanently excluded), so `DurationUnit` is exhaustively
  matchable forever. Unsigned integer
  magnitudes only, domain-bounded at decode (`NML3004` unknown unit,
  `NML3005` fractional magnitude, `NML3006` out of domain — all with
  machine-applicable fixes where one exists); `NML2029` is retired to a
  tombstone. A duration-typed field deserializes to
  `std::time::Duration` from every provenance: literals directly, and
  `$ENV`-resolved strings through `coerce_to_duration`, joining the
  existing `de` coercion family (never-echo rule inherited). The quoted
  spelling `"30s"` in a duration-typed field is an `NML0001` migration
  with a mechanical fix, applied in bulk by the new `nml fix`.

  *This reverses the earlier in-cycle removal of `Value::Duration`.* The
  removal's premise — no code path could produce the variant — was
  correct while durations had no grammar; this RFC adds the grammar and
  removes the premise. `Value::Path` **stays removed**: a path's
  meaningful typing is safety (containment, traversal), none of it
  knowable at parse time — that belongs in the consumer's newtype.
- **`Value::Path` removed** — the variant was unreachable by
  construction: paths are *quoted* strings in the grammar with no lexeme
  of their own, so only schema validation can judge path-ness, and no
  code path ever produced the variant (its `PrimitiveType` doc already
  said "treated as a string at runtime" — path-typed validation never
  even accepted `Value::Path`). Path-typed fields keep validating
  exactly as before; consumers keep receiving `Value::String`. Dead
  match arms across the workspace went with it.
- **Oneof arm values report every bad escape** — bare string-literal
  positions (oneof discriminator/arm values) now use the total decoder:
  all escape errors surface at once (rustc-style, matching property
  values) and the U+FFFD-recovered text is kept, so lenient surfaces
  retain the arm's identity instead of an empty string.

- **List validation: spelling parity & the item-shape matrix** — the two
  spellings of a list field (`f = [v]` and `f:` + `- v`) now validate
  identically, by construction and by CI-enforced test. — one contract,
  *raw is transport, escaped is content*, enforced end to end:
  - New diagnostics: bare CR (NML0016; inside a string the machine
    fix is the `\r` escape), raw
    C0/DEL control characters (NML0017), bidirectional controls and
    interior U+FEFF — the Trojan Source defense, rustc's banned set
    (NML0018). A leading U+FEFF is accepted as a BOM.
  - New escapes: `\r`, `\s` (protected space, as in Java text blocks),
    and `\u{…}` (1–6 hex digits, as in Rust/Swift) — the sanctioned
    spelling for every character the policy bans raw. `\u{7B}\u{7B}`
    writes a literal `{{` (template detection reads raw text, so escapes
    can never smuggle a template expression).
  - Multi-line strings adopt the Java text-block order: dedent is
    computed on source lines *before* escapes are interpreted, so
    escaped newlines/whitespace are content, never indentation. Content
    must begin on the line after the opening `"""` (NML0019), an
    own-line closing `"""` must align with the content (NML0020,
    machine-fixable — and gated on termination: an unterminated
    string's trailing blank line is a recovery artifact, not the
    closing quotes, so it reports only the missing delimiter), tabs
    may not appear in body indentation
    (NML0005 extended), and `\` before a line break is Java/Swift line
    continuation. Line endings are transport: LF and CRLF documents are
    byte-identical in value (fuzz-verified).
  - Value decoding is now *total*: every malformed escape in a string is
    reported at once (rustc-style) with U+FFFD recovery, via the new
    `decode_value_all` / `ValueErrors` API; the strict single-error API
    is unchanged.
  - The formatter can never emit a document the parser rejects: edge
    spaces render as `\s`, `"""` runs and template braces are broken by
    escaping, tabs render as `\t`.
  - A fallback chain in a list position gets a teaching diagnostic
    (NML0021) instead of a misleading pipe-modifier parse error — with
    recovery, in both list spellings. Chains stay property-position;
    the `const`-name idiom (`const K = $ENV.A | $ENV.B`, then
    `keys = [K]`) is the supported way to use one as an element.

- **Numbers are exact decimals (RFC 0016)** — `Number { Int(i64),
  Float(f64) }` is replaced by a single exact decimal (34 significant
  digits, the finite IEEE 754-2019 decimal128 value space): `taxRate =
  0.20` now stores exactly 0.20, written scale survives formatting and
  serialization (`2.50` stays `2.50`), and integers parse to 34 digits
  (previously hard-capped at `i64`). Anything outside the exact domain is
  a parse error — NML never rounds. Breaking API changes: `Number` is a
  struct (gains `Eq`/`Ord`/`Hash`, the `num!(…)` const literal macro,
  `total_cmp`, `to_i128`/`to_u128`, `try_from_f64`); `as_i64`/`as_f64`
  are now `to_i64`/`to_f64` on `Number`, `Value`, and `ValueQuery`;
  `From<f64>` and `PartialEq<f64>` are removed (use `num!`);
  `deserialize_i128`/`u128` are supported and `u64 > i64::MAX`
  deserializes exactly; `Serialize` never emits binary floats (fraction
  forms serialize as exact strings); env-string coercion accepts
  scientific notation exactly and rejects `inf`/`nan`; f32 fields are
  single-rounded (the old path double-rounded). `NML0014` generalizes
  from "integer out of `i64`" to "number outside the exact decimal
  domain" with a structured payload; trailing-dot literals (`1299.`) are
  now `NML0013` with a machine-applicable fix. Money parses through the
  same decimal core (`Money::to_number()` is new; exactly-`i64::MIN`
  minor units are now accepted; >`i64` amounts error as `NML3003` rather
  than `NML3000`). The typed-error integer extraction is now the single
  wide rung `TryFrom<&Value> for i128` (replacing the `i64` impl):
  every integer target through `u64` narrows exactly from it via
  `T::try_from` (a `u128` consumer uses `Number::to_u128`), so
  nothing funnels through `i64` and falsely rejects the
  `(i64::MAX, u64::MAX]` band; `Value::to_u64` covers the probe flavor.
  Programmatic `Number::try_new` rejections carry their own raw-pair
  kinds (`CoefficientTooWide`/`ScaleOutOfRange`/`NegativeScaleZero` —
  one per invariant, never diagnostics); `Malformed` is grammar-only.

- **Rust edition 2021 → 2024** across the workspace (one inherited line; the
  MSRV stays 1.86, which predates the flip and supports edition 2024). The
  audited migration surface was 12 sites, all rowan node-handle temporaries
  in `cst/` whose 2024 drop/scoping changes are semantically inert — each
  verified individually, and the natural `if let` forms kept over the
  migration tool's defensive rewrites. Code delta: one `use<>` capture
  bound in a test helper. Formatting adopts the 2024 style edition
  (mechanical sweep, separate from the semantic change). Downstream crates
  on any edition are unaffected.

- **`nml_core::de::from_block` is now `from_body`** — it always took a
  `&Body`, and every other deserialization entry point is named for its
  input type (`from_value`, `from_body_resolved`, the `defaults` family);
  the naming rule is now stated normatively in the `de` module docs.
  Pre-publish rename, no deprecation alias. Also: `nml_core::File` is
  re-exported on the crate facade beside `parse` (its return type), and
  `nml-validate` re-exports the core facade essentials (`parse`, `File`,
  `Document`, `ValueResolver`, `Diagnostic`, `Severity`, `SchemaIndex`,
  the defaults family) so the common parse → validate → defaults →
  deserialize flow is a single dependency.

- **Declared defaults are checked where the schema is loaded** — a
  field's `= default` is now type-checked by `load_schema` (and so by
  both CLI verbs, schema packages, and any embedder), instead of only
  by surfaces that happened to validate a file containing the
  definition. A type-wrong default in a `--schema` directory used to
  load clean and materialize into runtime config; it is now an error at
  load. **API change**: `SchemaValidator::validate()` no longer reports
  default diagnostics — call `nml_validate::schema::default_diagnostics`
  (or just use `load_schema`, which does). The check runs *before*
  inheritance resolution, so one declared default yields exactly one
  finding however many models inherit it.

</details>

<details>
<summary>The 0.1.0 feature list as first written — the language, the library, the CLI and the docs</summary>

These three groups predate the ones at the top of this version and nothing
above replaces them: they are what 0.1.0 adds to an empty tree. Release notes
for 0.1.0 are both strata — the groups at the top say what the review rounds
changed, these say what the release IS.

### Added

- **Numeric schema facets (RFC 0018)** — `number` fields constrain
  their value range first-class in the type:
  `port number(min = 1, max = 65535)`, with `exclusiveMin`/
  `exclusiveMax` and `multipleOf`. Enforcement is exact through the
  RFC 0016 decimal core — boundary comparisons cannot lie and
  `multipleOf` is exact decimal divisibility (`0.3` IS a multiple of
  `0.1`; float-based validators famously disagree), via the new
  `Number::is_multiple_of` predicate (modular arithmetic, no bignum,
  no rounding). Facets apply element-wise to `[]number`/`set<number>`,
  hold field defaults to the same rule, render canonically in fmt and
  hover, and come with two diagnostics: `NML2057` (value violates a
  facet) and `NML2058` (invalid facet declaration). Schema packages
  need no format change — packages carry schema source, and a
  pre-facet parser rejects the new syntax loudly rather than silently
  under-validating. Wire-shape note for external `nml parse` JSON
  consumers: `Named` and `Primitive` are struct variants now
  (`{"Primitive": {"ty": "Number", "facets": …}}`).

- **`nml fix` — the batch fixer** (RFC 0017 §4.1): applies
  machine-applicable suggestions in bulk (`nml fix [--schema <dir>]
  [--dry-run] <path>...`, directories walked for `.nml`). A suggestion is
  applied only when it is the **sole candidate for its span** (so RFC
  0015's N-mutually-exclusive-fixes rule holds by construction), edits
  splice highest-offset-first via the new span-splice primitive
  (`cst::edit::splice`), and every round is re-checked before acceptance
  — a round that does not strictly improve the file is discarded, and
  writes are atomic. This is the missing half of the stability policy's
  "breaking changes ship with fixers" commitment: the `=>` → `->` and
  quoted-duration migrations are now bulk-appliable (and the repo's own
  examples were migrated with it).

- **The proof surface**: a [case study](docs/case-study.md) describing how
  a production workflow platform embeds NML end to end (schema packages +
  embedded `<tool> lsp`, diff-driven `#live`/`#restart` reload, config as
  a security surface — attribution generic until the platform's launch),
  and a [footprint page](docs/footprint.md) with measured, reproducible
  numbers: 19 packages for core parse+serde embedding (no async stack),
  ~0.75 MiB release example binary, corpus parse throughput via a
  committed measurement example (`measure_parse`).

- **The cookbook** (`docs/guides/`): fourteen task-oriented recipes for
  embedding NML as a library — parse/query, serde deserialization, custom
  secret resolvers, schema defaults, CI validation, semantic diff +
  `#live`/`#restart` classification, format-preserving edits, collect-all
  errors, directive vocabularies, schema packages & the store, embedding
  the language server, idempotent formatting, schema testing, and a
  migrate-from-TOML guide whose side-by-side claims are *executed*: the
  same config is parsed from TOML and NML into one struct and asserted
  equal in CI. Every recipe is a compiled example (or test) in the new
  `nml-cookbook` workspace crate; the docs harness runs all of them and
  verbatim-syncs every page listing against the compiled source.

- **MSRV: measured, declared, and enforced — Rust 1.86.** The floor was
  established by an audited bisect (every build target, the
  `wasm32-wasip1` server, and doctests, each candidate resolved with the
  MSRV-aware resolver): nml's own code compiles below 1.86, but the
  `icu_*` dependency family (via `url`/`idna` under `tower-lsp`) sets
  1.86 as the honest floor of the resolvable dependency set. CI now
  builds on exactly that toolchain across Linux/macOS/Windows (the job
  reads the version from `Cargo.toml` — one line to bump, and bumps are
  minor-version events per `docs/stability.md`), `Cargo.lock` is now
  committed (current cargo-team guidance; CI runs `--locked`, so a
  dependency release can never break `main` silently), and three more
  gates joined the reusable pipeline: rustdoc with warnings denied,
  `cargo package` publish-readiness for every publishable crate, and a
  nightly-resolver minimal-versions check proving the declared dependency
  bounds are real. Running the supply-chain gate locally also surfaced a
  latent failure: the unpublished tutorial apps tripped cargo-deny's
  license check — now exempted via `private = { ignore = true }`
  (licensing applies to what ships). Locally: `just msrv`.

- **Full error explanations in-editor (RFC 0010 tier 2)**: every coded
  diagnostic offers an **Explain NML0000** code action that renders the
  complete error-index entry — meaning, runnable examples, the fix — as
  a markdown preview beside the code, and the **NML: Explain a
  Diagnostic Code** palette command opens the same entries for codes you
  can't hover (CI output, a teammate's log), listing all of them
  searchable by code or summary. One composer
  (`nml_core::diagnostic::explain_document`) shapes the entry for both
  the editor and `nml explain`; explanations always come from the exact
  server binary that produced the diagnostic, so error and explanation
  can never version-skew. The action is negotiation-gated
  (`initializationOptions.explainCommand`) — LSP clients that registered
  no command never receive an unexecutable action; the content rides two
  new custom methods, `nml/explain` and `nml/explainIndex`. Also:
  `nml explain --list` (every code with its summary, grep-able), the
  index's own relative links repaired after its move to
  `crates/nml-core/assets/` (now guarded by a docs-test link resolver),
  and a tripwire guaranteeing no fenced line can truncate an index
  section.

- **In-editor error explanations (RFC 0010 tier 1)**: hovering a
  diagnostic shows its error-index **summary** — the meaning paragraph,
  never the examples — after any regular hover content (or alone, on the
  diagnostic's range), with a pointer to `nml explain` for the full
  entry. One new primitive, `nml_core::diagnostic::explain_summary`,
  derives the summary from the same embedded index as `explain`
  (relative links stripped so nothing dangles in hover context); all 82
  documented codes are covered on day one. Behind it, the language
  server gains a per-document **diagnostics cache** validated against
  the exact buffer text it was computed from (an in-flight compute that
  races an edit can never serve stale ranges), invalidated by document
  edits, schema-registry rebuilds, and project-config changes — which
  also makes the document-pull's *Unchanged* path recompute-free.

- **The parse-error taxonomy is closed (RFC 0009)**: every syntax
  diagnostic derives its message, stable code, and any machine-applicable
  fix from a payload-carrying kind — the transitional prose carrier and
  the `Lex`/`Parse` variant split are deleted, and `NmlError` is
  `Syntax` + `Money` with no `String` field anywhere. New stable codes
  `NML0002`–`NML0015` (unexpected-token with expected/found/context,
  unterminated string, unexpected character, tab-in-indent, the
  offside-rule dedent — which lists the open columns — nesting limits,
  set separator, reserved `map`, unknown type constructor, duplicate
  directive, string escapes, invalid/out-of-range numbers, `$NS.key`
  references) and `NML3002`/`NML3003` (money precision / out-of-range;
  `NML3000` narrows to malformed amounts) — every one with a CI-verified
  index example.
- **Parser errors carry token-width spans** (editor squiggles cover the
  offending token, not a caret between characters); recovery cascades at
  one position coalesce into a single "expected X or Y" report; and the
  fixers engine grows `&&`→`&` and `set<a, b>`→`set<a | b>`, plus
  did-you-means for unknown type constructors and variable namespaces.
- **Diagnostics are honest and hardened**: truncation is never silent
  (every layer counts what it drops; one `info` line reports the exact
  suppressed total), every render path escapes control characters (file
  content cannot smuggle terminal escapes into CLI output or logs), and
  `Diagnostic` gains related information — `note:` lines in the CLI,
  spec-native `relatedInformation` in the editor — with unterminated
  strings pointing back at their opening delimiter as the first producer.

- **Total diagnostic code coverage**: every validator- and package-emitted
  finding now carries a stable code with a verified error-index entry —
  the Phase 4 sweep. New: `NML2036`–`NML2042` (arms and `oneof` instance
  rules: duplicate/unreachable arms, key/target mismatches,
  missing/invalid discriminators), `NML2043` union-list shorthand,
  `NML2044` validation-truncated advisory, `NML2045` role-written-as-string
  (now **machine-fixable** — the quick fix strips the quotes and adds
  `@`), `NML2046`–`NML2048` membership advisories, `NML2049` dropped item
  key and `NML2050` arm-shorthand mismatch (identity's materialization
  findings are now `Diagnostic`-native, each coded at its source — and
  with its last producer converted, the prose-carrying
  `NmlError::Validation` variant is deleted), and `NML4000` fully-shadowed
  package validator — the first code in the packages band. Six more
  type-mismatch sites joined `NML2008`.
- **Schema-driven block-keyword completion** (the keyword twin of RFC
  0003's field completion): the editor offers block keywords from the
  document's resolved schema context — its bound package (closed
  vocabulary, RFC 0012) or the scope registry plus the document's own
  definitions — concrete models and oneofs only, labeled with `schema`
  provenance. And **editor collision parity**: an open-mode document
  redefining a registry name gets the same `NML2009` the CLI reports
  (`.model.nml` registry sources are exempt from self-collision).
- `nml check --strict` with an empty schema universe is now a **usage
  error** ("--strict has nothing to enforce") instead of silently
  degrading to parse-only checking — the CI-points-at-the-wrong-path
  trap, closed.

- **Self-validating files — one namespace (RFC 0012)**: `nml check`
  composes one schema universe from the `--schema` directory *plus the
  checked file*, so `model cache:` above `cache Foo:` fully types `Foo`
  with no flags — in the CLI and in the editor's lenient mode alike.
  Splitting a file into a schema directory no longer changes semantics; a
  name declared in both places is `NML2009`, never a silent shadow.
  Package-bound validation is the deliberate opposite — a **closed
  vocabulary**: in-file definitions under a binding type nothing, cannot
  mint keywords past `--strict`, and draw `NML2026` (warning lenient,
  error strict) instead of today's silence. Closedness follows provenance
  (`SchemaPackage::validator`); there is no knob.
- **Document-scope deserialization (RFC 0013)**:
  `from_document_defaulted` materializes top-level array-declaration
  references (`endpoints = monitoredEndpoints`) — shared properties and
  items inlined — before the serde pipeline runs, so the modular layout
  and typed structs stop being a trade-off; `Document::array_body(name)`
  exposes the referenced declaration directly.
- **Shared properties and modifiers are validated** (previously invisible
  even under `--strict`): unknown `.prop` names get the unknown-property
  treatment with a did-you-mean at the `.prop` token (for `oneof` and
  union elements alike, flagged only when no variant defines the name —
  with the did-you-mean drawn across every variant's fields), shared
  values type-check against the element's field (union elements: against
  the defining variants, through the standard union value check), and a
  model that declares modifier fields (`|allow []role?`) becomes its
  blocks' modifier vocabulary — `|alow` is caught with a suggestion even
  with no project/package modifier list.
- New coded diagnostics with error-index entries: `NML2027` duplicate enum
  variant (both authored forms normalize), `NML2028` empty enum, `NML2029`
  invalid duration (the spec grammar, enforced by the same
  `nml_core::types::parse_duration` consumers use — `"30x"` no longer
  passes), `NML2030` duplicate set element (one emitter replaces three
  hand-written sites), `NML2031` non-arm entry in an arms body, `NML2032`
  no union variant matches. `EnumDef`/`OneOfDef`/`ModelDef` all carry
  their declaring source for `file:line:col` attribution.
- `PackageError` and `StoreError` implement `std::error::Error` — `?`
  works in embedders' `main`.
- **The definition verbs can never disagree**: `nml validate` runs the
  same definition-side body pass `nml check` runs
  (`SchemaValidator::validate_definitions` — one code path, so a future
  definition check can't split the verbs). Surfaced by review: schema
  *defaults* (`duration = "5x"`) and RFC 0007 §4.3 type-shape violations
  previously erred under `check` while `validate` said ok.
- More formerly-uncoded diagnostics now carry stable codes with verified
  index entries: `NML2033` type composition with no instance form (arm
  sets in impossible positions; multi-arm-set unions), `NML2034` field
  definition outside a model, `NML2035` routing arms inside a schema
  declaration.
- The formatter's blank-line policy for shared properties is
  context-independent (nested lists now preserve the separator line the
  array form always kept).
- `NML2025`: a mixin listed twice in one `is` clause warns (the merge is
  idempotent; transitive diamonds stay silent).
- **Shared properties now scope to every body** (spec §Shared Properties
  clarified): a nested list's `.prop` merges into that list's items at any
  depth — previously the authored value was silently dropped in favor of
  the schema default in the serde pipeline (data loss). Precedence is
  unchanged: schema default → shared property → the item's own entry.
- **Positional items receive schema defaults** through the serde pipeline:
  a bare `- "value"` item's materialized body now defaults and merges like
  a named item's.

- **Traits are real (RFC 0011)**: `trait name:` declares a non-instantiable
  mixin — same field syntax as a model (defaults, markers, modifiers,
  directives all travel), composed with `is` by models and other traits,
  and never a block keyword, field type, or `oneof` arm target. Previously
  `trait` parsed but was silently ignored by schema extraction. Five new
  stable codes with error-index entries: `NML2020` unknown `is` target
  (with a machine-applicable did-you-mean at the target token — `is`
  targets were previously *silently* unresolved), `NML2021` non-composable
  `is` target, `NML2022` trait as a field type, `NML2023` trait as a
  `oneof` arm, `NML2024` trait instantiation (an error even in lenient
  mode). Strict-mode unknown-keyword suggestions (CLI and editor — the
  same validator) never offer a trait; `trait` joins the language-keyword
  completions.
- `nml validate` and `nml check` now run the full schema-finder pipeline
  on files that declare models/traits/enums/oneofs (reserved and duplicate
  names, composition, oneof integrity, positional arity, cycles) —
  previously these surfaced only through `check --schema`. `check` is a
  strict superset of `validate`; each finding is reported exactly once
  (the in-file composition twin defers when a loader pass covers the same
  content, and a file inside the `--schema` directory is covered by the
  directory load). Warnings report without failing the file. A
  self-contained file's own declarations resolve `is` targets — never
  falsely flagged against a foreign schema set.
- **Definition-anchored schema findings are source-attributed**: the
  loader stamps every model/trait/oneof with its declaring file, and
  composition, oneof-integrity, positional-arity, and cycle findings now
  render `file:line:col` instead of a directory-prefixed raw byte span.

- **Unified diagnostics model** (`nml-core::diagnostic`, RFC 0008): one
  `Diagnostic` type — severity (now incl. `Info`), span, source, structured
  suggestion, and a **stable error code** (`NML0000`-style, never renumbered
  or reused once released) — shared by the parser's error list, symbols,
  the validator, the LSP, and the CLI. One renderer, one LSP converter
  (replacing three hand-rolled bridges); the CLI prints rustc-style
  `error[NML2000]:` prefixes.
- New did-you-mean hints from core diagnostics: unresolved references
  (`DefaultPrt` → `DefaultPort`), unknown currency codes (`USE` → `USD`,
  from the ISO 4217 table), and unknown template namespaces — each with a
  machine-applicable quick-fix span.
- **Structured parse errors (RFC 0009 foundations)**: syntax errors carry a
  payload kind from which message, code, and fix derive; **`NML0001`
  Replaced syntax** is live — writing `=>` now yields a machine-applicable
  `->` fix in the CLI and as an editor quick-fix, with the error index
  carrying the migration ledger.
- **`nml explain NML2007`**: the error index is embedded in the binary
  (canonical file: `crates/nml-core/assets/error-index.md`, exposed as
  `nml_core::diagnostic::explain`), so explanations work offline; the CLI
  prints a rustc-style "for more information" hint after coded diagnostics.
- The oneof umbrella code split by fix-pattern: `NML2012` is now
  arm-references-unknown-model only, with `NML2015`–`NML2019` covering
  duplicate discriminator, name collision, bad default, non-enum
  discriminator type, and non-exhaustive enum arms.
- **Error index** (`docs/errors/README.md`): every stable code has a
  documented section, most with CI-verified examples; a bidirectional
  docs-test guard means a new code cannot ship without its documentation.
- `nml check --strict`: unknown properties and unmodeled keywords become
  errors (CI posture).
- Schema diagnostics from `nml check --schema` now locate as
  `file:line:col` against the attributed schema source instead of printing
  raw byte spans.
- **Changed:** every public findings API now returns `Vec<Diagnostic>` —
  `cst::parse_to_ast_all`, `cst::extract_schema`, the `SymbolTable` finders,
  and the schema-integrity finders (`find_oneof_errors`,
  `find_shorthand_errors`, `find_model_cycles`, `find_extends_cycles`) —
  with codes assigned (NML2007–NML2014 incl. missing-required-field,
  type-mismatch, duplicate-definition, reserved-type-name, cycle classes)
  and severity carried at the source; `nml_validate::diagnostics`/`::suggest` moved to
  `nml_core::diagnostic`/`::suggest` (no re-export shims); the LSP now
  reports duplicate/unresolved findings at their true severity (error),
  matching the CLI.
- **Unified did-you-mean engine** (`nml-core::suggest`): one OSA
  (restricted Damerau-Levenshtein) metric behind every suggestion, so
  transpositions (`"wran"` → `"warn"`) are caught; case-insensitive exact
  match wins outright; deterministic tie-breaking. Replaces the two divergent
  per-site suggesters.
- Did-you-mean suggestions (with machine-applicable quick-fix spans) at
  previously uncovered sites: unknown properties, unknown modifiers, unknown
  `oneof` discriminator values, and strict-mode unknown block keywords.
- `Diagnostic::rendered_message()`: the hint is derived from the structured
  suggestion by one renderer shared by the CLI, `Display`, and the LSP —
  producers no longer bake hint prose into messages. The `nml` CLI now prints
  hints for all suggesting diagnostics.

- Core parser with indentation-aware lexer
- AST types for all NML constructs (blocks, arrays, properties, modifiers, shared properties)
- Money type with ISO 4217 currency table and minor-unit storage
- Reference resolver with duplicate detection
- CLI with `parse`, `validate`, `fmt`, and `check` subcommands
- Canonical formatter with round-trip fidelity and idempotency
- LSP server with diagnostics, completion, hover, and go-to-definition
- VS Code extension with TextMate grammar for syntax highlighting
- Language specification (syntax, types, models, access control)
- Test fixtures for valid and invalid NML files

- **Template expressions**: `{{namespace.key}}` syntax in strings for dynamic interpolation
- **Fallback values**: `$ENV.KEY | "default"` pipe-chained fallback resolution
- **Template declarations**: `template Name:` for named string values
- **Const declarations**: `const Name = value` for file-level constants

- **Serde bridge** (`nml_core::de`): deserialize NML blocks into Rust structs
  - `from_block` -- deserialize a struct from an NML block body
  - `from_value` -- deserialize from a single NML value
  - `from_body_resolved` -- resolve + apply shared properties + deserialize pipeline
  - Recursive deserialization of nested blocks into nested structs
  - Named list item deserialization with automatic `name` field injection
  - Support for `Option<T>`, `Vec<T>`, enums, `camelCase` renaming

- **Value resolution** (`nml_core::resolve`):
  - `ValueResolver` with pluggable lookup (env vars or custom function)
  - Resolves `Value::Secret` (`$ENV.X`) and `Value::Fallback` chains
  - `resolve_body` / `resolve_array_body` for recursive resolution
  - `apply_shared_properties` -- merge `.key:` defaults into list items
  - `apply_array_shared_properties` -- same for array declarations

- **Query API** (`nml_core::query`):
  - `Document` wrapper with fluent block/property/nested lookups
  - `const_value`, `template_value`, `blocks`, `declarations` queries
  - `BlockQuery` and `ValueQuery` with `as_str`, `as_f64`, `as_bool` accessors

- **Value type conversions** (`nml_core::types`):
  - `TryFrom<&Value>` for `String`, `f64`, `i128`, `bool`, `Vec<String>`
  - Handles `Reference`, `RoleRef`, `Path`, `Duration`, `Secret` as string
  - `Value::as_str()`, `as_f64()`, `as_bool()`, `as_array()` accessors

- **Project configuration** (`nml-project.nml`):
  - `ProjectConfig` for schema files, template namespaces, modifiers, keywords
  - Auto-detected by LSP for workspace-aware validation

- **Model extraction** (`nml_core::model_extract`):
  - Extract model, enum, and trait definitions from parsed AST

- **LSP enhancements**:
  - Template expression validation (invalid namespace warnings)
  - Step reference validation in workflow files
  - Go-to-definition for keywords, references, and model fields

### Fixed

- **The VS Code status bar hid every refusal.** The extension's
  `nml/schemaInfo` contract accepted `warning` and `info` notes only, so
  an `error` note — an ambiguous claim, a manifest or project config that
  failed to load, a universe the walk could not finish, a derivation the
  kernel refused — dropped the whole notes list and the bar said "No
  schema package governs this file… commit a `<name>.package.nml`". A
  refused document now shows `nml: not validated` on the error
  background with the note's reason and remedy; a warning note, or a
  manifest above a derived root being ignored (`rootShadowed`), colours
  the item as a warning. The language guide's and integration guide's
  "Project Configuration" sections documented a `schema:` list no reader
  parses and claimed it chose model files; both now list the fields the
  resolver honours in both front ends and the editor-only tooling fields.

- **Comment runs parsed in quadratic time.** The syntax-tree builder
  rescanned the rest of a trivia run for every own-line comment (to ask
  whether a dedent follows) and released deferred comments from the
  front of a vector, so a run of N comments before any token cost
  N²/2 token visits plus N²/2 shifts: 40,000 comment lines took ~0.8 s
  to build and a 4 MiB comment-heavy file never finished. The dedent
  question is now asked once per run and deferred comments live in a
  deque of contiguous token ranges — the tree is byte-identical on
  every input (differential-checked across the repository's fixtures
  and fuzz artifacts), 40,000 lines build in ~1.5 ms, and the 4 MiB
  file parses in under 0.3 s. Every verb, the formatter, and the
  editor's re-parses share the path; three `perf_` tripwires pin the
  linear cost.
- **Parser memory ratio (RFC 0019 item 0).** `nml check` on a dense 10.5 MB file peaked at 1,637 MB of
  RSS (156 bytes per input byte); it now peaks at 830 MB (79), a 4 MiB
  file of tiny models 758 → 568 MB, a 10.5 MB file of small
  declarations 745 → 387 MB, and `nml parse` on the dense file 1,674 →
  826 MB — output byte-identical (CST-dump oracle 37,518 inputs, 0
  differing; `parse`/`fmt` 3,951/3,951; `check` ×3 modes 531/531).
  Four causes fixed: the lexer and parser held two 32-byte records per
  token (now 12 bytes each, text sliced from the source on demand);
  `check`, `validate`, `fix` and the editor's model-buffer passes
  parsed the same file TWICE — once for the AST, once inside the
  schema loader — holding the first AST across the second parse (each
  now parses a target exactly once and hands its extraction to the
  loader; `load_schema` also drops each source's tree before parsing
  the next); `Duration` cached a `u128` that made every `Value` 128
  bytes (now 104 / 112, API unchanged); and `nml parse` built its
  whole JSON document in memory before printing (now streamed).
- **`nml check` could run for minutes on a file with many unresolved
  references** — a did-you-mean was computed for every one against
  every name (N² edit distances; a 10 MB single-declaration file never
  finished). Suggestions are now budgeted to the first 128 findings of
  a file (every reference is still reported) and candidates outside
  the length window are skipped before the distance: 2 MB 20.9 s →
  0.67 s, 10 MB from "killed after 9 minutes" to 18 s, same findings.
- `nml check`/`nml validate` did not detect const/template reference cycles
  (the LSP did) — now reported as `NML1002`, caught by the error index's own
  verified example
- Money `format_display` now correctly handles negative fractional amounts
  (e.g. `-$0.50` was previously displayed as `$0.50`)
- Serde bridge now uses `format_display()` for money values instead of raw
  minor units (previously serialized `1999 USD` instead of `19.99 USD`)
- Money values can now be deserialized into `String` fields via serde

### Documentation

- Language guide with complete feature coverage
- Integration guide for using NML in Rust projects
- Formal language specification with PEG grammar
- Template expression, fallback value, and const declaration documentation
- README rewritten around a CI-verified 30-second demo, an honest comparison
  table ("when NOT to use NML" included), and a Rust embed quickstart
- Docs verification harness (`just docs-test` + CI job): tagged ```` ```nml ````
  blocks in the Markdown docs run through the real CLI, including asserted
  error examples, so documentation examples cannot rot
- Per-crate READMEs, CONTRIBUTING (docs-required gate), SECURITY policy,
  PR template, RFC status index, and a stability & compatibility policy
  (`docs/stability.md`); workspace `rust-version` declared
- **Nine-chapter tutorial** (`docs/tutorial/`): one growing status-page
  config from first parse through schemas, composition, `oneof`, access
  control, the Rust embedding pipeline, a diff-driven reload classifier,
  and shipped schema packages. Every chapter's finished config is a
  CI-validated fixture; chapters 7–9 are workspace crates the docs harness
  compiles **and runs**, asserting the output printed on the page; full
  Rust listings on pages are CI-checked to be verbatim excerpts of the
  compiled programs (```` ```rust source=<file> ```` guard)
- **Spec legacy purge + role-conjunction docs**: the never-implemented
  angle-bracket constraints, the parenthesized composition form, the
  `[]@roleRef` element type, and the `&`-reference marker are gone from
  the spec (`&` now means conjunction); role conjunctions are documented
  in the spec grammar (a `RoleExpr` production plus a normative
  `" & "` canonical-form clause), the access-control semantics, and the
  language guide. The docs harness gains banned-pattern tripwires so
  none of the dead forms can be re-taught, and the pre-commit hook now
  guards RFC-number uniqueness and index parity.

</details>
