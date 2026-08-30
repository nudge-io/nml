# RFC 0023 — Structural fix resolution and composition conformance closure

- **Status:** **Implemented 2026-08-30** (all parts, in the Sequencing
  order; RFC 0025 is not yet built) — sealed 2026-08-29 after five
  review rounds, then three post-implementation review rounds
  (2026-08-30, adversarial lanes + paired certifiers) folded in place:
  the block-form `.shared` distribution refusal (§A.2 — the sealed text
  knew only the scalar shape's `NoNodeAt`), the adaptive round budget
  and its collision mechanism (§A.3), the linear seal-count dedup
  (Part D), the editor's cache-text coherence (§A.3), per-suggestion
  verbatim-span validation (`InvalidSpan`, §A.2) with the colon splice
  checked against accepted edits, and terminal sanitization of every
  walked-filename print in the CLI
  (round 1: four design lanes — edit layer + appliers, compose semantics,
  spec conformance, whole-document coherence; round 2: certification + an
  implementation dry-run; rounds 3–5: certification, each round's blockers
  folded; the round-5 certifier's single sealing edit folded verbatim).
- **Date:** 2026-08-29
- **Crates:** nml-core (cst/edit, cst/lower, ast, diagnostic, error, layers),
  nml-cli (fix, check renderer), nml-lsp (server code actions, diagnostics
  wire and mapping), scripts/docs_test.py, .gitignore
- **Depends on:** RFC 0004 §4.3 (lossless CST — the trivia-attachment policy,
  0004:157-161), RFC 0009 (diagnostics contract), RFC 0015 (nominal union
  annotations), RFC 0017 §4.1 (the sole-candidacy applier), RFC 0019
  (instance composition), consumer RFC 0030 (nudge: the `Suggestion` wire
  contract and the structural CST edit doctrine)
- **Amends:** RFC 0019 (the receiver rule's reach, NML2060 count semantics,
  the one-entry-per-field invariant, plan item 2 — errata E15–E17, E19);
  the error index entries NML0016, NML2042, NML2060, NML2062, NML2077
- **Companion:** RFC 0025 (normalize-on-merge) builds on Parts B–C and is
  sequenced after this RFC lands.
- **Origin:** the RFC 0019 union-compose review arc (rounds 12–19) and the
  design reviews of 2026-08-29. Every claim below is grounded with a
  file:line against the working tree or a probe restated as a fixture in
  the test plan.

## Summary

Five parts (the former Part E became RFC 0025), landable in the order
given under Sequencing:

- **Part A — Structural fix resolution.** One resolver in nml-core turns a
  diagnostic's suggestions into byte splices for BOTH appliers (`nml fix`
  and the editor's quick-fix): verbatim substitution for did-you-mean and
  mechanical fixes, and a new structural kind `SuggestionKind::Delete`
  that removes a body entry, a `uses` clause, or a clause reference from
  the lossless CST by **token walks, never tree mutation**. Every refusal is
  per-suggestion and printed. The structural-injection refusal moves into
  the resolver, so no applier is exempt. The textual whole-line widening is
  deleted. NML2062 gains the "delete the clause" fix RFC 0019 promised;
  NML2077's hand-stitched deletion migrates to the same kind. NML0016's
  out-of-string deletion — which corrupts CR-terminated files — is removed.
  The editor offers an action only for a diagnostic that is still current.
- **Part B — The head rule.** A composed entry carries the span, name and
  provenance of the **head of its surviving group** — the layer whose
  contribution the composed body IS — at the three routes that still
  anchor at the base after a switch. This extends RFC 0019's receiver rule
  from the body to the entry, fixes a finding-loss bug under the one-home
  dedup key, and moves NML2085's item-scope note to the establishing item.
- **Part C — Non-string discriminators.** Discriminator entries are stripped
  **by name** on both sides of the plan/merge seam — the validator's own
  reading — and the surviving group's non-string entries pass through so
  the every-entry NML2042 fires on the composed view. No new verdict; one
  verdict moves and one stops firing, both tabled. The one-entry-per-field
  invariant is restated truthfully.
- **Part D — Seal-hit identity and the NML2060 count.** The backstop message
  counts **distinct sealed fields** (RFC 0019's promise) with an assignment
  count when it exceeds the fields, over a structured identity that is
  hashed, never printed. `Related.source` (RFC 0019 plan item 2) lands, with both
  renderers locating a note in its own file.
- **Part F — RFCs as tracked design records.** `docs/rfcs/` is tracked;
  two external vendor identifiers in examples are replaced by the
  documentation's fictional vocabulary under a gate-enforced reserved-name
  rule; the docs gate's coverage becomes identical locally and in CI.

## Motivation

Three live defects and one specification contradiction:

1. **Data corruption by `nml fix`.** On a CR-terminated file
   (`a = 1\rb = 2\rc = 3\n` — the old-Mac line ending the source policy's
   own tests exercise, `crates/nml-core/src/source_policy.rs:106`) the
   NML0016 out-of-string deletion produces `a = 1b = 2c = 3` and the round
   is **accepted**, because `improved` counts 8 → 6 findings
   (`nml-cli/src/fix.rs:304-309`); `service Api:\r    port = 8080\r`
   becomes `service Api:    port = 8080`. The producer's claim "deleting the
   stray CR provably preserves intent (it is never a line ending)"
   (`crates/nml-core/src/error.rs:419-422`) is false for that file class,
   and the index's "the file still parses" (NML0016 entry) with it — its
   own fence expects `[NML0002, NML0002, NML0016, NML0016]`.
2. **Finding loss.** Two dependents that both switch away from a base each
   compose a body missing the new arm's required field; both NML2007s
   anchor at the **base's** `cfg:` and collapse to one under the
   `(code, span, message)` dedup key (`crates/nml-core/src/layers.rs:3847-3866`;
   `nml-cli/src/main.rs:431-445`). The same collapse happens at item scope,
   and a base→mid-switch→top-join chain anchors at the base.
3. **A misattributed note.** NML2085's item-scope `established here` note
   anchors on the accumulated item's span (`anchors[0] = existing_item.span`,
   `layers.rs:5505`/`5624`), so after a mid-layer `as` switch it points at
   the base item while the message names the mid layer's variant.
4. **Spec contradiction.** The error index documents NML2060's
   `(and N more)` as a count of *assignments*; RFC 0019 says "the message
   states the count when more than one sealed **field** would be
   discarded" (`docs/rfcs/0019-instance-layers-and-sealed-fields.md:700-702`).

And one structural cause: `nml fix` reasons about text it does not
understand (whole-line widening by whitespace predicates) while the
lossless CST knows every token — and the editor's quick-fix path applies
suggestions with **no** injection guard at all
(`crates/nml-lsp/src/server.rs:5324-5338`).

## Part A — Structural fix resolution

### A.1 The suggestion kind

```rust
// crates/nml-core/src/diagnostic.rs
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    DidYouMean,
    Fix,
    /// Structural deletion of the syntax node whose content span equals
    /// `span`: a body entry (`BodyEntry.span`), a `uses` clause
    /// (`BlockDecl.uses_span`), or a clause reference (`Identifier.span`).
    /// `replacement` is always empty. Singular and machine-applicable
    /// (DidYouMean's exclusivity, RFC 0017 §4.1); the bytes are computed
    /// only by `cst::edit::resolve_suggestions`.
    Delete,
}

impl SuggestionKind {
    /// The LSP `data.suggestions[].kind` string — exhaustive HERE, where a
    /// new variant cannot compile without naming itself.
    pub fn wire_name(self) -> &'static str { /* "didYouMean" | "fix" | "delete" */ }
    /// The inverse, for the editor's code action (an unknown string is no action).
    pub fn from_wire_name(s: &str) -> Option<Self>;
}

impl Diagnostic {
    pub fn with_deletion(self, span: Span) -> Self; // pushes {"", span, Delete}
}
```

`Delete` renders nothing in the message — the producer's prose states the
action (`Rendered` already filters by kind, `diagnostic.rs:800-812`). The
wire name lives in the defining crate because `#[non_exhaustive]` would
otherwise force a wildcard arm on nml-lsp's exhaustive match
(`crates/nml-lsp/src/diagnostics.rs:422-425`) and lose the compile-time
forcing; RFC 0008 applied the attribute to `Diagnostic` and `Severity`
(`docs/rfcs/0008-unified-diagnostics-error-codes.md:56-63`) and excludes
nothing. No external crate matches on the kind (nudge: zero references;
nml-validate compares two values in tests, unaffected).

**The empty-replacement convention is retired.** Today an empty replacement
means "structural deletion" (`diagnostic.rs:795-804`; `error.rs:571-574`).
After this RFC only `Delete` is structural; `Fix("")` remains a verbatim
byte removal — `NumberTrailingDot` deletes the last byte of a `Number` token
(`error.rs:427-429`) and still renders ` (fix: remove)` (`diagnostic.rs:837-841`).
The `|| matches!(kind, BareCarriageReturn { .. })` classification clause at
`error.rs:573-579` **stays** (the surviving in-string `\r` fix is not
whitespace and must remain a `Fix`); only its comment and the test at
`diagnostic.rs:947-953` change.

**Producers.** Exactly three; the empty-replacement producers today are
`error.rs:422, 431` and `layers.rs:888, 4283`:

| Code | Node | Today | After |
|---|---|---|---|
| NML2060 equal-value restatement | a `Property` or inline `Modifier` — `scalar_of` returns `None` for blocks (`layers.rs:4258-4283`); a **fourth shape** is a `.shared`-distributed restatement, whose entry span is the synthesized property's (`sp.name.span`, after the dot) and matches no node | `with_suggestion("", c.entry.span)`; the `.shared` shape is rejected by parse regression in `nml fix` ("not auto-fixable") and would emit a broken edit (`. = "s"`) in the editor | `with_deletion(c.entry.span)`; the scalar `.shared` shape resolves to `NoNodeAt` and the block `.shared` shape to `SharedDistribution` (its rows are real nodes distributed into every item — deleting one on one item's diagnostic would strip every sibling's value), both printed — an improvement on both surfaces |
| NML2062 `uses` on a schema definition | the `uses` clause | message "delete the clause", **no fix** (RFC 0019:1097 promises one); the diagnostic anchors at `block.name.span` (`layers.rs:1255-1264`), outside the `Uses` node | `with_deletion(block.uses_span)` — a new `BlockDecl.uses_span: Option<Span>`, the `Uses` node's content span, recorded by the lowerer (`crates/nml-core/src/cst/lower.rs:67`; invariant `uses_span.is_some() == !uses.is_empty()`); the primary span stays the name |
| NML2077 redundant `uses` ref | one `Ident` in the clause | a hand-stitched span `[prev.end, ref.end)` (`layers.rs:874-889`) with a dead `idx == 0` branch — `site_checked_refs` returns refs only when all resolved (`:1006-1041`), `deduped` preserves first occurrence (`:784-789`), and the pair loops iterate `b` after `a`, so the redundant ref is never first | `with_deletion(blk.uses[idx].span)`; the dead branch becomes `return None` with the ordering argument; a name listed more than once carries no fix (one span cannot remove two occurrences, and a fix that does not fix stalls the round gate) |

NML0016 (a raw CR byte) stays on the verbatim path — see A.4.

### A.2 The resolver

```rust
// crates/nml-core/src/cst/edit.rs
/// Every refusal is per-suggestion; nothing refuses a batch. Display
/// carries no offsets — the printer adds `line:col` from `span`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SuggestionError {
    #[error("replacement contains a control character")]
    ControlCharacter { span: Span },
    #[error("a structural deletion needs a source that parses ({errors} error(s))")]
    UnparsableSource { span: Span, errors: usize },
    #[error("no deletable node at this span")]
    NoNodeAt { span: Span },
    #[error("the node shares its line with another entry")]
    NotLineExclusive { span: Span },
    #[error("the entry is distributed by its `.shared` block into every named item")]
    SharedDistribution { span: Span },
    #[error("the span is not a valid byte range of the source")]
    InvalidSpan { span: Span },
    #[error("overlaps an earlier suggestion's edit (deferred to a later round)")]
    Overlap { span: Span, with: usize },
}

/// What a structural deletion removed — closed, so every title exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted { Property, NestedBlock, ListItem, Modifier, SharedProperty, Arm, FieldDef, UsesClause, LayerRef }
impl Deleted {
    /// "Delete this property", "Delete this nested block", …,
    /// "Delete the `uses` clause", "Remove this layer reference".
    pub fn title(self) -> &'static str;
}

pub struct Resolved {
    /// Sorted, non-overlapping (adjacent allowed) — `splice`-ready.
    pub edits: Vec<SpliceEdit>,
    /// One outcome per input suggestion, in order: a verbatim edit
    /// (`Ok(None)`), a structural deletion (`Ok(Some(_))`), or a refusal.
    pub outcomes: Vec<Result<Option<Deleted>, SuggestionError>>,
}

/// ONE owner for both appliers. Verbatim substitution for DidYouMean/Fix
/// (the injection refusal — the render-escape set: every Unicode
/// control, the source policy's Trojan-Source bidi set, U+2028/U+2029)
/// and structural expansion for Delete.
/// Bounds and char boundaries are validated by the shared edit check the
/// applier's `splice` also runs.
pub fn resolve_suggestions(source: &str, suggestions: &[Suggestion]) -> Resolved;
```

**Why token walks, not mutation.** Deletion mints no tokens. rowan's
`detach` requires a mutable clone, and the tree's trivia ownership makes
node detachment wrong: the parser flushes leading trivia into the node that
opens next (`crates/nml-core/src/cst/parser.rs:1552`, `flush_leading`), so an
entry's terminating newline is the **next entry's leading trivia**; the
header's newline is the `Body`'s first token; the last entry's terminator
and any trailing blank-line newlines sit in the `Body` before `Dedent`.
Detaching a node swallows its neighbour's line break (probe: detaching `db`
yields `    host = "h"    |tags:` and a broken reparse). The tree-mutation
engines (Roslyn `SyntaxEditor.RemoveNode` with `SyntaxRemoveOptions`, Biome
`BatchMutation::remove_node` with trivia transfer, rust-analyzer
`ted::remove`) each carry an explicit trivia policy because detachment
alone mis-assigns line breaks; range computation over the token stream
expresses that policy without a mutable clone. clang-tidy's
`FixItHint::CreateRemoval` and ESLint's `fixer.remove` are textual and
produce exactly the blank-line wart this RFC deletes. The insertion
primitive (`insert_entry_at_path`) keeps its tree-splice implementation
because insertion mints tokens; the module doc gains the second operation
and the rationale for the asymmetry.

**Parsing.** The resolver parses once, lazily, on the first `Delete`. A
parse error refuses **each** `Delete` (`UnparsableSource`) and never a
verbatim suggestion: the parse-layer fixes (`=>`→`->`, `&&`→`&`,
`MultilineClosingMisaligned`, `NumberTrailingDot`) act only on files that do
not parse — they are how a file *becomes* parseable (`fix.rs:19-28`) — and a
batch refusal would disable them. (`insert_entry_at_path`'s gate stays
batch-level because insertion is always structural.)

**One locator rule.** The tree is indexed by recomputed `content_span`
(`crates/nml-core/src/cst/syntax.rs:281-297`, first to last non-trivia token;
zero-width `Indent`/`Dedent` are not trivia, so a nested block's span ends
at its `Dedent`, after its trailing newline) over three target classes:
entry nodes (`content_span` is what `lower.rs:148` records on every
`BodyEntry`, and every entry kind opens with its own significant token, so
it is unique), `Uses` nodes (`BlockDecl.uses_span`), and clause-reference
`Ident` tokens — the children of `Uses` after the `uses` keyword, which is
itself an `Ident` inside the clause and must never be a target
(`cst/ast.rs:393-398`, `clause_idents`, skips it). Equality is required; a
span that locates nothing is `NoNodeAt`, so a stale span fails closed
instead of editing a stale offset (what the editor path does today).
"Significant" for the range walks below means neither trivia nor a layout
marker — a different set from `content_span`'s.

**Entry deletion — two token walks, no text predicates.** Backward from
the entry's first significant token through `Whitespace | Indent | Dedent`:
a `Newline` (or the stream start) gives `start` = its end; any other token
refuses (`NotLineExclusive` — the grammar accepts several entries on one
line, `host = "h"    port = 1` parses as two `Property` nodes, and the
formatter never prints that shape; a total rule exists for that case and is
deferred). Forward from the last significant token through
`Whitespace | Indent | Dedent`: a `Newline` gives `end` = its end (nested
blocks end `Newline Whitespace Dedent` — the whitespace is the outer
deferred comment's indentation and survives; a block-form value's interior
newlines are inside its `String` token, and its terminator is the next
entry's leading trivia like any property's); a `Comment` gives `end` = the
comment's start with `start` = the first significant token (the indentation,
the comment and its newline keep the line — today's tested behaviour); the
stream end gives `end` = the source length (EOF without a newline keeps the
header's newline); any significant token refuses. CRLF needs no rule: the
`\r` is a `Whitespace` token before the `Newline`. Blank lines after a
target stay (the walk stops at the first newline); a body with blank lines
is never fmt-clean, so A.5's property is unaffected. All ranges verified
fmt-clean on the probe set (first/middle/last entry, nested block,
block-form value, CRLF, EOF, trailing comment, own-line comment above,
deferred outer comment).

**Comments.** RFC 0004 attaches an own-line comment as leading trivia of
the *following* node and a same-line trailing comment to the *preceding*
node (`0004:157-161`). The walks start at the entry's first significant
token, so an own-line comment above the entry **stays** (never delete prose
the author placed before something) and a trailing comment is kept at the
indentation. Comments inside a deleted nested block go with it — they are
the block's content — except a trailing OWN-LINE comment at the block's
end, which RFC 0004 attaches to the *following* outer node: it survives,
at its authored indentation (prose is never deleted on structural
grounds; the result reparses).

**Emptied bodies.** A body is the `Body` node `Newline Indent entry*
[Newline] Dedent` after the header's `Colon` (`parser.rs:343-350, 622-636`;
the trailing newline is absent at EOF; an empty nested block `b:` has no
`Body` node at all — the walks still work, its last significant token is
the colon). When every entry child of a body is a target: if the owner is a
top-level `BlockDecl` that still carries an `is`/`uses` clause **after this
batch**, add one splice deleting the colon byte — the formatter's canonical
bodyless form for such headers (`crates/nml-fmt/src/formatter.rs:157-166`;
probed: `flow t uses base:` → `flow t uses base`, `flow t is p:` →
`flow t is p`); every other owner — a plain block (`service Api:`), a
nested block, a named item, a modifier block, an arm target (arms carry
bodies, `parser.rs:687-693`) — **keeps** its colon (the formatter prints one
there; dropping it would make the fixed file fmt-dirty); a `SharedProperty`
block (`.x:`) never empties, because ANY target inside a `.shared` body is
refused (`SharedDistribution`) unless a containing deletion subsumes it: a
shared row is distributed into every named item, so deleting it on one
item's diagnostic silently rewrites the siblings (the block-form NML2060
shape — probed to corruption in review), and deleting the last row would
re-read `.x:` as a scalar the formatter prints `.x = ""`. No producer targets a `ListItem`; the rule covers it for
totality.

