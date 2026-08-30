# RFC 0009 — Structured Parse Errors and the 0xxx Code Band

- **Status:** **Implemented** (2026-07-23). The taxonomy is **closed**:
  every syntax-error site emits a payload-carrying `ParseErrorKind`
  (message/code/suggestion all derived; ~40 sites over 16 kinds, codes
  NML0001–NML0015 with CI-verified index sections), the transitional
  `Generic` carrier and prose helpers are **deleted**, and the `Lex`/
  `Parse` variants are merged into one `Syntax { kind, span }` (the phase
  distinction carried nothing the code doesn't). Money joined the model
  (D13): `MoneyErrorKind` with NML3000/3001/3002/3003, leaving **no
  `String` field in `NmlError`**. Shipped with the sweep: token-width
  spans from the single emission channel (`error_kind` /
  span-override `error_kind_at`), same-offset `Expected` coalescing,
  exact-count suppression accounting with an `info` marker at the one
  findings boundary (`finalize_diagnostics`), control-character
  sanitization in the shared `Rendered` adapter, `Diagnostic::related`
  (CLI `note:` lines, LSP `relatedInformation`; first producer:
  unterminated strings), the fixers `&&`→`&` (as `DoubleAmp`, sharing
  NML0001's fix pattern) and `set<a, b>`→`set<a | b>`, and the
  offside-rule page (NML0006) listing open columns — refinement from
  cold-start QA remains a docs-debt item. History: foundations 2026-07-22;
  identity findings `Diagnostic`-native + `Validation` deleted 2026-07-23.
- **Builds on:** [RFC 0008](./0008-unified-diagnostics-error-codes.md)
  (codes, index, guard), the stability policy's fixers commitment.
- **Crates touched:** `nml-core` (error.rs reshaped, lexer/parser emission
  sweep, `diagnostic` gains related-info), `nml-lsp` (relatedInformation),
  `nml-cli` (note-line rendering). nudge: audit expected zero (`NmlError`
  had no references at last sweep; re-verify at step 0).

## 1. Design

Parse/lex errors become **payload-carrying kinds** (modern rustc's shape —
structs with semantic fields, everything derived), not prose tagged with a
code. Message, code, and suggestion all derive from one payload; the two
channels cannot drift because there is only one channel:

```rust
#[non_exhaustive]
pub enum ParseErrorKind {
    UnexpectedToken { expected: &'static [SyntaxKind], found: SyntaxKind },
    UnclosedString { open: Span },            // → related-info label
    InvalidEscape { escape: String },
    BadIndentation { expected: u32, found: u32 },
    ReplacedSyntax { old: &'static str, new: &'static str },
    NumberOutOfRange { raw: String },
    // …the recovery classes; ~8 at introduction
}
```

- `code()` is an **exhaustive match** minting from `diagnostic::codes`
  (allocated sequentially from `NML0001` in the 0xxx band) — a new kind
  cannot compile without a code decision, and the bidirectional guard then
  forces its index section. Taxonomy discipline becomes
  compiler-enforced end to end.
- `message()` derives from the payload via a **single**
  `SyntaxKind::describe()` human-name mapping — consolidating the expected-
  token wording currently scattered across the emission sites' format
  strings (a DRY win independent of codes).
- `suggestion()` makes **`ReplacedSyntax` the fixers engine**: every syntax
  migration (`=>`→`->`, `!`→`+`, and all future renames) emits a
  machine-applicable replacement at the lexer — quick-fix in editors,
  did-you-mean in the CLI, one table entry per rename. Its index section is
  the living migration ledger the stability policy points at.

**Step 0 — variant audit, not decoration:** after RFC 0008, audit live
constructors of every `NmlError` variant. Expected outcome: `Validation` is
dead (delete), `InvalidMoney` folds into kinds, and `NmlError` collapses
toward a single `Syntax { kind, span }` shape. Decorate only what survives.

**Related information:** `Diagnostic` gains `related: Vec<Related>`
(`Related { span, message }`, `with_related` builder) — non-breaking under
`#[non_exhaustive]`. `UnclosedString` labels its opening quote; the LSP maps
to spec-native `DiagnosticRelatedInformation`; the CLI prints secondary
`path:line:col: note: …` lines from the shared printer.

## 2. Pedagogy

Index pages per kind; two are the point: **`BAD_INDENTATION`** (the
offside-rule mental model — written after tutorial cold-start QA shows how
newcomers actually stumble) and **`REPLACED_SYNTAX`** (the migration
ledger). Parse-error examples are trivially CI-verifiable
(`expect-error='[NML0001]'`; deliberate-error blocks are ban-exempt).

## 3. Documentation (required)

Index sections for every kind; stability.md cross-link from the fixers
paragraph to the ledger page; CHANGELOG; rustdoc on the kind enum (seeds the
pages).
