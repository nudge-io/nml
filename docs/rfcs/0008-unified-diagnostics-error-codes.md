# RFC 0008 — Unified Core Diagnostics and Stable Error Codes

- **Status:** **Implemented** (2026-07-22; all eight steps, gates green —
  nml 901/0, nudge 3931/0 vs 3930/0 baseline, docs-test 14/14 + 5/5).
  Documentation landed per §7. Remaining from §7: the Phase 4 error-index
  pages + `codeDescription` links (docs-plan work, not this RFC's).
- **Builds on:** [RFC 0004 — Lossless CST](./0004-lossless-cst.md) (spans),
  the unified suggestion engine (2026-07-22, `nml-validate::suggest`), and the
  documentation plan's Phase 4 (error index).
- **Crates touched:** `nml-core` (new `diagnostic` module, `suggest` moves in,
  `error.rs` slimmed, `symbols`/`cst` signatures), `nml-validate` (module
  deleted in favor of core's), `nml-lsp` (single converter), `nml-cli`
  (code-aware rendering), **nudge** (2 files, ~3 import/call lines).
- **Removes (legacy, no shims):** `nml_validate::diagnostics` as a distinct
  type family; `nml_validate::suggest` as the engine's home; the LSP's second
  converter (`nml_error_to_diagnostic`).

## 1. Summary

The workspace has **two diagnostic systems**: `nml_core::NmlError` (parse,
lex, symbols, money — four variants of `{message, span}`) and
`nml_validate::diagnostics::Diagnostic` (severity, span, source, structured
`Suggestion`, one renderer). Everything good built this cycle — did-you-mean
hints, machine-applicable fixes, `rendered_message()` — lives only in the
second one, so core's own errors (unresolved references, template namespaces,
bad currency codes) **cannot hint**. The seam is hand-bridged **three
separate times** today: `nml-validate/loader.rs::to_diagnostic`,
`nml-lsp::nml_error_to_diagnostic`, and the CLI's inline error printing —
three converters that can drift independently.

This RFC moves the diagnostic model down into `nml-core` as the single
`diagnostic` module, moves the suggestion engine with it, gives the model a
**real, populated error-code field** (`code: Option<Code>` — not reserved,
implemented), and migrates every producer and consumer. One model, one
renderer, one converter, one code space.

## 2. The type (nml-core::diagnostic)

```rust
#[non_exhaustive]
pub struct Diagnostic {
    pub code: Option<Code>,          // stable, never reused once released
    pub severity: Severity,          // Error | Warning | Info (non_exhaustive)
    pub message: String,             // prose; hints NEVER hand-baked here
    pub span: Option<Span>,
    pub source: Option<String>,      // multi-source loads (RFC 0030)
    pub suggestion: Option<Suggestion>, // machine-applicable fix
}
```

Builders (`error`/`warning`/`info`/`with_span`/`with_source`/
`with_suggestion`/`with_code`), `rendered_message()` (the one hint renderer),
and `Display` move over unchanged in behavior. `Suggestion` moves as-is.

Two hardening changes over the current type (both verified free in-repo —
nothing constructs these via struct literal outside their module):

- **`#[non_exhaustive]` on `Diagnostic` and `Severity`.** The RFC's promise
  is "the type breaks once, ever"; `non_exhaustive` turns that from a hope
  into a compiler guarantee — future fields (e.g. related-spans, tags) and
  future severities (e.g. `Hint`) become non-breaking additions. Builders are
  the construction path, which they already are everywhere.
- **`Severity` gains `Info` now** — not speculatively: the LSP *already
  emits* an `INFORMATION`-severity diagnostic (the RFC 0030 undeclared-
  sibling notice) that today is hand-built precisely because the two-variant
  enum cannot express it. Without `Info`, "every producer flows through the
  one converter" would be false by omission. `Hint` is *not* added — it has
  no producer, and a variant nothing emits is dead code; `non_exhaustive`
  makes adding it later free.

**Principle made explicit:** `NmlError` and `Diagnostic` are different roles,
not duplicates — `NmlError` is the *abort* error for `Result` signatures
(implements `std::error::Error`); `Diagnostic` is a *findings report* (data,
deliberately not an error trait object). rustc draws the same line between
fatal errors and diagnostics. This RFC unifies the findings path and keeps
the abort path thin — it does not force one vocabulary onto both roles.

### 2.1 `Code`

`pub struct Code(u16)` — **inner field private**: codes are constructible
only from the vetted constants in `diagnostic::codes`, so the never-reuse
rule is enforced by construction, not convention (`Code(9999)` from outside
the module is a compile error). `Display` = `NML{:04}` (flat rustc-style
space — familiar, greppable, sortable) is the **only** accessor — both known
consumers (the CLI prefix and the LSP's `code` field, which the LSP spec
types as number-or-*string*) want the formatted form, and a numeric getter
with no consumer would be speculative API, the same sin as a reserved field.
Add one when a consumer exists. Constants (SCREAMING_SNAKE, e.g.
`codes::UNRESOLVED_REFERENCE`) live in `diagnostic::codes`, grouped by band,
each with a doc comment that later seeds its error-index page. Bands are an
allocation convenience, **not API**: a diagnostic moving between subsystems
keeps its code.

| Band | Subsystem |
|---|---|
| 0001–0999 | Lexing & parsing |
| 1000–1999 | Symbols & resolution (duplicates, unresolved refs, const cycles) |
| 2000–2999 | Schema validation (`nml-validate`) |
| 3000–3999 | Values & money |
| 4000–4999 | Schema packages & store |
| 5000–5999 | Editor/LSP-specific (directive vocabulary, project config) |

Rules (added to `docs/stability.md`): a code, once released, is never
renumbered and never reused; retirement leaves a tombstone in the index. A
unit test asserts constant uniqueness. Sites not yet assigned a code emit
`None` — partial coverage of a real mechanism; the Phase 4 sweep completes it.

## 3. What happens to `NmlError`

It stays — as what it honestly is: the thin **`Result` abort error** for
`parse()`, `format_source()`, and money decoding. It gains
`to_diagnostic(&self) -> Diagnostic` (variant → code band). One structural
change: `InvalidMoney` gains a `currency: Option<(String, Span)>` field
carrying the offending code **and its own sub-span** (captured at the parse
site, where the trailing-token fact is local), so conversion attaches a
machine-applicable did-you-mean against the ISO-4217 table **without message
parsing** (the module's own founding rule).

Signature migrations (all list-of-findings producers become `Vec<Diagnostic>`):
- `cst::parse_to_ast_all` (nudge: zero call sites — verified)
- `SymbolTable::{find_duplicates, find_unresolved_references, find_const_cycles}`
  (nudge: one `.message()` call site in `workflow/parser.rs` — updated directly)
- *(follow-up, same day)* `cst::extract_schema` and the schema-integrity
  finders (`find_oneof_errors`, `find_shorthand_errors`, `find_model_cycles`,
  `find_extends_cycles`) — completing the boundary principle across every
  public findings API; the loader's severity-override bridge thereby lost its
  last caller and was deleted, and `find_model_cycles` carries its advisory
  severity at the source.

New hints this unlocks, wired in this RFC:
- unresolved reference → nearest declared const/template/declaration name
- `19.99 USd` → `did you mean "USD"?`
- unknown template namespace (LSP `validate_templates`) → nearest configured
  namespace

## 4. Consumers

- **nml-validate**: its `diagnostics` and `suggest` modules are **deleted**;
  all internals import `nml_core::diagnostic`/`nml_core::suggest`. No
  re-export shims — nothing is published yet, and shims are exactly the
  legacy accommodation this project removes. Validation sites touched in the
  migration get band-2000 codes assigned as they're edited.
- **nml-lsp**: `nml_error_to_diagnostic` is deleted; every producer (parse,
  symbols, validator, templates, directive vocabulary, **and the
  undeclared-sibling info notice**, now expressible via `Severity::Info`)
  flows through the one converter, which maps `code` → LSP `Diagnostic.code`
  and `Severity::Info` → `INFORMATION`. `codeDescription.href` stays unset
  until the error-index pages exist (Phase 4) — a URL that 404s is worse
  than none.
- **nml-validate/loader.rs**: its private `to_diagnostic` converter is
  deleted in favor of `NmlError::to_diagnostic()` — the third and last
  hand-rolled bridge.
- **nml-cli**: one shared printer; codes render rustc-style —
  `error[NML2001]: invalid value "wran" … (did you mean "warn"?)`.
- **nudge** (sibling repo, heavy uncommitted work in tree — see §6), ~4
  lines in 2 files: `embedded_schemas.rs` import line →
  `nml_core::diagnostic::Severity` plus a wildcard arm on its `Severity`
  match (`non_exhaustive` requires it — the arm is honest future-proofing,
  not busywork); `workflow/parser.rs` `.message()` → `rendered_message()`,
  which upgrades its `bail!` output with hints for free.

## 5. Execution plan (green tree between steps)

| # | Step | Gate |
|---|---|---|
| 0 | Preflight: re-grep nudge exposure; baseline both suites (nml 897/0, nudge full) | baselines recorded |
| 1 | **Additive:** new `nml_core::diagnostic` (+`codes` + uniqueness test) and `nml_core::suggest`; nothing consumes them yet | nml suite green |
| 2 | nml-validate cut-over: delete its modules, update imports, assign band-2000 codes at touched sites | nml suite green |
| 3 | Core emitters: `NmlError::to_diagnostic` (deletes loader.rs's private converter), `InvalidMoney.currency`, migrate `parse_to_ast_all` + `SymbolTable` finders, add the three new hint sites | nml suite green |
| 4 | LSP: single converter (incl. `Severity::Info` for the undeclared-sibling notice), `code` mapping, templates/directives on the one path | LSP + harness tests green |
| 5 | CLI: shared code-aware printer; refresh README demo output block | docs-test green |
| 6 | nudge: the ~3-line update; **touch nothing else** | nudge full suite == baseline |
| 7 | Docs: CHANGELOG, stability.md code rules, rustdoc on `diagnostic` (seeds the Phase 4 index), plan-doc execution log | all gates + fmt + clippy |

Estimated effort: one focused day. Steps 1–5 are one repo and revertible as a
unit; step 6 is deliberately last and minimal because nudge's working tree
carries unrelated uncommitted work — imports only, suite compared
before/after so any failure is attributable.

## 6. Risks & mitigations

- **nudge tree contamination** — mitigated by the import-lines-only rule and
  baseline/after suite comparison (§5 step 0/6).
- **Signature breaks reaching unknown callers** — `parse_to_ast_all` and the
  finders were grepped across both repos (nudge count: one), re-verified at
  step 0 before any edit.
- **README demo text drift** — the CLI gains `error[NMLxxxx]` prefixes; the
  demo's `expect-error` block pins a substring that survives, but the shown
  output text is refreshed in step 5 and CI-verified.
- **Premature code stability** — codes become stable *at first crates.io
  release*, not at merge; pre-publish renumbering is legal and free.

## 7. Documentation (required section)

- `docs/stability.md`: code stability rules (never renumber/reuse; tombstones).
- CHANGELOG: Added (unified diagnostics, error codes, new hint sites),
  Changed (signature migrations, CLI output format).
- Rustdoc: module-level docs on `diagnostic` + per-code doc comments — the
  single source the Phase 4 error-index pages generate from.
- README: demo output refreshed to show the code-prefixed diagnostic.
- Phase 4 (docs plan) completes: full code coverage sweep + error-index pages
  + LSP `codeDescription` links to the published site.

## 8. Open decisions (approve before execution)

1. **Band allocation** (§2.1 table) — codes are forever once released.
2. **CLI shows codes** (`error[NML2001]:`) — recommended yes, rustc-familiar.
3. **`Code(u16)` + `NML{:04}`** vs. per-subsystem letter prefixes — flat
   numeric recommended.