**Clause deletion (NML2062).** The `Uses` node's text range — it owns the
whitespace separating it from the header (`Uses@6..16` = ` uses a, b`):
`model n uses m:` → `model n:`; `flow F uses b:` with a body → `flow F:`. A
bodyless header whose only clause is deleted becomes `flow t:` — a
**replacement** (the parser accepts `flow t`, but the formatter prints the
colon on a bodyless plain header; `flow t is p uses a` keeps its `is` and
no colon), which is one reason the output is `Vec<SpliceEdit>` and not
`Vec<Span>`.

**Clause-reference deletion (NML2077).** The `Ident` plus one separator:
a non-first ref takes the separator before it (`uses mid, base` →
`uses mid`; `uses a, b, c` deleting `b` → `uses a, c`); the first ref takes
the separator after it (`uses a, b` deleting `a` → `uses b`). Deleting the
only reference is a clause deletion. Today's producer can never reach the
first-ref or only-ref cases (A.1), so the rule is defined once for totality
and the producer's dead branch is removed.

**Batch rules.** A target contained in another target is subsumed (an entry
inside a deleted block): its outcome is `Ok(Some(_))` with no edits of its
own, so it counts as applied and is never printed as a refusal; a body
emptied by several targets yields its colon splice **once**; edit-level non-overlap is validated over the *resolved*
edits — RFC 0017 §4.1 keeps sole-candidacy on suggestion spans
(`docs/rfcs/0017-duration-literals.md:249-262`), but the resolver mints
edits the applier never saw (a colon drop, a separator) — so the overlap
predicate is factored out of `splice` (`edit.rs:413-421`: strict, so
adjacent edits are legal) into one shared `validate_edits`, and a later
suggestion whose edits overlap an earlier **accepted** suggestion's is
refused (`Overlap { with }` naming that suggestion's index; greedy in
suggestion-span `(start, end)` order, the order `sole_candidates` sorts
by; a refused suggestion contributes no edits, so nothing overlaps with
it) — the applier's own deferral doctrine (`fix.rs:311-314`); the next
round re-derives it. In the one realistic overlap (an entry deletion
containing a value-level verbatim fix) the container's span starts
earlier, so the deletion lands and the moot inner fix is dropped. No producer pair
reaches a shared header today (NML2077 aborts the block's composition, so
NML2060 never co-occurs — probed); the rule is a totality guarantee.

