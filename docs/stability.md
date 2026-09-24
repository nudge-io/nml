# Stability & Compatibility Policy

NML is **pre-1.0**. This page states exactly what that means, so you can
judge adoption risk on facts rather than version-number vibes.

## Versioning

- **0.x releases follow pre-1.0 semver:** breaking changes (language syntax,
  Rust API, CLI flags) may land in a **minor** release (`0.1 → 0.2`), never in
  a patch release. Every breaking change is called out in the
  [CHANGELOG](../CHANGELOG.md).
- **From 1.0**, the language grammar and the documented Rust API follow full
  semantic versioning.

## Breaking syntax changes ship with fixers

When the language changes shape, the tooling carries you across — this is a
commitment, demonstrated by the migrations already shipped (`=>` → `->`,
`&&` → `&`, quoted durations → duration literals):

- The old form is **rejected with a pointer**, not silently misparsed: writing
  `=>` in an arm today produces "`=>` was replaced by `->`" at the exact span.
- Where the rewrite is mechanical, `nml fix` applies it in bulk (whole
  trees, `--dry-run` for a preview) and the language server's quick-fixes
  apply it in place.
- Syntax changes go through an RFC (design records) with a required
  Documentation section — a change is not "Implemented" until its docs and
  migration story have landed.

## Minimum supported Rust version (MSRV)

- **The MSRV is 1.86**, declared as `rust-version` in
  [Cargo.toml](../Cargo.toml) — the single source of truth: the CI `msrv`
  job and the `just gate-msrv` recipe both read the number from that line.
- It is a **measured** floor, not a guess: the audited bisect (2026-07)
  found the workspace — every build target, the `wasm32-wasip1` server, and
  doctests — compiles on exactly 1.86, and that the floor is set by the
  `icu_*` dependency family (via `url`/`idna` under `tower-lsp`), not by
  nml's own code.
- It is **enforced**, not aspirational: CI builds every target on exactly
  this toolchain, on Linux/macOS/Windows, against the committed
  `Cargo.lock`. A dependency raising its own floor surfaces as a reviewed
  lockfile-update failure — never as a downstream user's broken build.
- **Raising the MSRV is a minor-version event**, never a patch, and always
  CHANGELOG-noted with the reason. It is never raised beyond what a change
  actually needs.

## What is and isn't a stable interface

| Surface | Stability |
|---|---|
| Language syntax & semantics | Versioned; breaking only in 0.x minors, with fixers |
| Documented Rust API (`nml-core`, `nml-validate`, `nml-fmt`, `nml-lsp`) | Versioned on two axes — `apiVersion` for a removal or a reshape, `revision` for an addition — recorded item by item in `docs/api/<crate>.api.txt` and named in the CHANGELOG (an item behind a cargo feature is in neither, and is not a stable interface): [the public Rust API record](#the-public-rust-api-record) below |
| CLI subcommands, flags, and **exit codes** | Versioned |
| Schema package format | Gated by an explicit `formatVersion`; readers reject versions they don't support. A new manifest key (`budgetUnits`; a binding's `layers:` grant) is an addition, not a bump: the vocabulary is closed, so an older reader refuses a manifest carrying it at load as an unknown property — precisely, never silently — and a bump would make every newer manifest unreadable, the ones that never use the key included; `formatVersion` moves for a change of syntax or of an existing key's meaning |
| Structured diagnostics (`Suggestion` spans/replacements, LSP wire shape) | Versioned |
| CLI `--json` stream (a `contract` row first, the same facts on the closing `summary`) | Versioned on two axes — `formatVersion` for a rename or removal, `revision` for an addition — both stated on the stream's first row; the consumer rule and every addition so far: [the `--json` stream](#the---json-stream) below |
| **Error codes** (`NML0000`) | Stable from the first published release: never renumbered, never reused; a retired code leaves a tombstone in the error index. Band grouping is allocation convenience, not API. |
| Diagnostic **message text** | Not a stable interface — do not parse it; use the structured fields (including `code`) |

## The `--json` stream

The CLI's `--json` output is versioned on two axes, both stated on the
stream's FIRST row — `{"type":"contract","formatVersion":1,"revision":3,"nmlVersion":"…"}`
— and again on its last (the closing `summary` row):

- **`formatVersion`** moves for a rename, a removal, a type change or a
  narrowed vocabulary — never for an addition.
- **`revision`** moves for every addition within one `formatVersion`: a
  new key, a new row type, a new enumerated value.

The consumer rule:

- Pin `formatVersion` — refuse a stream at one you do not know.
- Read `revision` from the header. Validate strictly (the published
  schema, every row closed) only against the schema at exactly that
  revision: the schema pins its revision as a `const` on the `contract`
  row, and the row is first, so a stricter consumer fails on the row
  whose meaning is the version, never on a finding.
