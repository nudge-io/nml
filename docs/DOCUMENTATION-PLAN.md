# NML Documentation & Adoption Plan

Status: **v3.1 — IN EXECUTION** (converged after three adversarial passes;
see the execution log below for what has landed)
Owner: docs

Execution log (2026-07-22):
- **Phase 0 — DONE** (all truth fixes; README item superseded by the full
  Phase 1 rewrite).
- **Phase A — local items DONE** (MSRV, per-crate READMEs, CONTRIBUTING with
  the docs-flow gate, SECURITY, PR template, RFC index, stability policy
  page). Open: the external-account steps (crates.io reserve/publish, VS Code
  Marketplace/OpenVSX, GitHub Discussions, CHANGELOG cut at publish).
- **Phase 5 harness — DONE early** (`scripts/docs_test.py`, `just docs-test`,
  CI docs job; opt-in `check` tags, flips to opt-out at the Phase 4 rewrite).
- **Phase 1 README — DONE** (verified 30-second demo, comparison table,
  when-not-to-use; by-example page still open).
- **Bonus (Phase 4 pulled forward):** unified did-you-mean engine
  (`nml-validate::suggest`, OSA metric, one renderer, uniform coverage) —
  the CLI now renders hints everywhere. Error *codes* + error index still open.
- **Phase 4 pulled forward (2): `spec/examples` repaired + `<shorthand>`
  retired — DONE.** The flagship example rewritten in current syntax
  (`is` composition, `[]role`, `path+`; instance file now validates against
  it in CI); all nine teaching sites now teach the `+` positional marker;
  the harness gained an example-files pass (`spec/examples/*.nml`) and a
  banned-legacy-token tripwire (`<shorthand>` everywhere on teaching
  surfaces, `=>` inside nml blocks; RFCs/plan exempt as historical record) —
  which caught the ninth teaching site the manual sweep missed.
- Publish prep partial: internal deps now carry `version` (publish dry-run
  empirically fails without it; nml-core dry-runs clean).
- **RFC 0008 EXECUTED (2026-07-22)** — unified core diagnostics with stable
  error codes: `nml_core::diagnostic` + `suggest` in core, validate's
  modules deleted (no shims), one `NmlError::to_diagnostic` bridge, one LSP
  converter, CLI `error[NML2000]:` prefixes, new hints (unresolved refs,
  currency codes, template namespaces). Gates: nml 902/0, nudge 3931/0 vs
  3930/0 baseline, clippy 0, docs-test 14/14+5/5. Phase 4's error-index
  pages + `codeDescription` links + code-coverage sweep now unblocked.
- Open next: tutorials (Phase 2), cookbook/rustdoc (Phase 3), remaining spec
  parity — PEG grammar regen, oneof/arms/set/directives sections, error
  codes (Phase 4), site (Phase 5), unified core diagnostics (approved
  design, needs dedicated turn incl. nudge import updates).
Goal: documentation good enough that a Rust developer who lands on the README
wants to embed NML in their project — and succeeds within 15 minutes.

Revision log:
- v3.1: added the docs-debt **flow** gate (features can't merge undocumented —
  closes the root cause the audit found; tripwires only guard enumerable
  surfaces). Final review pass; no other findings — next step is execution.
- v3: added launch & announcement phase (docs need an audience event — the
  plan previously ended at "site live"), cold-start usability testing as a
  pre-launch gate (human fresh-eyes + AI-agent-in-clean-environment doc QA),
  "when NOT to use NML" honesty section, troubleshooting/FAQ page, per-crate
  READMEs (all five currently point at the root README), GitHub Discussions
  as the help surface, error-code range allocation + stability rule,
  inverted-pyramid rule in STYLE.md, explicit deferral of non-Rust bindings.
- v2: added Phase A (distribution & adoption infrastructure — v1's biggest gap:
  docs can't convert if `cargo add` doesn't work), positioning brief, naming/SEO
  strategy (crates.io `nml` is taken; "NML" search collisions), diagnostics
  error index with stable codes + LSP linking, motion (CI-regenerated GIFs),
  migration guide, case study, footprint page, AI-assistant rules file,
  glossary/style guide as deliverables, generated reference tables, explicit
  cut list. Site recommendation revised mdBook → Starlight. By-example page
  promoted out of Phase 6.
- v1: initial audit + Diátaxis restructure.

---