### A.3 The two appliers

- **`nml fix`** (`nml-cli/src/fix.rs`): `sole_candidate_edits` becomes
  `sole_candidates(&[Diagnostic]) -> Vec<(FindingKey, Suggestion)>` —
  one suggestion per diagnostic, same-span agreement, and byte-identical
  `(span, replacement, kind)` candidates collapsed to one (two diagnostics
  carrying the same suggestion are one application, as today's dedup),
  sorted by suggestion span; the greedy non-overlap pass leaves with the
  control-character filter and the whole-line widening (overlap has ONE
  owner, the resolver, and its refusals are printed — a silent greedy pass
  would be an exception to "prints every refusal"); the key is what the
  round gate needs; the round calls `resolve_suggestions(&text, &sole)`, applies
  `Resolved::edits`, and **prints every refusal** as
  `<file>:<line>:<col>: fix refused: <reason>` (a refusal is a legitimate
  outcome, not an upstream bug; today's silent `Err(_) => break` hides it
  behind "0 edit(s) applied"). The doc sentence on `sole_candidate_edits`
  (`fix.rs:322-323`) "editors present quick-fixes for a human to eyeball; a
  batch applier must refuse this class by construction" is replaced: **every
  applier refuses it, by construction, in one place.** No producer can emit
  a control character today — the role-literal producer's charset gate
  excludes them (`crates/nml-validate/src/schema.rs:2794-2808`),
  `MultilineClosingMisaligned` is spaces, the in-string CR fix is the
  two-character `\r` escape — so the refusal is defense in depth, and the
  CLI test that claims to exercise it (`tests/integration/cli_tests.rs:433-450`)
  is vacuous: its fixture emits no suggestion at all. It is replaced by a
  resolver unit test with a `\n` in a verbatim replacement.
- **The round gate.** `improved` (`fix.rs:304-309`) counts findings, which
  rejects a legitimate round that *reveals* the next finding (probe:
  applying NML2077's ref deletion reveals an NML2060; 1 → 1, so `nml fix`
  reports zero edits and the file is stuck at a false fixpoint); and the
  obvious per-key repair — every applied key must vanish — would reject a
  round that applies one of two message-identical findings (the key is
  still present after). The
  gate becomes a **multiset decrement over the keys the round applied**:
  for every `(code, message)` key with `applied(key) > 0` — the
  `FindingKey`s of the sole candidates whose outcome was `Ok`, projected to
  `(code, message)` — `count_after(key) <= count_before(key) − applied(key)`;
  keys the round did not apply are unconstrained (a revealed finding
  normally lands on a key the round did not apply — the probe's NML2060 has
  `count_before = 0`; a gate over *every* key would reject the reveal it
  exists to accept, and a gate over keys present before would reject a
  repair that reveals more instances of an existing key). One applied
  fix's reveal CAN land on another applied fix's key — NML2060's
  equal-value message names the field, not the block (`layers.rs:4278-4283`),
  so deleting one dependent's restatement while an NML2077 ref deletion
  un-suppresses an identical-message NML2060 elsewhere fails the decrement
  for a round today's fixer converges on — so a failed gate **retries the
  round as the first APPLIED sole candidate alone** — the first in
  suggestion-span order whose outcome was `Ok`; a refused candidate
  contributed no edits and cannot have failed the gate — before the
  fixpoint is declared: a singleton that passes lands and the next round
  re-derives the rest; a singleton that still fails is a genuinely moved
  finding and reverts — visible as "not auto-fixable", and the
  resolver-bug catcher A.5 relies on. The round budget scales with the
  file — one round per initial finding plus reveal headroom, clamped to
  [8, 64]. Plain same-message findings land together (the decrement is
  per key); rounds collide when applied NML2077 repairs un-suppress
  same-message NML2060s the round also applied, and each colliding
  round lands one candidate — a fixed budget of eight stalled a fully
  fixable mixed file (seven restating dependents beside seven
  suppressed pairs) mid-run. A round that resolves to zero edits
  ends the loop the same way, its refusals printed once. And the parse
  layer did not regress (`after.parse_clean || !before.parse_clean`). It still rejects the
  injection class (a parse error appears) and does not change A.4's
  conclusion (the CR deletion is removed, not gated). The module
  doc's "re-check and revert … strictly improve" bullet (`fix.rs:19-24`)
  states the new rule.
- **The editor** (`crates/nml-lsp/src/server.rs`, code actions). The server
  never publishes: diagnostics are **pulled** (`textDocument/diagnostic`,
  `server.rs:4519`) and cached by exact text and registry generation
  (`:815-836`), with an `Unchanged` result-id hashed over the items
  including `data` (`:1376-1383`). So staleness is settled by **membership**,
  not by a version: `code_action` offers an action only for a client
  diagnostic whose `data` equals a member's `data` in
  `cached_diagnostics(&uri)` for the current text — `data` is the one
  field the LSP spec preserves verbatim between a report and
  `textDocument/codeAction` (stated for publish; the codeAction context
  carries the same `Diagnostic` shape for pull), it need not be unique (the
  `.shared` pair shares one `data` — equal `data` on the current text
  yields the identical action, so which member matched is immaterial), the action consumes only `data` plus the
  current text, and `range`/`message` derive from the same analysis while
  adding only what a client may re-encode (`related_information` URIs).
  No wire field, no new state, and it also refuses a suggestion made stale
  by a registry rebuild without a buffer edit, which a document version
  cannot see. The action then parses the member's `data.suggestions`
  (`from_wire_name`; an unknown kind is no action) and, **for each
  suggestion separately**, calls the resolver on the current text with
  that one suggestion (a singleton batch: N did-you-mean alternatives share
  one span and are N actions, RFC 0015's rule — `Overlap` is a
  batch-applier verdict and never fires here; a refusal is no action),
  emits one `TextEdit` per splice inside one `WorkspaceEdit`, and titles a
  deletion with `Deleted::title` (an empty verbatim `Fix`, the
  `NumberTrailingDot` removal, is titled `Remove` rather than today's
  ``Apply fix: `` ``). Deletions are **not** marked
  `is_preferred`: a preferred action is what editors auto-apply without a
  menu, and a structural removal is not a spelling repair (`is_preferred`
  stays reserved for a singleton did-you-mean, `server.rs:5350`). The
  `MAX_SUGGESTION_ACTIONS` cap keeps counting suggestions. The widening call
  (`server.rs:5331`) is deleted.

### A.4 NML0016

The out-of-string deletion suggestion is **removed** (`error.rs:422` →
`None`; `source_policy.rs` only constructs the kind and does not change);
the in-string fix stays the `\r` escape. A bare CR in token position is
reported without a machine fix. The one value-preserving repair for an
old-Mac file — CR→LF (canonical, since `nml fmt` normalizes CRLF→LF, and
harmless on a mid-line stray, where the re-check gate rejects it) — has a
**line break as its replacement**: exactly the control character the shared
injection guard refuses by construction, and a per-producer carve-out would
hole the one guard every applier now shares. No count gate catches the
deletion's corruption generically (8 → 6). The index entry's first
paragraph and `**Fix:**` trailer are reworded (its hover summary changes;
its fence and code list are unchanged — the harness runs `check`, never
`fix`, and the expectations already prove the file does not parse). Tests:
the `cst/mod.rs` source-policy test's "(fix: remove)" assertion flips
(`source_policy_flows_through_parse_and_supersedes_generic_errors`, `:2404`); a new CLI pin
(CR-terminated file → zero edits, byte-identical);
`fix_escapes_a_bare_cr_inside_a_string…` stays green.

### A.5 The canonicality property

For every fixture under `tests/fixtures/**` and `docs/**` that is fmt-clean
(`format_source(src) == src`; today 32 of 69, two of which carry an
applicable edit — `layers/linearization-contradiction.nml`,
`invalid/unknown-mixin.model.nml`), `nml fix` on a copy of the fixture's
**directory** (its siblings are its schema; `--schema` is that directory)
leaves it fmt-clean and a second run applies zero edits. The test lives in
`tests/integration/cli_tests.rs` and runs in under a second. This is the
fix analogue of RFC 0019 plan item 8 (compose idempotence). It holds by
construction: deletions are token-exact (A.2); did-you-mean and mechanical
fixes are same-token rewrites at non-aligned positions; the two aligned
constructs (routing-arm runs, `formatter.rs:286`; `oneof` arms, `:223`)
have no validation-layer producer, and the parse-layer fixes that touch arm
tokens act only on files `format_source` refuses. It is a ratchet for the
day a producer targets an aligned construct. A "format after the round when
the original was canonical" fallback is **rejected**: it would mask
resolver bugs behind the formatter and couple two verbs.

## Part B — The head rule

**Rule.** *A composed entry carries the span, the name identifier and the
provenance row of the head of its surviving group — the layer whose
contribution the composed body IS: the base when nothing switched; the
switching layer after an accepted switch; unchanged by a pin (a pin names,
it does not displace) or by a rejection. Its position in the body stays the
base slot (replace in place); a union annotation keeps the establishing or
pinning identifier.* This extends RFC 0019's receiver rule (`0019:431-434`
— which layer's *body* receives `with_entries`; already honoured at
`layers.rs:4039-4044`, where `merge_model_bodies` picks the first non-empty
body of the surviving group) from the body to the composed entry, at the
three routes that still anchor the entry at the base:

| Route | Today | Probe | After |
|---|---|---|---|
| `merge_overlay`, oneof-typed field (`layers.rs:4365-4380`) | span/name/record at `nested[0]` | NML2007 at the base's `cfg:` (16:5) after a switch | `nested[head]` |
| `merge_union` → `merge_variant_group` → `merge_oneof_bodies`: an arm switch inside a joined oneof **variant** | `est = replay.group.first()` (`:4534-4600`) — right for a union `as` switch (`replay_union` clears the group on `Switch`, `:4676-4680`), blind to the arm switch beneath | NML2007 at the base's `slot as oo:` (22:5) | head threaded through `merge_variant_group`; the annotation stays on `est` |
| `merge_items` identity groups (`:5489-5515` Named, `:5516-5640` Shorthand) and `merge_union_bodies` inside items | `existing_item.span`, the base's name identifier, `resolved[pos].2 = existing_layer` (recorded at `:5685-5689`) | NML2007 at the base's `- w:` (20:11); NML2085's note at the base item after a mid-layer switch | head-selected span/name/owner |

NML2007's anchor is the composed block's *name* span (`nml-validate/src/schema.rs:1789`
passes `header_span: Some(name.span)`), so the head rule needs only the
entry's name and span to follow the head.

**Shape.** `struct Composed { body: Body, head: usize }`, returned by
`merge_oneof_bodies`, `merge_union_bodies` and `merge_variant_group`;
`merge_model_bodies` keeps `-> Body`. The head is derived from the one
survivorship rule, not tracked by a second accumulator: `merge_oneof_bodies`
runs the trace for diagnostics only (rejections → NML2060; the
believed-unreachable verdicts → NML2086, as today), then
`let survivors = surviving_indexes(trace); let head = survivors.first().copied().unwrap_or(0)`
(an empty survivor set is unreachable with non-empty layers — index 0 is
always Switch or Join, `:1990-2014` — and debug-asserted); the effective
discriminator is the first stated one among the survivors (after the last
accepted switch every survivor omits or restates it), the arm model follows,
the survivors are stripped and merged, and the canonical entry **when one
exists** plus the Part C passthroughs are prepended. Equivalence with
today's accumulator holds on every trace shape (`[Join a, Switch b, Join ∅,
Join b]` → survivors `[1,2,3]`, effective `b`; `[Join ∅, Join a]` → `a`;
`[Join a, Rejected b, Join ∅]` → `a`; `[Switch a, Join ∅, Join a]` → `a`;
none stated → the default, no canonical entry). `merge_variant_group`
returns `Composed` (Model/None arms → head 0); `merge_union` builds a
`members` index beside `group` with the same body filter so
`contributions[members[head]]` is exact (a `NestedBlock` by construction,
with the existing fail-safe falling back to `est`); `merge_union_bodies`
maps the head through `replay.group` for a named establishment,
`replay.group[0]` for an ambiguous one, `replay.structural[0]` for a
structural one (a model-less deep merge — no single layer *is* the body),
and `0` for the zero-item path (its named-establishment fallback merges an
empty group and panics today, `:4040` — pre-existing, closed by the same
change); the `merge_items` closure and both identity arms select span, name
identifier (or shorthand value token) and owner layer from
`[existing, item][head]`, so the `record` at `:5685-5689` follows
automatically. Callers: `merge_root` (`.body` on the oneof arm),
`merge_overlay`, `merge_union`, `merge_variant_group`, `merge_union_bodies`,
the items closure, one direct-`Merger` test.

**Consequences.** (1) The finding-loss bug closes: two switching dependents
get two findings at two `cfg:` sites (field and item scope); a base→mid-
switch→top-join chain reports once at mid's `cfg:` (top's composed finding
has the same head and collapses onto its true home). (2) Provenance follows
the head: the table's contract is "which layer's assignment produced each
effective entry" (`layers.rs:3586-3590`), and after a switch the base
produced nothing at that position; no CLI or LSP consumer reads origins
today. (3) No new related note: NML2007 is validator-emitted over a plain
`File` with no layer provenance to attach, the switching block's own span
plus the message's `defined in model 'armb'` is self-explanatory, and the
union route already behaves this way; cross-file `uses` is unreachable
today (`compose_file` builds a single-file `InstanceIndex`, `:3894`), so the
checked-file coordinates are always right. (4) NML2085's item-scope
`established here` lands on the establishing item. The comments on
`item_scope_notes_point_at_the_base_item` and
`three_layer_identity_item_provenance_is_the_base_span` ("the BASE item's
position") are reworded to "the head's — the base when nothing switched";
both stay green (nothing switches in them) and each gains a switching twin.

## Part C — Non-string discriminators

**`ArmDecision::Invalid` is rejected**, on two grounded failures: a
both-invalid stack yields an empty group and hits `merge_model_bodies`'
`layers.last().expect("stack is non-empty")` (`layers.rs:4043` — sound
today only because index 0 is always Switch or Join, `:1990-2014`); and an
invalid layer's *other* fields validate today (`a = "9"` overlays; `zzz`
draws NML2001) and would be dropped silently — against RFC 0019's "skipped
contributions come with a diagnostic" and the NML2085 doctrine that silence
is data loss. RFC 0019's accumulator has three cases keyed on *stating* the
discriminator (`0019:718-727`: omit → inherit); the engine's predicate
defines stating as a string property (`layers.rs:3058-3060`), so a
non-string reads as *omits* → Join — today's behaviour, which stays.

**Design.** Two predicates with two jobs. `is_discriminator_entry`
(string-valued, `layers.rs:3061-3064`) remains the **selection** predicate
for the fold; a new `is_discriminator_named` (any `Property` named like the
discriminator) is the **strip** predicate used by `without_discriminator`
(`:3090-3098`) and by the plan's hidden-discriminator filter (`:1864-1869`)
— the only two strip sites — so no `kind` group ever forms in
`merge_model_bodies` and the plan gathers none (parity by construction, and
the validator's own reading: `variant_body` already filters every
`Property` named like the discriminator, any value,
`crates/nml-validate/src/schema.rs:2208-2216`). `merge_oneof_bodies`
collects the non-string discriminator entries of the **surviving** group's
raw bodies, in layer order, and places them after the canonical entry — or
first, when no survivor states a string discriminator (the both-invalid row)
— without a provenance row (the table records effective entries only). A
later *string* restatement is neither canonical nor passed through —
first-wins, exactly as the validator reads it (`schema.rs:2098-2101`). The
validator reports the first discriminator-named entry through its
first-entry check (`schema.rs:2160-2172`, which returns early) and every
later non-string one through the `.skip(1)` loop (`:2096-2129`), each at its
author's span. Outcomes (probed today / after):

| Scenario | Today | After |
|---|---|---|
| base invalid, dependent switches | NML2042 at the base (raw) + NML2007 at the base's `cfg:` | the same NML2042 (the base is displaced, not passed through; raw still reports); NML2007 at the **switching** layer (Part B) |
| base invalid, dependent omits | one NML2042 (composed + raw dedup) | identical |
| dependent invalid, base valid | NML2042 at the dependent + NML2001 for its unknown field | identical |
| both invalid | two NML2042 (`kind = 6` overlays `kind = 5` in the composed view; the base's is reported raw) | two NML2042; both entries pass through (`kind = 5, kind = 6`, at the front), the base's collapses onto its raw home |
| oneof root instance `kind = 5` | NML2042 via the first-entry check | identical |
| oneof variant of a union | NML2042 | identical |
| the NML2054 shape (an arm field named like the discriminator), base supplies the field's body `kind as va:`, dependent `kind = 5` | NML2085 at the dependent ("a whole-value spelling never switches"), no NML2042 | NML2042 at the dependent; no NML2085 (the union field never sees the entry — the truthful finding: NML2054 says the field can never be set); the validator's first-entry check returns early, so the arm body is not otherwise validated — an error-severity finding exists either way |
| non-string restated (`kind = 5` over `kind = 5`) | NML2084 dead-delta at the dependent + two NML2042 | two NML2042; NML2084 no longer fires (nothing overlays) |

No new verdict; one verdict moves and one stops firing, as tabled. The
existing pins survive: `non_string_discriminator_survives_to_validation`
(some `kind` entry equals the non-string value) and the CLI count of one
NML2042.

**Invariant.** `layers.rs:3594-3600` already carries E12's carve-out ("one
VALUE entry per field name … a declaration may sit ahead"); this RFC extends
it: *one VALUE entry per field name; validator-facing passthroughs sit
beside it — a declaration ahead of its value, non-string discriminator
entries after the canonical one (first, when none exists) — and every
passthrough of the second kind is always accompanied by an error-severity
finding.* That last clause is a requirement on every future artifact
consumer (`nml resolve`, the editor's resolved peek, RFC 0019 plan item 5 —
none exists today; the composed body's only consumer is validation): no
artifact of record while an error-severity finding exists. Erratum E16 to
RFC 0019:274 and a layers sentence in the NML2042 index entry.

## Part D — Seal-hit identity and the NML2060 count

RFC 0019 counts **fields** (`0019:700-702`); the index (`error-index.md:1486-1491`)
and the engine (`seals.len() - 1`, `layers.rs:2845`) count assignments.
Conform to the RFC; keep the notes per assignment.

```rust
/// One step of a seal's path relative to the judged position.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Seg { Field(String), Item(ItemKey), Arm(ArmSelector) }   // ArmSelector gains Eq + Hash