- Otherwise MUST-IGNORE a key you do not know (it is an addition — the
  JSON norm: Protobuf's unknown fields, Kubernetes, JSON:API), keep a
  default arm on every enumerated value, and read a key you know that a
  stream lacks as absent (an older writer).

Revision 1 was the contract's first shape; the additions below belong
to it, and revisions 2 and 3 follow them. Each revision's additions are named
by one CHANGELOG entry (`--json formatVersion F, revision R`, the
latest first), and the docs gate holds the writer's number, the
schema's constant and those entries together — a schema change without
a bump, or a bump without one, fails the build. The contract is
published as a JSON Schema, `docs/json/nml-ndjson-v1.schema.json`
(every row closed — a consumer that validates learns of an addition
when it ships).

What `formatVersion: 1` states:

- A workspace file is named by its workspace-relative **key** on every
  `source` and `related[].source` field (the same spelling the `binding`
  row, the universe notes and the editor use), while the human
  `file:line:col` prefix keeps the path as typed.
- Every `col`/`endCol` is a 1-based BYTE column of its line (stated,
  not changed).
- The `root` object carries `fence` and `shadowed` (the entry above the
  fence that shadows a derived universe: another `.git`, or a manifest
  above a directory fence); the closing row carries `skipped`
  (`{byWhy, rows, shown, hidden}` — exact counts, rows under the
  `--max-findings` budget), with the `skipReason` values `unkeyableName`
  (the entry's name is no key, so its row carries `entry`, the name) and
  `componentBound` (a directory at the 64-component bound, never listed)
  — additions.
- `root.origin` no longer takes the value `derivedComponentCap` — that
  derivation is refused (exit 2) instead of narrowing the universe, so
  the value's disappearance is a behaviour change, not a wire removal.
- The `binding` row carries `absent` (an addition); `nml version --json`
  emits a `version` row (a new row type — an addition) and the closing
  row's `verb` vocabulary gains `version`.
- The gate's NML2090 rows are one per hidden directory rather than one
  per file — the diagnostic rows' granularity, not the wire's shape; a
  consumer counting rows counts directories.
- The hidden `compose-dump` verb and its `compose` row — a dev-time
  surface the guide marked hidden and no stability surface — are gone,
  and `verb` no longer takes the value `compose-dump` (a surface's
  removal, not a wire removal: no documented verb ever emitted it).
- The `diagnostic` row carries `suggestions` (`[{kind, source, edits[{line,
  col, endLine, endCol, lines}]}]` — the finding's machine-applicable
  edits, resolved against the files as the run read them, `source` the
  edited file's key, each `lines` element one line; an addition).
- The stream opens with the `contract` row (a new row type), the closing
  row carries `revision`, and the `fix` row and `fix`'s closing row
  carry `routed` (additions, revision 1).
- At **revision 2** the `diagnostic` row carries `cause` (`{code, source,
  line, col, message}` — the finding a wrapping row reports the refusal
  of: NML2088 over a manifest's first finding, NML2091 over a declared
  source's; `code` never null, the place in its own file, `line`/`col`
  null when it has none; absent from every other row, so
  cause?.code ?? code` is the code to act on; an addition).
- At **revision 3** the closing `summary` row carries `schemaSources`:
  how many schema sources a `--schema <dir>` invocation contributed,
  `null` when the run was given no `--schema`. `0` says the directory
  listed and held none, so every target was validated against its own
  definitions alone — the shape of a green gate over a validation that
  loaded nothing (an addition).

## The public Rust API record

The libraries have the same three instruments the `--json` stream has — a
record, a stamp, and a ledger — for the same reason, and you can read all
three without cloning anything.

- **The record** is `docs/api/<crate>.api.txt`: every public item of
  `nml-core`, `nml-validate`, `nml-fmt` and `nml-lsp`, one line each, as
  rustdoc sees it (so a re-export, a blanket impl or an inherited trait
  method appears the way the compiler resolves it, not the way a text scan
  would guess). Generated by `cargo public-api --simplified`, never edited
  by hand. **Diff two revisions of that file and you have the whole API
  change, exactly.**
- **The stamp** is the `# apiVersion A, revision R` line at the head of
  every record, and one number for the whole workspace — the crates
  version together and a consumer pins them together. `revision` + 1 for
  an ADDITION (a line appears); `apiVersion` + 1 for a BREAK (a line is
  removed or changed, or a constructible struct gains a private field).
- **The ledger** is one CHANGELOG entry per stamp — `**public API
  apiVersion A, revision R**` — saying what moved. A stamp with no entry,
  or an entry with no stamp, fails the build.
- **What the record does NOT carry** is an item behind a cargo feature.
  It is generated with no features on, so the `test-support` items of
  `nml-validate` and `nml-lsp` — helpers their own test crates drive —
  have no line, no stamp and no promise. Turning such a feature on in a
  production build takes you outside this contract deliberately.

The record is kept SMALL on purpose, and that is the other half of what it
promises: before a new item is recorded, its author measures who outside the
crate would reach it, and an item only the crate's own tests need goes behind
`test-support` instead. `nml-lsp`'s public surface is two entry points and
one result type for that reason — so expect a removal now and then of
something that was never meant for you, and read the record, not the module
list, for what you may rely on.

Why a record at all, when semver exists: these crates are consumed **by
path and by pinned rev**, never from crates.io, so cargo's own version
resolution protects nobody. A removed method or a reshaped variant lands
green and reaches the consumer days later as a compile error nothing
announced. The record makes it visible in the review that makes it.

What the stamp is NOT: a release version. Inside one unreleased `0.x`
cycle the stamp may move several times — each move is one reviewed step,
not one release — so the ledger reads as a work log and the *net* change
for an upgrading consumer is the diff of the record between the two
revisions they actually pin.

## Support window

Fixes land on `main` and ship in the next release; there are no long-term
support branches before 1.0. Security reports: see [SECURITY](../SECURITY.md).