## 1. Where we are (audit summary, 2026-07-22)

The existing docs are not so much *wrong* as **silently behind**. The language
and library kept moving (commits through today); the docs stopped at different
points in the past:

| Doc | Last touched | State |
|---|---|---|
| `README.md` | 2026-07-05 | Describes the v0.1 config language. Says "7 primitive types" (there are 9: `object` and `role` are missing). No mention of CST, diff, packages, LSP capabilities, arms, `set<T>`, directives. |
| `docs/language-guide.md` | 2026-07-05 | Widest coverage, reference-style. Missing: `set<T>`, directives, typed arms `(K -> V)`, positional marker `+`, `role` keyword, workflows. |
| `docs/integration.md` | 2026-03-16 | Four months behind the library. Missing `from_value`, `resolve_array_body`, `apply_array_shared_properties`, `template_value`, the defaults pipeline, diff engine, schema packages, CST editing. |
| `spec/*` | 2026-06-12 | Missing `oneof` **entirely**. PEG grammar omits constraints, `&T`, arms, `set<T>`, `->`, field markers. Keyword list at `spec/syntax.md:243` omits `oneof`/`role`. `spec/README.md` doesn't link the two user guides. |
| RFC headers | — | RFC 0004 says "Draft / P0 spike" but the CST **is** the production parser everywhere. RFC 0005's title says marker `!` but the shipped sigil is `+` (stale `!` references also linger in `identity.rs`/parser doc-comments). |

Pedagogical findings (the user-facing problem):

- **No tutorial exists.** Every doc is feature → syntax → table. There is no
  "build your first NML file," no incremental narrative, no capstone.
- **No entry path.** Nothing says "start here." `spec/README.md` indexes only
  the spec and never mentions the guides.
- **Examples are fragments.** Snippets reference undefined names and split
  model/instance pairs. The genuinely runnable material —
  `spec/examples/*.nml`, `tests/fixtures/valid/*`, `nml-cli/examples/*.rs` —
  is orphaned: no prose ever points a learner at it.
- **The best features are undocumented.** The things that differentiate NML
  from TOML/YAML/CUE/Pkl (semantic diff with reload classes, schema packages,
  machine-applicable fixes, LSP-grade tooling from a lossless CST) appear in
  zero documents.

Adoption-readiness findings (added in v2):

- **None of the crates are published.** `cargo add nml-core` does not work;
  every quickstart we could write today would be dishonest (git deps only).
- **The crate name `nml` is taken on crates.io** (a Fortran Namelist parser,
  v0.2.0, last updated 2024-02). `nml-core`, `nml-validate`, `nml-fmt`,
  `nml-lsp`, `nml-cli` appear free — reserve them early. The installed binary
  can still be `nml` (binary name ≠ crate name).
- **"NML" has heavy search collisions**: OpenTTD's NewGRF Meta Language (a
  GitHub repo literally named `nml`), the nML processor ADL, and the Fortran
  crate. "nml language" searches will lose without a deliberate SEO strategy
  (see §3.2). Also verify GitHub Linguist's existing claim on `.nml`.
- Cargo metadata is largely publish-ready (descriptions, keywords, categories,
  dual license, repository). Missing: `rust-version` (MSRV). Verify the
  `repository` URL is public before publishing.
- Two breaking syntax changes shipped recently (`=>`→`->`, `!`→`+`). Without a
  stated stability policy, evaluators will read this as churn risk.

## 2. Undocumented shipped capability (ground truth from code)

Everything below is implemented and tested today, and taught nowhere:

**Language**
- `set<T>` type (order-insensitive, duplicate = load error)
- Directives `#name` / `#name(arg)` — general, consumer-interpreted
  (e.g. nudge's `#live`/`#restart` reload classes, `#key` element identity)
- Typed arm fields `(K -> V)` incl. unions like `(string | (role -> denial))?`
- Positional/scalar-shorthand field marker `+` (`name type+`, `name type?+`)
- `role` primitive + `role` keyword; `object` primitive
- `oneof` discriminated unions (in the guide, absent from the spec)
- Workflows (`*.workflow.nml` convention; validated generically)

**Library (nml-core / nml-validate)**
- Lossless CST: resilient parsing (`parse_to_ast_all`, `parse_best_effort`),
  comment preservation, doc-comment extraction, **structural editing**
  (`cst::edit` — splice entries while preserving every other byte)
- Semantic diff engine (`diff.rs`): schema-driven, multi-file, `ChangeKind`
  (`Added/Removed/Modified/SetDelta/OpaqueChanged`), `Origin::{File,Default}`,
  structured `FieldPath`, secret-awareness, LCS list pairing, keyed identity
- Defaulting pipeline: `apply_shared_properties → apply_defaults →
  resolve_body → from_block` (`from_block_defaulted` etc.)
- `SchemaIndex` / `FieldTarget` field resolution; body-shape union selection
- Validation: strict vs lenient modes, arms/oneof/enum checks, Levenshtein
  did-you-mean with **machine-applicable `Suggestion`s**, `MembershipSemantics`
- **Schema packages (RFC 0030)**: `SchemaPackage`, `.package.nml` manifests,
  blake3 content addressing, per-user `Store` with publish/current slots,
  validator glob bindings, embedded meta-schema

**Tooling**
- LSP: schema-driven completion, hover, goto-def/references, rename,
  formatting + on-type, quick-fix code actions, pull diagnostics,
  `nml/schemaInfo`, store-backed schema resolution, **wasm32-wasip1 build**
- VS Code extension: WASM server delivery, `<tool> lsp` discovery ladder with
  trust gating, schema status-bar, getting-started walkthrough
- CLI: `parse`, `validate`, `fmt` (atomic write), `check --schema <dir>`
- Formatter: comment-preserving canonical `format_source`, idempotent

This list doubles as the acceptance checklist: **the overhaul is done when
every item above has a home in the docs.**

---

## 3. Strategy

### 3.1 Positioning brief (write first; every page inherits it)

One page, `docs/POSITIONING.md`, agreed before any prose is written:

- **Category sentence.** "A typed configuration language you embed in your
  Rust application — with a real schema system, first-class secrets and money,
  and editor-grade tooling you inherit for free."
- **The wedge.** The typed-config field is crowded (CUE, Pkl, Dhall, Nickel,
  KCL). NML's defensible difference is that it is **library-first for Rust
  hosts**: serde-native embedding, a semantic diff engine for live-reload
  classification, schema packages your tool ships to its users, and `<tool>
  lsp` — *your* CLI gets a full language server for its config files. No
  competitor has that last story. Lead with it everywhere.
- **Three pitch pillars** (recur in README, site landing, talks):
  1. *Typed for real* — models, unions, arms, money, secrets; errors with
     spans and machine-applicable fixes.
  2. *Embeds in minutes* — parse → resolve → defaults → your serde structs.
  3. *Your tool becomes a platform* — schema packages + embedded LSP + diff.
- **Terminology table** (tech-writing craft; drift here is how docs rot):
  pick one name per concept and enforce it — *model* (never "schema" for the
  declaration; "schema" = the extracted collection), *arm*, *positional
  marker* (the `+` sigil; retire "shorthand marker"), *directive*, *package* /
  *store* / *binding*, *oneof variant*, *discriminator*. Publish as a glossary
  page; the style guide (`docs/STYLE.md`) requires its use.

### 3.2 Naming & discoverability (SEO is a docs feature)

- Always title as **"NML configuration language"** — the bare word "NML" is
  contested (OpenTTD NML, nML ADL, Fortran namelist crate). Page titles,
  repo description, crate descriptions, and the site `<title>` all use the
  full phrase.
- Dedicated docs domain (e.g. `nml-lang.dev` or similar; decide once), GitHub
  topics, social cards. `llms.txt` + `llms-full.txt` build target so AI
  assistants answer NML questions from current syntax (see §4 Phase 6 — this
  matters double for us because pre-`->`/`+` snippets will otherwise dominate
  what models know).
- Investigate GitHub Linguist: `.nml` may already be associated with OpenTTD
  NML; registration affects README/code-block highlighting on GitHub.

### 3.3 Framework: Diátaxis, adapted

Four quadrants, three audiences:

| | Learning-oriented | Working-oriented |
|---|---|---|
| **Practical** | **Tutorials** (`docs/tutorial/`) | **How-to guides / cookbook** (`docs/guides/`) |
| **Theoretical** | **Explanation** (`docs/explanation/`) | **Reference** (`docs/reference/` + `spec/` + rustdoc) |

Audiences, in priority order — and the landing page routes by *intent*, not by
quadrant ("Embed NML in your app" / "Write NML files" / "Ship NML to your
users"):
1. **Rust developers embedding NML** — the "use as a library" goal. They need
   the integration tutorial, the cookbook, and first-class rustdoc.
2. **Config authors** writing `.nml` — need the language tutorial + reference.
3. **Toolsmiths/platform builders** — schema packages, directive vocabularies,
   `<tool> lsp` embedding. Nudge is the existence proof; docs make it repeatable.

### 3.4 The narrative spine: one example that grows

State-of-the-art tutorials (Rust Book, Vue tutorial, Pkl's tour) grow a single
artifact. Ours: **a deployable SaaS service config** that naturally exercises
every feature as it grows:

- ports/hosts → primitives, constants, `set<string>`
- API keys → `secret`, fallback chains, `ValueResolver`
- pricing tiers → `money`, arrays, shared properties
- email provider → `oneof` (`log | postmark`) with discriminator defaults
- admin surface → `|allow`/`|deny`, roles, parameterized roles
- routing/denial pages → arms `(role -> denial)`
- zero-downtime reload → directives `#live`/`#restart` + the diff engine

Seed material already exists: `tests/fixtures/valid/full-service.nml`,
`pricing.nml`, `spec/examples/*`. Reuse, don't invent.

### 3.5 Quality bar (non-negotiables)

1. **Every snippet is runnable and CI-verified** (see Phase 5). No fragments
   that reference undefined names.
2. Every tutorial page opens with "What you'll build / what you'll learn" and
   ends with a working artifact + a short recap + "where to next."
3. Every feature page shows the *error message you get when you do it wrong* —
   NML's diagnostics (spans, did-you-mean, quick fixes) are a selling point;
   show them off. Long-term, error output links to the error index (Phase 4).
4. Reference pages are **generated or guarded**: tables of primitives, type
   forms, CLI flags are emitted by a `just docs-gen` step from the code (single
   source of truth), with a CI check that the committed output is current.
   Grep tripwires only where generation isn't practical.
5. Docs use the same voice (second person, present tense, no future promises)
   and the §3.1 terminology table, codified in `docs/STYLE.md` — which also
   mandates the inverted pyramid (v3): every page's first paragraph answers
   "what is this and when do I need it" before any detail, so skimmers
   self-route instead of bouncing.
6. **Motion stays fresh or dies**: terminal demos are VHS-scripted
   (charmbracelet/vhs) and regenerated in CI, so GIFs can't drift from real
   CLI output. Editor GIFs (LSP completion, quick-fix) are re-recorded per
   release at most.

---

## 4. Workstreams

### Phase 0 — Truth reconciliation (fix wrong before writing new) — ~½ day

Small diffs, high embarrassment-avoidance value:

- [x] `README.md`: superseded — the full Phase 1 rewrite landed directly.
- [x] RFC 0004 header: Draft → Implemented ("CST is the production parse path
      for all crates").
- [x] RFC 0005: retitle to the shipped `+` sigil; sweep stale `!` mentions in
      `identity.rs` / `cst/parser.rs` doc-comments (+ fixture header).
- [x] `spec/syntax.md` keyword list: add `oneof`, `role`.
- [x] `spec/README.md`: link `docs/language-guide.md` + `docs/integration.md`.
- [x] `docs/integration.md`: add the four missing APIs (`from_value`,
      `resolve_array_body`, `apply_array_shared_properties`, `template_value`)
      as stopgap entries (+ fixed the stale `model_extract` example).
- [x] Add `rust-version` (MSRV) to the workspace manifest.

### Phase A — Distribution & adoption infrastructure — ~1–2 days (new in v2)

Docs cannot convert if the install step is fiction. Prerequisite for the
README rewrite's quickstart being honest:

- [ ] **Reserve + publish crates**: `nml-core`, `nml-validate`, `nml-fmt`,
      `nml-lsp`, `nml-cli` (0.1.0). `nml` itself is taken — the CLI installs
      via `cargo install nml-cli` but keeps binary name `nml`. Reserve names
      immediately even if 0.1.0 publishing waits.
- [ ] docs.rs builds green for all crates (`[package.metadata.docs.rs]` as
      needed); crate READMEs render correctly on crates.io.
- [x] **Stability & compatibility policy page** (`docs/stability.md`):
      pre-1.0 semver stance, MSRV policy, stable-vs-unstable interface table
      (diagnostic *text* is explicitly not an interface), and the
      trust-builder — *breaking syntax changes ship with fixers*.
- [ ] VS Code extension published to the Marketplace (+ OpenVSX for
      Cursor/VSCodium users — a meaningful slice of the 2026 audience).
- [ ] Binary releases: cargo-dist (or equivalent) GitHub releases for
      `nml`/`nml-lsp`; Homebrew tap optional later.
- [x] Repo hygiene evaluators check: CONTRIBUTING.md, SECURITY.md, PR
      template, RFC index page (`docs/rfcs/README.md`) with an accurate
      status table. (Dedicated issue templates incl. docs-feedback: still
      open.)
- [x] **Docs-debt flow gate** (v3.1 — root-cause fix): CONTRIBUTING + the PR
      template require every user-facing change to include its docs (or an
      explicit "no docs needed" justification), and the release checklist
      blocks a version cut while any shipped capability lacks a documented
      home. The 2026-07 audit exists because features shipped docs-less for
      months; tripwires and `docs-gen` guard enumerable surfaces (types,
      flags), but only a merge/release gate guards *new* capability. RFCs
      adopt "Documentation" as a required section — an RFC isn't
      Implemented until its docs are.
- [x] **Per-crate READMEs** (v3): all five crates now ship their own
      crates.io landing tailored to their job (nml-core pitches the embed
      story; nml-lsp pitches `<tool> lsp`).
- [ ] **Help surface** (v3): enable GitHub Discussions; every docs page footer
      links "Get help" → Discussions and "Report a docs bug" → issue template.
- [ ] Cut CHANGELOG 0.1.0; per-change entries from then on.

### Phase 1 — The front door: README rewrite — ~1 day

The README is the conversion surface. Structure:

1. **One-sentence positioning** (from §3.1) + a 3-line "why not YAML/TOML"
   paragraph.
2. **The 30-second demo** — one model + one instance + the *diagnostic* you
   get when you typo an enum value (showing the did-you-mean quick fix), as a
   CI-regenerated VHS GIF with a text fallback. This is the screenshot moment.
3. **Quick start** — `cargo add nml-core`, `nml check`, deserialize into a
   Rust struct in ~15 lines. Copy-paste runnable (Phase A makes this honest).
4. **Feature tour** — table with links, now including: lossless CST & resilient
   parsing, semantic config diff, schema packages, LSP/VS Code, `set<T>`,
   arms, directives, access control, money/secrets.
5. **"NML vs X"** — honest comparison vs TOML, YAML+JSON Schema, CUE, Pkl,
   Dhall (typed? schema-native? secrets? money? diff/reload? LSP? embed cost?).
   Include the row where competitors win (maturity, ecosystem) — credibility
   is the point of the table. Pair it with a short **"When NOT to use NML"**
   list (v3): flat key-value config with no schema needs → keep TOML;
   non-Rust host today; need a decade-stable format. The anti-pitch is what
   makes the pitch believable, and almost no config language writes one.
6. Docs map (tutorial / guides / reference / spec), project structure, license.

Also in this phase (cheap, high-traffic): **"NML by example"** — a single
skimmable Go-by-Example-style page generated from the verified example corpus.
Promoted from v1's Phase 6: by-example pages are consistently among the
most-visited pages of language docs and cost little once the harness exists.

### Phase 2 — Tutorial track (`docs/tutorial/`) — ~3–4 days

Progressive chapters; each ~10–15 min; the service config grows throughout:

| # | Chapter | Teaches |
|---|---|---|
| 01 | Your first NML file | files, declarations, properties, nesting, comments, `nml parse`/`fmt` |
| 02 | Types that mean something | 9 primitives, money, duration, secrets + fallbacks, templates, constants |
| 03 | Give it a schema | `model`, required-by-default, `?`, defaults, constraints, enums, `nml check --schema`, reading diagnostics |
| 04 | Compose and reuse | traits, inheritance, shared properties `.key`, lists, `set<T>`, positional marker `+` |
| 05 | One of many | `oneof`, discriminators, body-shape dispatch, arms `(K -> V)` |
| 06 | Lock it down | `\|allow`/`\|deny`, roles, parameterized roles, denial arms |
| 07 | **Embed it in Rust** | parse → resolver → defaults → serde structs; error handling; the full pipeline in ~40 lines |
| 08 | React to change | directives (`#live`, `#key`), `diff_config`, building a reload classifier (mini-nudge) |
| 09 | Ship schemas to your users | `.package.nml`, content hashing, the store, editor status bar, `<tool> lsp` |

Chapters 07–08 are the library conversion moment; 09 is the "platform" close.

Pedagogy mechanics (added in v2 — chapter list alone isn't a tutorial design):
- Each chapter: 1–2 **exercises with hidden solutions**, a **"common
  mistakes"** box built from real diagnostics, and a 3-bullet recap.
- **Show the chapter-to-chapter diff of the growing config** — rendered by
  NML's own diff engine. Dogfooding as pedagogy: learners see `Modified`/
  `SetDelta`/`Added` output every chapter, so Chapter 08 lands on familiar
  ground.
- Deliberate-error checkpoints: "break it like this, read the diagnostic,
  apply the quick fix" — teaches the tooling reflex, not just syntax.
- Each chapter's final state is a fixture in `docs/tutorial/examples/NN/`,
  verified by CI (Phase 5).

### Phase 3 — Library track: rustdoc + cookbook — ~3–4 days

The "want to use it as a library" ask lives or dies here.

**Rustdoc overhaul** (`cargo doc` must read like a product):
- [ ] Crate-level docs for `nml-core`, `nml-validate`, `nml-fmt`, `nml-lsp`
      with a worked, doc-tested example each (doc-tests run in `cargo test` —
      free CI verification).
- [ ] Module-level docs for the big subsystems: `cst` (incl. editing), `diff`,
      `de`/`defaults`, `resolve`, `schema_index`, `package`, `store`.
- [ ] `#[doc(alias)]`s for discoverability (`"toml"`, `"yaml"`, `"parse"`,
      `"deserialize"`); examples use `?`, never `unwrap`, in doc code.
- [ ] `#![doc = include_str!(...)]` so crates.io/docs.rs landing matches repo.

**Cookbook** (`docs/guides/`, one task per page):
1. Parse a file and read values with the query API
2. Deserialize into structs with serde (incl. named lists, label injection)
3. Wire a custom secret resolver (vault instead of env)
4. Apply schema defaults before deserializing
5. Validate in CI: strict mode, exit codes, `--schema`
6. Diff two configs and classify changes (live vs restart)
7. Programmatically edit a file without destroying formatting (CST edit)
8. Collect *all* parse errors for an editor-like experience
9. Define and enforce a directive vocabulary for your tool
10. Build, hash, and publish a schema package; consume from the store
11. Embed the LSP: give your CLI a `<tool> lsp` subcommand
12. Format user files idempotently (comment preservation, atomic writes)
13. **Migrate from TOML/YAML + serde in 30 minutes** (added in v2) — the
    single highest-intent adoption doc: side-by-side syntax cheatsheet,
    incremental adoption path ("gradually adoptable" is already a stated
    design principle — prove it), keeping old config parsing behind a flag.
14. Test your schemas and configs (`nml-validate`'s `test-support` feature).

**Proof surface** (added in v2):
- [ ] **Case study: "How a production workflow platform embeds NML"** — the
      nudge story (anonymized as needed): schema packages delivered to users,
      diff-driven live reload with `#live`/`#restart`, embedded `<tool> lsp`.
      Case studies are the strongest "want" signal a library can publish.
- [ ] **Footprint & performance page**: dependency tree honesty (nml-core is
      rowan + serde — no tokio; tokio lives in nml-lsp only), parse
      benchmarks on the fixture corpus, binary-size delta for embedding.
      Rare among config languages; evaluators love it.

### Phase 4 — Reference & spec parity — ~3 days

- [ ] Rewrite `docs/language-guide.md` as the **language reference**: complete
      per-feature pages, each with syntax, semantics, diagnostics, and a
      runnable example. Add the missing features (§2 list).
- [ ] **Diagnostics as product** (added in v2): assign stable error codes in
      `nml-validate`/`nml-core` (`NML0xxx`), build an **error index** (one
      page per code: what it means, why, fix, example), and set LSP
      `Diagnostic.codeDescription.href` to the docs URL so every squiggle in
      VS Code links to its explanation. Rust's error index proved this
      pattern; no config language does it — this is a genuine
      better-than-SOTA move and a small code change. (v3) Allocate code
      ranges by subsystem (parse/schema/resolve/package) so codes never
      renumber, and state the rule: once published, a code is never reused.
- [ ] **Troubleshooting / FAQ page** (v3): seeded from the real failure
      paths in the code — indentation mistakes, secrets failing because env
      resolution is disabled (`env_disabled_var`), strict-vs-lenient
      confusion ("why wasn't my typo caught?"), `=>` rejection, duplicate
      set elements. The support surface for questions the tutorial can't
      preempt; grows from Discussions threads after launch.
- [ ] New `docs/reference/cli.md` — all five subcommands, flags, exit codes
      (generated from `--help` output via `docs-gen`).
- [ ] New `docs/reference/editors.md` — VS Code setup, WASM vs native server,
      trust model for `<tool> lsp`, `nml.server.path`, status bar semantics.
- [ ] Glossary page from the §3.1 terminology table.
- [ ] Spec parity: add `oneof`, arms, `set<T>`, directives, field markers,
      `role`, reference types to `spec/`; **regenerate the PEG grammar** from
      the current parser; version the spec (v0.2) and state conformance =
      `tests/fixtures/` behavior.

### Phase 5 — Infrastructure: site + verified examples — ~2–3 days

- **Site: Astro Starlight** (revised from v1's mdBook). Rationale: Phase 6
  wants a wasm playground, editable snippets, and an interactive diff demo —
  all components, which Starlight/Astro hosts natively and mdBook fights;
  Starlight's SEO/search/landing components also serve §3.2, and content
  remains plain Markdown/MDX (low lock-in). Cost: a JS toolchain in a Rust
  project — isolate under `site/` with its own CI job. Reuse the TextMate
  grammar for `nml` highlighting (Shiki custom language).
- **Verified examples harness** (build FIRST, before tutorial prose):
  - fenced ```nml blocks carry a tiny annotation (`file=…`, `schema=…`,
    `expect-error=…`); a `just docs-test` script extracts them, assembles the
    per-chapter workspace, and runs `nml check --schema` — including asserting
    that *intentional* error examples produce the documented diagnostic;
  - Rust snippets are doc-tests or `docs/tutorial/examples/*/main.rs` compiled
    in CI;
  - `just docs-gen` regenerates reference tables (primitives, type forms, CLI
    help) from code; CI fails if committed output is stale;
  - VHS tapes for terminal GIFs regenerated in CI (§3.5.6);
  - link checker (`lychee`).
- **Feedback loop** (added in v2): privacy-light analytics on the docs site,
  per-page "Was this helpful? / Edit this page," a docs issue template, and
  one metric we actually watch: time-to-first-success proxied by quickstart
  page → cookbook page progression.
- **Cold-start doc QA — the pre-launch gate** (v3, the strongest remaining
  win): before anything ships, watch real cold readers fail.
  1. *Human pass*: 2–3 developers who have never seen NML run the quickstart
     and tutorial chapters 01–03 + 07 while thinking aloud; every stall is a
     docs bug, filed and fixed.
  2. *AI pass, repeatable*: a fresh AI agent in a clean environment is given
     ONLY the published docs and the task "get a validated config
     deserialized into a Rust struct." If it fails or improvises around the
     docs, the docs are wrong. Run this per release — it is cheap, brutal,
     and doubles as verification that AI assistants learn current syntax
     (the same channel most 2026 evaluators will ask first).
  No amount of self-review substitutes for watching a cold reader; this gate
  is what separates "we think it's clear" from "it converts."

### Phase 6 — Beyond state of the art (after 0–5 land) — sized separately

Honest calibration (v2): a browser playground is *parity*, not beyond — CUE
ships one today. The genuinely differentiating layer is the combination:

- **Browser playground**: nml-core compiled to wasm (the LSP already builds
  `wasm32-wasip1`); type NML on the left, AST/diagnostics/formatted output on
  the right; every docs snippet gets "open in playground."
- **Interactive diff demo**: two config panes, live `ChangeKind` output —
  sells the reload story visually. No other config language can show this.
- **Error index wired into the editor** (Phase 4) — squiggle → docs page.
- **AI-assistant rules file** (added in v2): a shippable
  "using NML with AI assistants" page + downloadable rules snippet
  (CLAUDE.md / .cursorrules block) teaching current syntax (`->`, `+`,
  `set<T>`, directives). Cheap, differentiating, and defensive: models
  trained on pre-rename snippets will otherwise generate wrong NML forever.
  Pairs with `llms.txt`/`llms-full.txt` from §3.2.
- **Interactive tutorial** (Svelte-tutorial style: prose left, live editor
  right, checkpoints) — stretch goal after the playground proves the wasm
  embedding.

### Phase 7 — Launch (new in v3): docs need an audience event

The v1/v2 plan ended at "site live" — but nobody wants what they never see.
Convert the docs investment into eyeballs, timed to the 0.2 spec cut:

- [ ] **Announcement post**: "Why we built NML" — the Diátaxis *explanation*
      doc doubles as the launch essay (typed config, the library-first wedge,
      the diff/reload story, honest comparison). Written once, used twice.
- [ ] Coordinated: r/rust post, Show HN, This Week in Rust submission,
      crates.io release, VS Code Marketplace listing — same week, so each
      channel reinforces the others and the repo looks alive to every
      arrival.
- [ ] The README, by-example page, and playground (if ready) are the landing
      targets; the cold-start QA gate (Phase 5) must be green first — launch
      traffic is a one-shot resource, don't spend it on unvalidated docs.
- [ ] Post-launch: triage Discussions daily for two weeks; every recurring
      question becomes a FAQ entry or a tutorial fix (the feedback loop's
      first real harvest).

## 5. Explicitly deferred (so scope stays honest)

- Versioned docs site (single "current" version pre-1.0; the changelog +
  stability page carry history).
- i18n / translations.
- Homebrew tap, distro packages (revisit post-1.0 or on demand).
- Video course / screencast series (VHS GIFs cover motion until demand exists).
- Workspace-wide LSP diagnostics docs (feature itself is `workspace_diagnostics:
  false` today — document what exists, not what might).
- Registry channel for schema packages (RFC-level; document the store, note
  the registry as future work).
- Non-Rust bindings (npm/Python via the wasm build) — a real adoption lever
  the docs work surfaces, but it is *product* work, not docs; revisit only if
  launch feedback demands it.

## 6. Sequencing & effort

| Phase | Effort | Dependency |
|---|---|---|
| 0 Truth fixes | ~½ day | none — do immediately |
| A Distribution & adoption | ~1–2 days | none; unblocks honest quickstart |
| 5 Harness (+ docs-gen, VHS) | ~1 day of Phase 5, pulled early | 0 |
| 1 README + by-example | ~1–1½ days | A (quickstart), harness |
| 2 Tutorials | ~3–4 days | 0, harness |
| 3 Library track + case study | ~3–4 days | A (docs.rs) |
| 4 Reference/spec + error index | ~3 days | 0 |
| 5 Site + feedback loop + cold-start QA | ~2 days | benefits 1–4 |
| 6 Playground, AI rules, interactive | separate | 5 |
| 7 Launch | ~1 day + a triage week | 0–5 green, QA gate passed |

Recommended order: **0 → A → 5(harness) → 1 → 2 → 3 → 4 → 5(site + QA gate)
→ 7(launch) → 6.** Phase 6 can land after launch — playground and interactive
docs compound over time, but launch should not wait on them.
Realism check: phases 0–5 are ~2½–3 weeks of focused solo work. If that must
compress, cut from Phase 4's spec-parity depth and Phase 3's recipe count —
never from the harness, Phase A, the cold-start QA gate, or tutorial chapters
01–03 + 07 (the conversion path).

## 7. Definition of done

- A newcomer on a clean machine can go from `cargo add nml-core` (published!)
  to a validated config deserialized into their own struct in ≤15 minutes
  using only the docs — and hits a first "it works" moment in ≤5.
- Every §2 capability has a documented home (tutorial, guide, or reference).
- `just docs-test` and `just docs-gen --check` pass in CI: all NML and Rust
  snippets verified, all generated tables current, GIFs regenerated.
- Spec, guide, and code agree on: keyword list, primitive count, grammar,
  arrow token, marker sigil, terminology table.
- README demonstrates a diagnostic with a quick-fix (as motion), a Rust embed,
  and an honest comparison table above the fold.
- Searching "nml configuration language" finds the docs site; docs.rs pages
  read like a product, not an afterthought.
- The cold-start gate passed: at least two human cold readers and one fresh
  AI agent completed the quickstart from the published docs alone, without
  improvising around them.