/// A seal's identity — hashed and compared structurally, rendered
/// non-disclosingly by the ONE rule.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
struct FieldIdentity(Vec<Seg>);
impl FieldIdentity {
    fn child(&self, seg: Seg) -> Self;
    /// `secret`, `[w].secret`, `nest.secret`: a dot for fields, brackets
    /// for items (`ItemKey::segment`), nothing for arms; `at("slot")`
    /// joins the position — the ONE join, replacing the emitter's
    /// bracket special case (`layers.rs:2839-2843`).
    fn at(&self, position: &str) -> String;
}
/// The one redaction point: the type that holds a token prints its
/// `segment()` (a name or a TYPE name), never the token.
impl fmt::Debug for ItemKey { … }
impl PartialEq for ItemKey { fn eq(&self, o: &Self) -> bool { self.same(o) } }
impl Eq for ItemKey {}
impl Hash for ItemKey { /* discriminant tag, then the `token_prehash` arms */ }

#[derive(Clone)]
struct SealHit<'a> { id: FieldIdentity, span: Span, layer: InstanceId<'a> }
struct SealSink<'a> { hits: Vec<SealHit<'a>>, seen: HashSet<(FieldIdentity, &'a str, usize, usize)> }
```

- No parallel `shown` string: the path was built in three places with two
  join rules; the identity renders itself. Four sites insert a segment:
  `displaced_list_seals` (`:2682`) and `scan_list_items` (`:6158`) push
  `Seg::Item(ItemKey)`; `scan_arm_bodies` (`:1542`) and `merge_arm_set`
  (`:5133`) push `Seg::Arm(ArmSelector)` per inline arm body — two arms'
  `token` seals are two **fields** (probed: `'route.token' (and 1 more)`
  today) and render unchanged as `route.token`. `seal_scan_body` and every
  scan signature take `at: &FieldIdentity` instead of `path: &str`.
- Hash coherence with `same` follows the in-tree precedent `token_prehash`
  (`layers.rs:6062-6086`: numbers by the normalized pair, durations by
  nanos, money by amount and currency, exotic values by type name), pinned
  beside its test. No textual serialization of a value is ever created —
  `Value` has no `Display`, and `segment()`'s type-name rendering stays the
  only thing printed.
- **Non-disclosure is enforced at the token holder**: `ItemKey` derives
  `Debug` today and prints the `Value`; the redacting `Debug` on `ItemKey`
  makes every derived `Debug` above it safe. RFC 0019 requirement 4
  ("scalar-keyed identities are not exempt: their tokens are values",
  `0019:1076-1082`) holds by construction.
- **The sink key keeps the identity** — `(FieldIdentity, file, start, end)`,
  today's `(path, file, span)` made structural — because one span IS two
  fields when a list-level `.shared` line is distributed into several items:
  the synthesized property in each item carries `sp.name.span`
  (`crates/nml-core/src/resolve.rs`, `merge_shared_into_body`); probed,
  today's message reads `'slot[w].secret' (and 1 more)` with two notes at
  one span. Fields = distinct identities; assignments = distinct
  `(file, start, end)` among the hits; notes = one per distinct assignment,
  the first four (`RELATED_SEALS`; RFC 0010's hover cap of three is a
  different axis — diagnostics per hover, not notes per diagnostic).
- **Message** (one owner; house form: one lead, one action tail):
  `'slot[string].secret'` then `(and N more field[s])`; `(M assignments)`
  when assignments exceed fields — `(and N more fields; M assignments)`, or
  `(M assignments)` for one field; nothing for one field and one
  assignment. Probed: two scalar-keyed items → two fields (both render
  `slot[string].secret`); two files assigning one field → `(2 assignments)`;
  one `.shared` line over two items → `(and 1 more field)` with one note.
  `.first()` remains the lowest-then-document hit (the RFC's related-span
  invariant).

**`Related.source`.** RFC 0019 plan item 2 (`0019:1529-1531`) specifies
`source: Option<String>` and `with_related_in`; the engine prints
`sealed here (in b.nml)` instead (`layers.rs:2870-2878`). Since Part D
rewrites exactly these notes, it lands the field: `Related { span, message,
source: Option<String> }` (the same vocabulary as `Diagnostic.source`, a
path; the only constructor is `with_related`, `diagnostic.rs:725`),
`Diagnostic::with_related_in(span, message, source)`, and
`Diagnostic::related_source(&self, rel) -> Option<&str>` (the field, else
the diagnostic's own) so both renderers share the inheritance rule; every
compose note sets it. **Both renderers must locate a note in its own file**:
today the CLI maps every note through the checked file's `SourceMap`
(`nml-cli/src/main.rs:58-67`) and the LSP through one `line_index`
(`nml-lsp/src/diagnostics.rs:395-462`) — landing the field without this
would print the right file with a wrong range. The CLI keeps a lazy map per
path beside the checked file's (an unreadable path renders
`<src>: note: <message> (bytes <start>..<end>)`); the LSP's `compute` and
`push_diagnostic` take `locate: &dyn Fn(&str) -> Option<(Url, String)>`
(the callee builds the `LineIndex`; the server closes over its document
map) and fall back to the diagnostic's own location with the file named in
the message when it cannot. Both consumers compose single-file today, so
the cross-file rendering is pinned at the renderer level with a synthetic
diagnostic. The parenthetical leaves the message.

Pins (file:line as of the seal — the shipped tree's tests are the
authority, and the battery grew to nine assertions): the eight
`(and 1 more)` assertions — seven become
`(and 1 more field)` (`layers.rs:7842, 8434, 11379, 12282, 12303, 13186`;
`cli_tests.rs:1676`), one becomes `(2 assignments)` (`layers.rs:13366`, two
files, one field); the index example → `(and 3 more fields)`; the `ItemKey`
coherence test beside `token_prehash`'s; two files → one field, two
assignments, two notes with two sources rendered as `b.nml:2:9: note:
sealed here`; the arm-set case → two fields; scalar-keyed items → two
fields; the `.shared` case → two fields, one assignment, one note; a
scalar-keyed path's `Debug` never contains the token.

## Part F — RFCs as tracked design records

**Evidence.** `docs/rfcs/` is git-ignored (`.gitignore:21-22`, "RFC drafts
(local working docs)", added in an unrelated 2026-06 commit; never
tracked). The docs gate walks `docs/**/*.md` (`scripts/docs_test.py:78-87`)
and **executes** RFC 0019's `nml check` fence locally (0019:28); CI
(`.github/workflows/ci.yml:26-40`, plain checkout) executes zero RFC fences
and prints different unverified counts — while `justfile:78-81` claims the
recipe matches CI, and RFC 0019 itself instructs retagging self-contained
fences to `check` "so they become executed regression tests"
(`0019:1452-1461`). The tracked error index cites "RFC 0019 errata E7"; code
cites E12 (`layers.rs:4060`); CHANGELOG cites E12 and E13;
`CONTRIBUTING.md:47-48` links the RFC README (broken on GitHub; the link
check is page-scoped and never reaches CONTRIBUTING, `docs_test.py:945-962`).
Extracting errata alone would leave the executed fence local-only and every
citation dangling.

**Decision.** Track `docs/rfcs/` — remove the ignore and add the files **in
the same commit** (the link check judges `git ls-files`), keep the ban-list
exemption for historical syntax, add `CONTRIBUTING.md` to the link check,
correct the gate docstring (`docs_test.py:52`, "historical records" →
ban-exempt design records), set RFC 0019's status to *Slice 1 implemented*
(the README's precedent partial form is 0010's "Tiers 1–2 implemented"; the
promised verbs `resolve`/`binding`/`diff --resolve-layers` and the
language-guide chapter are absent, `import` does not parse) in both the
README row and 0019's own status line, and drop the RFC's self-contradictory
instruction to retag the policy-denial example (it needs a manifest and
cannot run single-file).

**Sanitization (resolved).** Two identifiers in the 0019/0020 examples name
an external core-banking vendor and its product that tape integrates with
(`tape/docs/rfcs/0001`, `0003`; no workspace directory or crate carries
either name) — a stronger reason to sanitize than "consumer identifiers".
Rule: **first-party names are allowed; anything else is replaced by the
documentation's fictional vocabulary** (`skylight`: 122 mentions in the
tutorial and guide Markdown, 139 in their `.nml` examples), where
first-party means a directory of this workspace (`nml`, `nudge` and
`nudge-*`, `tape`, `block`, `homestar`, `platform`, `sigil`, `regent`,
`knowledge-store`, `webtransfer`). The product name occurs in 3 RFC files
(5 lines), the vendor name in 2 (15 lines); `tape` in 5 files (45 lines)
and `nudge` in 18 (114) — counts excluding this RFC — are first-party and
stay. The rule is enforced, not remembered:
`scripts/docs_test.py` gains a `RESERVED_NAMES` regex (word-boundary,
case-insensitive) — **the only place in the repository that spells the two
names after this lands; no document, this RFC included, may quote them** —
and a `reserved_names_in(text)` helper applied to every doc's full text
beside the prose ban **without** the exemption guard (`:983-993`) and to
every `.nml` under the example and tutorial dirs beside `banned_tokens_in`
(`:442-444`, `:495-496`), reporting into the same failures list; a
self-test asserts `reserved_names_in` on a seeded string (the gate walks
fixed repo globs, so a temp file is never discovered); the summary line
prints "N docs + M example files scanned for reserved names" so local and
CI runs are comparable. RFC 0016:450's common-noun use of the product's name
("the ergonomics … below") is reworded ("linchpin") in the same change, so
the rule stays total. Prose that describes a first-party origin ("Tape's
multi-tenant flow composition") stays.

## Diagnostics and error-index changes

| Code | Change |
|---|---|
| NML0016 | out-of-string deletion fix removed; first paragraph and `**Fix:**` trailer reworded ("a bare CR in token position is reported without a machine fix; inside a string the fix is the `\r` escape"); the fence and its code list are unchanged — its hover summary changes |
| NML2042 | layers sentence: a non-string discriminator in any layer is reported on the composed view at its author's span; the layer's other fields still compose; at the NML2054 shape it replaces NML2085 |
| NML2060 | count = distinct sealed fields — `(and N more field[s])`, `(M assignments)` when they exceed the fields; one `sealed here` note per assignment (first four), the first the lowest-then-document hit; notes carry `Related.source`; example → `'slot[w].secret' (and 3 more fields)` |
| NML2062 | gains the "delete the clause" fix, stated in the `**Fix:**` trailer (structural) |
| NML2077 | the deletion is structural (unchanged wording) |

Hover summaries (first paragraph) are unchanged for every entry except
NML0016.

## Documentation

- The five index entries above (`crates/nml-core/assets/error-index.md`).
- `CHANGELOG.md`: the whole-line rule and "one owner … `widen_deletion_to_line`"
  narrative (≈209-223) and the in-string CR sentence (≈231-232) are
  superseded by the resolver; the "machine-fixable deletion" line (≈362).
- `crates/nml-core/src/cst/edit.rs` module doc (two operations; the trivia
  rule vs RFC 0004 §4.3; refusal ownership); `nml-cli/src/fix.rs`
  (`sole_candidates`'s doc — the renamed `sole_candidate_edits` — and the
  module doc's "re-check and revert" bullet for the new gate); `diagnostic.rs:795-799` and `error.rs:571-574`
  (the retired empty-replacement convention).
- `scripts/docs_test.py:52` docstring; `CONTRIBUTING.md` (linked into the
  link check); `.gitignore:21-22`; `docs/rfcs/README.md` (0019's row);
  RFC 0019's status line and errata E15–E17, E19; RFC 0016:450.

## Security considerations

- One injection guard for every applier (A.2/A.3); structural deletions
  inject nothing and remove only tokens the tree assigns to the node;
  stale suggestions fail closed twice (cache membership at the editor,
  `NoNodeAt` structurally); a parse-dirty source refuses every structural
  edit and no verbatim one.
- Deletions are never preferred actions, so no editor auto-applies a
  structural removal; refusals are printed by `nml fix`, never silent.
- `nml fix` no longer has a structure-changing textual fix (A.4), and its
  round gate keys on the applied diagnostics rather than a count.
- Non-disclosure of scalar item keys is enforced at the token holder
  (Part D): hashed identity, redacting `Debug` on `ItemKey`, type-name
  rendering — RFC 0019 requirement 4.
- `Related.source` renders a note's own file with its own line index; a
  path is never printed for a same-file note.

## Compatibility

- `SuggestionKind` gains `Delete`, `wire_name()`, `from_wire_name()` and
  `#[non_exhaustive]` (no external matcher exists). The diagnostics wire
  gains `"delete"`; consumers that ignore unknown kinds are unaffected.
- New public items: `cst::edit::{resolve_suggestions, Resolved, Deleted,
  SuggestionError}`, `Diagnostic::{with_deletion, with_related_in,
  related_source}`, `ast::BlockDecl::uses_span` (`BlockDecl` derives
  `Serialize`, so `nml parse` JSON gains the field). Removed:
  `cst::edit::widen_deletion_to_line` (added in the unreleased 0.1.0 tree;
  never shipped). `ArmSelector` gains `Eq + Hash`.
- `Related` gains `source: Option<String>` (a public struct; RFC 0019 called
  this "breaking-but-necessary"). Renderers print the note's own location
  from the field; the parenthetical text leaves the message. nml-lsp's
  `compute` gains the `locate` parameter.
- `ProvenanceTable` rows for switched blocks and merged items attribute to
  the head layer (no consumer today).
- NML2085's item-scope note moves to the establishing item after a switch.

## Test plan

- Part A: resolver rows — first/middle/last entry; trailing comment;
  own-line comment above; deferred outer comment after a nested block;
  CRLF; EOF without newline; block-form value; nested-block target; two
  entries on one line (refused, `NotLineExclusive`); emptied `uses` block
  (colon dropped); emptied plain/nested/modifier/list/arm bodies (colon
  kept); a target inside a `.shared` body (refused, `SharedDistribution` —
  partial, full, and block-form NML2060 alike; subsumption by a
  containing deletion still applies); two targets emptying one body (one colon
  splice); entry inside a deleted block (subsumed); a later overlapping
  suggestion (refused `Overlap`, applied next round); stale span
  (`NoNodeAt`); a `.shared`-distributed NML2060 restatement (`NoNodeAt`,
  printed); parse-dirty source (every `Delete` refused, a verbatim fix
  applied); `\n` in a verbatim replacement (refused — the real guard test);
  clause deletion with and without a body; first-ref, non-first-ref and
  only-ref deletion (the last restores the colon); `Fix("")` stays a
  verbatim removal (`NumberTrailingDot`); `Rendered` prints nothing for
  `Delete`; the canonicality property over the fixture directories; the
  reveal case (NML2077 then NML2060) applies; two message-identical findings
  with one applied (gate accepts); an applied fix whose diagnostic survives
  (gate reverts); the compound reveal (an applied NML2060 plus an NML2077
  deletion revealing a same-message NML2060: the singleton retry lands the
  NML2077 and the file converges); a repaired ref revealing more instances
  of an existing key (gate accepts); a refused candidate ahead of a
  failing compound round (the retry skips it and lands the first applied
  fix); CR-terminated file → zero edits,
  byte-identical; LSP:
  `from_wire_name(k.wire_name()) == Some(k)` for every variant,
  cache-membership refusal (edited buffer → no
  action; registry rebuild → no action), N did-you-mean alternatives → N
  actions, titles including `Remove` for the empty verbatim `Fix`, no
  `is_preferred`, one
  `WorkspaceEdit` with two `TextEdit`s, unknown kind → no action;
  `Deleted::title` total.
- Part B: the three probes as pins (the composed block's name span == the
  switching layer's `cfg` name token; NML2007 at the switching layer); the
  two-dependents finding-loss regression at field and item scope (CLI);
  the chain (one finding at mid's `cfg:`); item head (name span and `xs[w]`
  origin = top); NML2085's note at mid's `- w as ub:`; the two reworded
  tests plus their switching twins.
- Part C: both-invalid composes with `kind = 5, kind = 6` first and two
  NML2042; the NML2054-shape verdict move (NML2042, no NML2085); the
  non-string restatement (two NML2042, no NML2084); exactly one string
  `kind` entry; passthroughs have no provenance row; the existing pins kept.
- Part D: `ItemKey` hash coherence beside `token_prehash`'s test; the eight
  message pins as tabled; two files → one field, two assignments, two
  sources rendered (renderer-level, synthetic diagnostic); the arm-set case
  → two fields; scalar-keyed items → two fields; the `.shared` case → two
  fields, one assignment, one note; `Debug` redaction; the CLI's
  unreadable-path fallback; the LSP fallback location.
- Part F: `reserved_names_in` on a seeded string; the summary line's
  counts; the gate's local/CI counts equal.

## Sequencing

1. A.4 (data corruption; smallest). 2. Part B, then Part C (finding loss;
conformance). 3. Part A. 4. Part D with `Related.source`. 5. Part F with
E15–E17 and E19. Commit. RFC 0025 follows as its own series; E18 is issued
there.

## Errata to RFC 0019

- E15 — the head rule extends the receiver rule from the body to the
  composed entry (span, name, provenance) at the three routes; NML2085's
  item-scope note follows the establishing item.
- E16 — non-string discriminators: strip by name, pass through after the
  canonical entry (first when none exists); at the NML2054 shape the union
  field never sees a non-string entry (NML2042 replaces NML2085); NML2084 no
  longer fires on a non-string restatement; the invariant restated with the
  artifact-of-record requirement.
- E17 — NML2060 counts fields with the assignment clarifier; one note per
  assignment; `Related.source` with the own-file location requirement.
- E19 — RFC 0019's status "Slice 1 implemented"; the policy-denial retag
  instruction withdrawn.

## Decisions

Three questions the design review left open, resolved here:

- **Sanitization scope** — a rule and a gate (Part F), not a one-off pass;
  the external vendor names are the reason, and the gate is the only place
  that spells them.
- **The M-class nested-loser diff** — moved to RFC 0025, where the
  discarded-body vocabulary rule is normative ("its own readings at every
  level"); no merge-side decision log.
- **NML2062's promised fix** — delivered by the structural kind (A.1/A.2),
  which also absorbs NML2077's hand-stitched deletion; not a verbatim
  producer.
