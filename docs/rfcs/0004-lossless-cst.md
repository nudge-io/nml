# RFC 0004 — Lossless Concrete Syntax Tree (resilient parsing)

- **Status:** **Implemented.** The CST is the unconditional production parse path for every
  crate (`lib::parse` == `cst::parse_to_ast`; no feature flag). The legacy recursive-descent
  parser survives only in nml-core's own tests, pending removal. (Historical: P0 spike
  validated the approach; P1+ then shipped in full.)
- **Builds on:** [RFC 0001 — Schema-Driven Defaulting](./0001-schema-driven-defaulting.md),
  [RFC 0002 — Shared Body-Aware Dispatch / Workflow Migration](./0002-visitor-unification-oneof-defaults-workflow-migration.md),
  [RFC 0003 — Schema-Driven Field Completion](./0003-schema-driven-field-completion.md) (all complete)
- **Crates touched:** `nml-core` (lexer, parser, ast, de, defaults, resolve, model_extract,
  schema_index), `nml-validate`, `nml-fmt`, `nml-lsp`, `nudge`
- **Supersedes:** the "error-tolerant parsing" future item flagged in RFC 0003 §4.3/§9, and
  obsoletes the all-or-nothing parser, the manual per-node span plumbing, and the out-of-band
  comment sidecar.

## 1. Summary

The parser is a recursive descent where every level returns `NmlResult` and **aborts the
whole file on the first syntax error**; it produces a typed, owned AST (`File`, `Body`,
`BodyEntry`, …) with **manually-tracked spans**, and **comments are sidelined out of the
token stream** (`Lexer::take_comments` → a `Vec<Comment>` the AST never sees). Every consumer
touches that owned AST across `nml-validate` / `nml-fmt` / `nml-lsp` / `nudge` — a mix of
serde-mediated reads (which ride on `#[derive(Deserialize)]`) and raw structural walks; only
the raw walks migrate (see §3, §8 step 4).

This RFC migrates parsing to a **lossless concrete syntax tree (CST)** — a `rowan` red/green
tree that (a) covers **100% of source bytes** including whitespace, comments, and *error
nodes*; (b) is built by a **resilient** parser that recovers from errors and always produces a
tree plus the full error list; and (c) is exposed to consumers through a thin **typed-wrapper
AST layer** (accessor views over untyped `SyntaxNode`s). The owned AST is removed.

The win is broad and structural, not a point fix: every IDE feature works on incomplete input
(the RFC 0003 limitation), diagnostics report *all* errors at once, spans become exact and
free, formatting becomes comment-faithful, and incremental reparse becomes possible. It also
*removes architectural debt*: immutability forces the `resolve` and `defaults` passes from
"rebuild the AST" rewrites into **read-time deserialize adapters**, a cleaner pipeline.

## 2. Motivation — what we gain

- **Error tolerance, everywhere.** Today all schema-aware LSP features open with
  `nml_core::parse(source).ok()?` and go dark on any syntax error — i.e. while you are typing.
  A resilient CST always yields a tree, so completion (RFC 0003), hover, go-to-definition,
  document symbols, and semantic tokens all work mid-edit. This is the recurring tax RFC 0003
  §4.3 named; the CST is the one fix that lifts *all* of them.
- **All-errors diagnostics.** The strict parser reports one error and bails — fix it, see the
  next, repeat (whack-a-mole), in the LSP *and* in `nudge`/CLI loads. The resilient parser
  collects every error in one pass.
- **Exact spans, for free — and less code.** `rowan` gives every node/token a precise byte
  range; the manual `span: Span` fields threaded through the parser and AST (and the off-by-one
  hazards they invite) are deleted.
- **Comment-faithful formatting (over a lossless tree).** Comments and whitespace live *in* the
  tree, so `nml-fmt` can preserve comment placement while normalizing layout — removing the
  `Comment` sidecar (`take_comments`) and the "comments are not part of the AST" caveat. (The
  *tree* is lossless/byte-faithful; the *formatter* normalizes whitespace but no longer drops
  comments.)
- **Incremental reparse (enabled).** `rowan`'s immutable, position-independent green tree
  supports structural sharing, so a later optimization can reparse only the changed subtree —
  the precondition for scaling the LSP to large files. Not built here; unblocked.
- **A cleaner transform pipeline (debt removed).** See §6 — `resolve` and `defaults` stop
  allocating intermediate ASTs and become read-time adapters.

## 3. Current state

- **All-or-nothing recursive descent.** `parser.rs` (~2,529 lines): `parse` →
  `parse_declaration` → `parse_body` → `parse_body_entry` → … each `NmlResult`, propagating the
  first error with `?`/`return Err`. `pub fn parse(source) -> NmlResult<File>` aborts the file.
- **Owned, manually-spanned AST.** `ast.rs` (~24 node types: `File`, `Declaration`,
  `BlockDecl`, `Body`, `BodyEntry`/`BodyEntryKind`, `Property`, `NestedBlock`, `Modifier`,
  `ListItem`/`ListItemKind`, `OneOfDecl`, …) with hand-tracked `Span` fields.
- **Comments sidelined.** The lexer collects comments into a `Vec<Comment>` via `take_comments`
  and the doc says *"comments are not part of the token stream or the AST"* — so the source is
  **not** losslessly represented today.
- **Schema-driven loading rewrites the AST.** `resolve` (`ValueResolver::resolve_body`) and
  `apply_defaults` each **produce a new `Body`** (rewrite passes) before `de.rs`'s serde
  `Deserializer` (4 impls: `BodyDeserializer`, `NestedBlockDeserializer`, `NamedItemDeserializer`,
  `ValueDeserializer`) reads the typed AST. RFC 0001 deliberately kept `de.rs` schema-agnostic;
  the schema-aware step (`apply_defaults`) runs *before* it.
- **Consumer access sites** (`nml-validate` 102, `nml-fmt` 43, `nml-lsp` 227, `nudge` ~1,200 —
  of which ~255 are already serde-mediated and ~946 are raw AST walks) read the owned AST
  directly (`block.name.name`, `body.entries`, `prop.value.value`, …). Only the **raw walks**
  migrate; the serde sites ride on the unchanged derives (§5, §8 step 4).

## 4. The CST design

### 4.1 `rowan` red/green tree + `SyntaxKind`

Adopt `rowan` (the library underpinning rust-analyzer; battle-tested, no proc-macros, no
grammar DSL). One `SyntaxKind` enum enumerates every node and token kind (declaration, block,
body, property, value, identifier, `=`, `:`, newline, **whitespace**, **comment**, **error**,
…). The **green tree** is immutable, deduplicated, and position-independent; the **red tree**
(`SyntaxNode`) overlays absolute offsets and parent pointers. Tokens carry their text; every
node/token has an exact range — replacing all manual span tracking.

### 4.2 Lossless lexer

The lexer emits a **flat token stream that includes trivia** — whitespace and comments become
real tokens (`SyntaxKind::Whitespace` / `Comment`) rather than being discarded or sidelined.
The `Comment`/`take_comments` sidecar is removed: comments are tree tokens, attached to the
adjacent node, so formatting and hover see them in place.

The lexer's existing `TokenKind` is unified into (or mapped 1:1 onto) `SyntaxKind` — there is
exactly one kind enum across lexer, parser, and tree (no parallel token/node taxonomies).

### 4.2.1 Indentation as structural tokens (the offside rule)

NML is **indentation-significant** (an offside-rule language: a block's extent is its indent).
`rowan` was designed for brace-delimited, free-whitespace languages, so this is the single
most important design decision in applying it here, and it is resolved explicitly:

- Plain **whitespace and comments are trivia** (invisible to the parser, kept in the tree for
  losslessness).
- **Layout is carried by dedicated structural tokens** the parser *does* consume:
  `SyntaxKind::{Newline, Indent, Dedent}`. The lexer runs an offside-rule layout pass (the
  classic Python-tokenizer algorithm: an indent stack; emit `Indent` when a logical line opens
  a deeper column, one `Dedent` per popped level when it closes) and emits these as **real,
  non-trivia tokens**. They still **cover their source text** (or are zero-width at the exact
  byte offset), so 100% byte coverage / losslessness is preserved.
- The parser opens a body on `Indent` and closes it on the matching `Dedent`; recovery
  (§4.3, §9) synchronizes on these tokens (`Dedent` / next `Newline` at the block's column)
  rather than trying to re-derive columns from trivia. **Trivia stays invisible to the parser;
  structure is explicit in the token stream** — the two roles of whitespace are cleanly split.
- **Layout is suppressed inside multi-line strings and bracketed groups.** Layout is computed
  only at the **top lexical level**. NML has triple-quoted multi-line strings (`"""…"""`, the
  template form RFC 0003 relies on), where newlines and leading whitespace are *content, not
  structure*: the whole literal is **one token spanning its lines**, and the layout pass emits
  **no** `Newline`/`Indent`/`Dedent` within it. The same suppression applies to any bracketed
  continuation that may span lines (e.g. a `{{ … }}` template expression). This is the standard
  offside-rule rule (Python's implicit line-joining inside brackets/strings) and is **load-bearing
  for NML specifically** — without it the layout pass would open/close bodies *inside* a string,
  breaking valid input, not just adversarial input.
- **The layout pass recovers, never panics.** An *inconsistent dedent* — a column matching no
  open level (the classic `IndentationError`, routine mid-edit) — must not abort the lexer: it
  emits an **error token + diagnostic and snaps to the nearest enclosing level**, keeping the
  token stream well-formed so the parser keeps recovering above it. Column comparison uses the
  lexer's existing column rule (tab/space handling already defined), so the layout pass adds no
  new whitespace semantics. This closes the "always produces a tree" guarantee at the *lexer*
  layer, not just the parser.

This split (trivia = lossless reproduction; layout tokens = structure) is what makes a
`rowan` tree correct for an indentation language; it is not an optimization that can be
deferred.

### 4.3 Resilient parser emitting tree events

The recursive-descent **structure is kept** (it encodes the grammar), but the parser does
**not** write directly into a `GreenNodeBuilder`. It follows the rust-analyzer architecture:
**parse over a trivia-stripped token view and emit a flat `Vec<Event>`**
(`Event::{Start(kind), Token, Finish}`, plus `forward_parent` for retroactive node-wrapping);
a **separate tree-builder** then merges the events with the raw token stream — **re-attaching
trivia (whitespace/comments) to the adjacent nodes** — to produce the green tree. This
indirection is deliberate and is the clean/SOTA factoring: the parser never has to skip or
place trivia (that lives entirely in the tree-builder), so the grammar functions stay
trivia-free, and `forward_parent` allows wrapping an already-parsed node in a parent discovered
later. Writing straight into the builder would force every grammar function to interleave
trivia handling — the exact mess this migration removes.

The **trivia-attachment policy** is fixed explicitly (it determines where `nml-fmt` prints a
comment and what hover-on-comment resolves to, so it is a spec point, not an afterthought): a
comment **on its own line** attaches as **leading** trivia of the *following* node; a
**same-line trailing** comment attaches to the *preceding* node. (This mirrors rust-analyzer's
attachment heuristic.)

On an unexpected token the parser **does not abort**: it opens a `SyntaxKind::Error` node,
**synchronizes** to a recovery point — NML's natural boundaries are *next declaration at
column 0* and the `Dedent` / next `Newline` at the current block column (§4.2.1) — records a
diagnostic, and continues. The tree therefore always covers the whole input.

```rust
// nml-core
/// Always produces a (best-effort) lossless tree plus every error. The real parser.
pub fn parse(source: &str) -> Parse;
pub struct Parse { green: GreenNode, errors: Vec<NmlError> }
impl Parse {
    pub fn syntax(&self) -> SyntaxNode;     // typed wrappers view this
    pub fn errors(&self) -> &[NmlError];
    /// Strict callers: the full tree, or *every* error (not just the first).
    pub fn ok(self) -> Result<SyntaxNode, Vec<NmlError>>;
}
```

Strict callers (`nudge` load, schema build — the ~492 `parse()` sites that require validity)
use `.ok()`; the bail-on-first-error behavior is **removed** in favor of "parse fully, reject
with **all** errors if any." Note `.ok()` returns `Vec<NmlError>`, not a single error: the
all-errors win of §2 must reach `nudge`/CLI loads too, not just the LSP — strictly more
capability (every error in one pass), same strict contract.

### 4.4 Typed wrapper AST layer

Consumers do **not** touch `SyntaxNode` directly. A thin typed layer gives each former AST node
a zero-cost accessor wrapper:

```rust
pub struct BlockDecl(SyntaxNode);
impl BlockDecl {
    pub fn keyword(&self) -> Option<SyntaxToken>;
    pub fn name(&self) -> Option<Ident>;
    pub fn body(&self) -> Option<Body>;
}
```

Accessors return `Option`/iterators (a CST may be incomplete) — which is *correct*: it forces
consumers to handle partial trees, exactly what error tolerance requires. This is the same
24-node surface as today's `ast.rs`, re-expressed as wrappers; the owned structs are deleted.

**Hand-written, not generated.** rust-analyzer generates its typed layer from an `ungrammar`
spec via `sourcegen`. At 24 node types the wrappers are hand-written: it avoids a codegen
toolchain and grammar-DSL dependency for a surface this small, and the wrappers are trivial
(child-by-kind lookups). This is a deliberate call — if the node surface grows substantially
later, revisit codegen then.

## 5. Hard core #1 — `de.rs` over the CST

`de.rs`'s four `Deserializer` impls read the owned `Body`/`Value`; they are reimplemented to
read `SyntaxNode`s (find child nodes by `SyntaxKind`, read token text). This is the heart of
**all** schema-driven config loading (`from_block` / `from_body_defaulted`, every nudge config
and workflow), so it is the highest-stakes single change — but it is bounded to one file with a
well-defined trait contract, and the existing deserialize tests pin its behavior exactly.

The serde data model is unchanged (consumers' `#[derive(Deserialize)]` structs are untouched);
only the `Deserializer`'s *source* changes from owned AST to CST. `de.rs` stays
**schema-agnostic** (RFC 0001's separation is preserved — see §6).

## 6. Hard core #2 — `resolve` + `defaults` become read-time adapters (debt removed)

Today `ValueResolver::resolve_body` and `apply_defaults` each **rebuild a `Body`** before
deserialization. A green tree is **immutable** — you cannot rewrite it — so these stop being
rewrite passes and become **deserialize-time adapters**, which is cleaner and removes the
intermediate-AST allocations:

- **Resolve-on-read.** The value deserializer resolves `$ENV`/consts/fallbacks *as it reads* a
  value (the `ValueResolver` logic moves behind the `ValueDeserializer`), preserving RFC 0001
  §5 least-secret-exposure (resolution still happens last, at read) and RFC 0002 §9.1's
  const-only/deny/defer policies (the policy is a parameter of the read adapter). The
  **deferred** policy is explicit here: const/load-time policy resolves *during* the read, but a
  deferred value (operator `$ENV` deferred to execution, server keys resolved at request time —
  RFC 0002 §9.1/§9.2) deserializes into an **owned `ConfigValue::{Secret, SecretWithDefault}`
  carried out of the tree**, resolved later via `ConfigValue::resolve(context)` exactly as today.
  Deferred resolution therefore requires **no retained tree** (see §11) — the owned `ConfigValue`
  holds the unresolved ref.
- **Default-on-read.** A **schema-aware `MapAccess` adapter** sits in front of the
  schema-agnostic CST deserializer: when the tree omits a field that the schema defaults, the
  adapter synthesizes it. This *keeps `de.rs` schema-agnostic* — the defaulting layer is the
  one schema-aware piece, exactly mirroring RFC 0001's "schema-aware step in front of a
  schema-free deserializer," just at read-time instead of as an AST rewrite. `apply_defaults`'s
  budget/depth bounds (RFC 0001 §8.1) carry over to the adapter.

**Ordering is load-bearing: defaults are injected *unresolved* and flow *through* resolution.**
The current pipeline is `apply_defaults → resolve → deserialize` — defaults first, then
resolve. A `FieldDef.default_value` can itself be a `Reference`/`Role`/`Secret` (cf. RFC 0003's
`render_scalar`), i.e. a const ref or `$ENV`. So the default-on-read adapter must synthesize the
**unresolved** value and feed it **through** the resolve-on-read `ValueDeserializer`, never
emit a final value that bypasses resolution. Concretely: default-on-read is the **outer**
(`MapAccess`) layer and resolve-on-read is the **inner** (leaf-value) layer, preserving the
exact `default → resolve` order. A defaulted secret/const-ref that skipped resolution would
leak its literal name or evade the §9.1/§9.2 trust policy — so the layering is a security
property, not just a structuring choice.

Net: the `default → resolve → deserialize` pipeline keeps its *shape* and all its security
properties, but as a single read-time transform chain over the immutable tree rather than two
AST rebuilds. **This is a simplification, not just a port** — fewer allocations, one pass.

## 7. Dual-tree incremental phasing (the de-risker)

The migration is **not a big-bang cutover**. The CST is built **alongside** the existing owned
AST first, so nothing breaks while consumers move one at a time:

1. **Lossless lexer** (trivia tokens) — additive; the owned-AST parser keeps working off it.
2. **`rowan` + `SyntaxKind` + resilient parser emitting the green tree** — `parse` now returns
   the CST; a temporary shim builds the *old* owned AST from the CST so all current consumers
   compile unchanged. Both trees exist; behavior is preserved by the full suites — **with one
   expected, intentional diff: span values.** §2 replaces hand-tracked spans with `rowan`'s
   exact ranges, so wherever the old manual spans had off-by-ones/quirks the value changes.
   Tests that assert on span values are **re-baselined** to the exact ranges (exact > quirky);
   that re-baseline is the *only* sanctioned behavior change in this phase — everything else
   (parse results, diagnostics, deserialized values) is identical. The shim is *new* code that
   could silently diverge, so the precise gate is a **differential test** — old parser vs
   CST→shim over the existing corpus must yield structurally-equal owned ASTs (modulo the span
   re-baseline) — not merely "the suites pass."
3. **Typed wrapper layer.**
4. **Migrate consumers in risk order** (next section), each behind the green tree, deleting the
   shim's use site as it goes.
5. **Delete the owned AST, the shim, the manual spans, and the comment sidecar** once the last
   consumer is migrated.

The dual-tree phase means every step is green-suite-verified with no flag day. Steps are
independently shippable **with one exception**: the read-time deserialize stack
(`de.rs`-over-CST + the resolve/default adapters) is internally **atomic** — it must land as a
single phase, not a seam (§8 step 3, §13 P5) — because converting `de.rs` to read the CST
removes the consumption point for the previously pre-applied defaults/resolution.

## 8. Consumer migration order (lowest risk first)

1. **`nml-core`'s own AST walkers first — `model_extract` feeding `schema_index`.** These build
   `ModelDef` / `EnumDef` / `OneOfDef` and thus `SchemaIndex`, which the **default-on-read adapter
   (step 3 / P5) depends on**. So they migrate to typed wrappers in the read-only batch, *before*
   the read-time stack — a hard prerequisite, not an afterthought. (Building the schema-aware
   adapter against a not-yet-migrated `model_extract` would be a circular-dependency stall.)
2. **`nml-validate`, `nml-fmt`, `nml-lsp`** (read-only walks; 102 / 43 / 227 sites). The LSP is
   the biggest *winner* (resilience + exact spans) and a read-only consumer, so it lands early.
   `nml-fmt` *gains* capability here (comment-faithful over a lossless tree). One non-mechanical
   detail in the LSP: spans change type from the owned `Span` to `rowan`'s `TextRange` (u32 byte
   offsets), so the existing byte→UTF-16 line/col conversion (the LSP spec's default position
   encoding) is **retained and fed from `TextRange`** — a type swap, not a plain field rename.
3. **The read-time deserialize stack — `de.rs`-over-CST *plus* the resolve/default-on-read
   adapters (§5 + §6), landed atomically as one phase.** These are **not** a shippable seam: the
   instant `de.rs` reads the CST it reads the *raw* tree, so the defaults/resolution that used to
   be pre-applied to an owned `Body` have nowhere to be consumed — splitting them would load
   config undefaulted and unresolved (a correctness break, and for `$ENV` a security break), and
   a `de.rs`-over-CST built but unused until the adapters exist is dead code (§10 forbids it).
   Gated as a unit by the deserialize + defaulting + resolve suites.
4. **`nudge`** — last, largest, depends on all of the above; its 2,931 tests are the
   behavior-preservation gate. **The surface is not "1,121 mechanical edits."** Of nudge's NML
   access, ~255 sites are already **serde-mediated** (`from_block` / `from_body_defaulted` /
   `Deserialize`) and **do not migrate at all** — §5 keeps the derive structs untouched. The
   migrating surface is the **~946 raw AST walks**, and those are *not* trivial field-access:
   nudge matches directly on the owned AST's discriminants (`DeclarationKind::{Block, Array,
   Template, OneOf, Const}`, `BodyEntryKind::{Property, NestedBlock, ListItem, Modifier}`,
   `ListItemKind::{Shorthand, Reference, Named}`) and reads **non-optional** fields
   (`block.name.name`). Naively ported, each becomes an `Option`-handling rewrite or a
   `SyntaxKind`/typed-cast match — ~946 new `Option` decision points, a DRY and panic-risk
   regression.

   **So P6 is a triage, not a port (this is the legacy-removal lever).** RFC 0002 already proved
   the **serde/`from_block` path is the cleaner way to extract typed data** from NML. Most of the
   ~946 raw walks are *pure data extraction* — exactly serde's job — and per the project rule
   they are **replaced, not ported**:
   - **Extraction walks → converted to serde `Deserialize` / `from_block`.** They get validated,
     non-`Option` typed structs for free (the `de.rs`-over-CST core does the tree work), and the
     legacy raw-walk code is **deleted**. This shrinks the hand-migration dramatically and routes
     the behavior gate through the deserialize suite, not 946 hand-checked diffs.
   - **Only genuinely *structural* traversals** (workflow-graph walks, shape analyses that don't
     extract a struct) become typed-wrapper walks — and *that* small residual is the only place
     `Option`-handling legitimately lives.
   - For that residual, **one** shared validated-accessor helper expresses the post-validation
     can't-be-`None` invariant *once* (a typed error or a single `expect_valid` convention),
     never re-litigated per call site (DRY).

## 9. Security considerations

A parser is an attack surface: `nml-lsp` parses arbitrary opened files and `nudge` parses
config and (per RFC 0002 §13) potentially **tenant-authored** workflow files. Error recovery
must preserve the parser's safety invariants:

- **Guaranteed forward progress.** Every `synchronize()` consumes ≥1 token, so no input can
  drive an infinite recovery loop (a DoS). Asserted structurally and fuzzed.
- **Linear time, no quadratic recovery.** Synchronization only advances; it never rescans
  consumed input. Parsing stays O(n) on adversarial input.
- **Bounded output.** The error list is capped (stop emitting after N) so a pathological file
  cannot exhaust memory; the green tree's size is linear in input.
- **A fuzz/property harness** (any input → terminates in linear time, bounded tree + error
  list, never panics) is part of this RFC — the strict parser got termination "for free" by
  bailing; the resilient one must prove it. **It ships *with* the resilient parser (P2), not at
  the end:** the parser is the attack surface and is complete at P2, so fuzzing it then — rather
  than after the whole nudge migration — is the whole point of fuzzing the recovery logic.
- **No new resolution surface.** `resolve`-on-read keeps the §9.1/§9.2 const-only/deny/defer
  policies and least-secret-exposure intact — resolution still happens last, at read, with the
  same trust parameterization.
- **Trust-gating stays a static, whole-tree, load-time check — not field-read-conditional.**
  This is the subtle hazard of resolve-on-read. Today `resolve` is a *separate pass over the
  entire `Body`*, so RFC 0002 §9.2's fail-closed rule "a `WorkflowSource::Tenant` file
  containing `$ENV` is rejected at load" holds **regardless of which fields any struct later
  deserializes**. If rejection moved *into* the read adapter it would only fire for fields that
  happen to be read — a tenant file could smuggle `$ENV` into a skipped/ignored field and evade
  it. Therefore the tenant-`$ENV` rejection remains a **static pre-read scan of the whole CST**
  (a cheap walk gated on `WorkflowSource::Tenant`), run before/independently of deserialization.
  The invariant to preserve verbatim: *rejection at load is a whole-file static property,
  independent of which fields get deserialized.* Resolve-on-read changes **where the value is
  produced, never whether the file is admitted.**

## 10. Legacy removal (per the project rule)

The CST is strictly more capable, so the superseded pieces are **removed, not kept**:

- The **all-or-nothing / bail-on-first-error** control flow → replaced by parse-fully + `.ok()`.
- The **owned AST structs** in `ast.rs` → replaced by typed wrappers over the CST.
- The **manual `span: Span` plumbing** → replaced by `rowan` ranges.
- The **`Comment` / `take_comments` sidecar** → comments become tree trivia.
- Any **intermediate-AST rebuild** in `resolve`/`apply_defaults` → read-time adapters.

No compatibility shims survive the migration (the dual-tree shim in §7 is deleted in the final
phase). No `#[allow(dead_code)]`.

## 11. Encapsulation & crate boundaries

- The CST, `SyntaxKind`, resilient parser, typed wrappers, and the read-time resolve/default
  adapters all live in **`nml-core`** — the same crate that owns parsing today. Consumers depend
  only on the typed wrapper API and `Parse`, never on `rowan` directly (so `rowan` is an
  encapsulated implementation detail, swappable).
- `de.rs` stays schema-agnostic; the schema-aware defaulting adapter is the single schema-aware
  layer (RFC 0001's boundary, preserved).
- `nml-validate` / `nml-fmt` / `nml-lsp` / `nudge` consume the typed wrappers; no new
  cross-crate types beyond `Parse` + the wrappers.

### 11.1 Thread-safety: the CST is load-and-drop (red tree never crosses an await)

`rowan`'s **green** tree (`GreenNode`) is `Arc`-based → `Send + Sync`; the **red** tree
(`SyntaxNode`) caches parent pointers via `Rc`/`Cell` → **`!Send + !Sync`** (the same model
rust-analyzer uses — syntax trees are thread-local). Today's owned AST is `Send + Sync`, so this
is a *new* constraint, and it lands on **both** `Send`-bound async consumers — `nudge`
(multi-threaded `axum`) *and* `nml-lsp` (`tower-lsp` + multi-threaded `tokio`). The rule that
keeps it sound:

- **`nudge` retains only owned deserialized data — never a `SyntaxNode`.** The CST is consumed
  at load (deserialized into `#[derive(Deserialize)]` structs) and **dropped**; the red tree
  never crosses an `.await` or a task boundary. This is *already* how deferred resolution works
  (§6): the unresolved value is carried out as an owned `ConfigValue::{Secret, SecretWithDefault}`
  and resolved at request/execution time — so RFC 0002 §9.1/§9.2 deferral needs no retained tree.
- **If parse state must ever be retained** (a future optimization; not needed now), it holds the
  `GreenNode` (`Send + Sync`) and materializes a thread-local `SyntaxNode` on demand — never the
  red node itself.
- **The rule applies to `nml-lsp` too** — it is *also* `Send`-bound async (`tower-lsp` +
  multi-threaded `tokio`), so a `SyntaxNode` may **not** be held across an `.await` or stored in
  the document cache. Today's server already does the right thing: it caches **source strings**
  (`documents: Mutex<HashMap<Url, String>>`) and re-parses per request, so a `SyntaxNode` is
  created, used, and dropped *within* a single handler (never across an await). The precise
  invariant for both consumers: **the red tree is transient within a synchronous span of work and
  never crosses an `.await`, a thread, or a cache boundary**; anything retained is the `GreenNode`
  or owned deserialized data. If the LSP later caches parse state for incremental reparse, it
  caches the `GreenNode`, not the red node. Made explicit here so it is designed-for, not
  discovered during the P4 LSP migration or the P5/P6 `nudge` work.

## 12. Risks

- **`de.rs` + defaults are the heart of config loading** (all of nudge's config + workflows).
  *Mitigation:* migrate them as isolated phases (§7–8), each gated by the existing deserialize /
  defaulting / full nudge suites (2,931 + 421 tests) — behavior preservation is *testable*, and
  the test investment from RFCs 0001–0003 is precisely the safety net.
- **Scale of the nudge walk migration (~946 raw walks; ~255 serde sites don't move).**
  *Mitigation:* the dual-tree shim keeps nudge compiling until its turn; and — per §8 step 4 —
  most raw walks are *data extraction* that is **converted to serde and deleted** rather than
  ported, so the residual hand-migrated (structural) surface is far smaller than 946, reviewable
  in chunks, with the deserialize suite (not per-site diffs) as the gate. The risk is *triage
  judgment* (extract-via-serde vs. keep-as-structural), not volume.
- **Recovery quality (where to synchronize).** Poor sync points yield cascading phantom errors.
  *Mitigation:* NML's indentation gives clean, well-defined sync boundaries (declaration / entry
  / dedent); pin recovery behavior with golden tests over deliberately-broken inputs.
- **Defaulting redesign.** Moving from AST-rewrite to default-on-read is a real redesign.
  *Mitigation:* it is behavior-equivalent (same defaults, same bounds), pinned by the RFC 0001
  drift/default tests; and it is *simpler* (one pass), reducing long-term risk.

## 13. Phasing

Rough sizing is **T-shirt only** (S / M / L), not week estimates — enough to see where effort
and risk concentrate for the go/no-go, without false precision:

- **P0 — Decision + spike** *(S).* Land `rowan` behind a feature gate; prototype the
  lexer-with-trivia + a minimal resilient parser for one declaration kind, producing the green
  tree, to validate the approach and the recovery sync points. Throwaway-able.
- **P1 — Lossless lexer** *(M)* — includes the offside-rule layout pass (§4.2.1).
- **P2 — Resilient parser + green tree + the dual-tree shim + the fuzz/property harness** *(L)*
  (old AST built from the CST; full suites green, behavior preserved modulo the span re-baseline
  of §7; the recovery logic is fuzzed the moment it exists, per §9). *The risk-concentration of
  the parser half.*
- **P3 — Typed wrapper layer** *(M).*
- **P4 — Migrate `nml-core`'s walkers (`model_extract` → `schema_index`) + `nml-validate` /
  `nml-fmt` / `nml-lsp`** *(M)* — the schema walkers land **before** the read-time stack that
  depends on `SchemaIndex` (§8 steps 1–2); LSP error tolerance + exact spans land here; fmt
  becomes comment-faithful. *Front-loads the headline IDE wins.*
- **P5 — The read-time deserialize stack, atomic** *(L):* `de.rs`-over-CST **+** the
  resolve/default-on-read adapters in one phase (§8 step 3). Gated as a unit by the deserialize +
  defaulting + resolve suites; not split into a seam. *The other, deeper risk-concentration.*
- **P6 — Migrate `nudge`** *(M–L; risk is triage judgment, not volume)* — **not** a mechanical
  port: the ~255 serde-mediated sites don't move; the ~946 raw walks are **triaged** —
  data-extraction walks *converted to serde and deleted*, only structural walks ported to typed
  wrappers behind the shim (§8 step 4). The residual hand-migrated surface is far smaller than
  946, gated by the deserialize suite.
- **P7 — Delete the owned AST, the shim, manual spans, and the comment sidecar** *(S).* (The fuzz
  harness already landed in P2.)

Each phase is independently shippable behind the dual tree **except P5, which is internally
atomic** (§7); P4 already delivers the headline IDE wins, so value front-loads even though the
full migration is multi-phase. **Risk concentrates in P2 (parser/recovery) and P5 (the
deserialize stack)** — the two L's that aren't merely large-by-volume; everything else is
mechanical or additive.

### 13.1 P0 outcome — spike complete, approach validated

The P0 spike is implemented and green behind the `cst` feature in `nml-core`
(`src/cst/`: `syntax.rs`, `lexer.rs`, `parser.rs`, `mod.rs`), parsing the block-declaration
subset end-to-end. It is **purely additive** — `rowan` is absent from the default dependency
tree, the existing 416-test suite is unchanged, and the spike adds 8 tests (424 total green),
clippy-clean. It validated every load-bearing claim in this RFC:

- **`rowan` fits NML.** The green/red tree, the single `SyntaxKind` taxonomy, and the
  `Language` binding work; **zero-width `Indent`/`Dedent`/`Eof` tokens are accepted** by
  `GreenNodeBuilder` — the offside-marker design (§4.2.1) is viable, no empty-token obstacle.
- **Losslessness on *any* input.** A 4,000-iteration fuzz/property harness (§9) over an
  adversarial alphabet confirms: the parser never panics, terminates (forward-progress
  recovery), the tree's text is **byte-identical to the source**, and the error list stays
  bounded (`MAX_ERRORS`).
- **Offside + multi-line-string suppression (§4.2.1) — the NML-specific risk — works.** A test
  with deeper indentation and a `key:` line *inside* a `"""…"""` value confirms it stays one
  `String` token and contributes **no** layout structure. Inconsistent dedents recover with a
  diagnostic, never a panic.
- **Resilience / all-errors (§2, §4.3).** Two broken lines followed by a valid property: the
  parser recovers, still parses the valid property, and collects *every* error; `.ok()` returns
  `Result<_, Vec<NmlError>>` (all errors, not the first).
- **The rust-analyzer event architecture (§4.3) is clean.** The parser emits a flat `Event`
  list over a trivia-stripped view; a separate tree-builder re-attaches trivia. Grammar
  functions stay trivia-free. Typed wrappers (§4.4) are `Option`-returning and trivia-aware.

Two findings that **confirm RFC items, not surprises**:

- **Trivia attachment is real and correctly deferred to P4.** With naive eager-flush, leading
  trivia attaches *inside* the following node, so accessors must skip trivia (they do). The
  precise `n_attached_trivias` policy (§4.3) is the P4 refinement, exactly as scoped.
- **P1 is de-risked.** The *legacy* lexer already emits `Indent`/`Dedent` and already treats
  `"""…"""` as a single token — so P1 reuses that offside logic rather than rebuilding it.

The spike is throwaway scaffolding (it parses one declaration kind); P1+ build the real thing
*as the CST*. Nothing here commits to that — but the approach now carries **demonstrated**
evidence, not just argument, into the go/no-go.

## 14. Alternatives considered

- **Resilient *recursive descent* on the owned AST** (the cheaper option discussed alongside
  this RFC): adds error recovery to today's parser, keeping the owned AST. It delivers the LSP
  error-tolerance win at a fraction of the cost — but it does **not** give lossless source,
  exact-free spans, comment-in-tree formatting, or incremental reparse, and its recovery *code*
  is thrown away if the CST is later adopted (the recovery *design* transfers). **It is the
  right call only if the CST is not going to be built.** If this RFC is accepted, that bridge is
  skipped (the CST parser is resilient by construction).
- **tree-sitter.** Incremental and error-recovering, but means a grammar in its DSL and a
  *parallel* parse tree alongside the typed model — duplication and a second source of truth for
  a custom language with an existing clean hand-written parser. `rowan` keeps one hand-written
  parser and one tree.
- **LSP-local heuristics** (indentation line-scan for context; synthetic-edit at the cursor):
  a weaker, parallel context mechanism that re-implements — imprecisely — what the AST walk does
  exactly, helps only completion (not diagnostics/hover/symbols), and is legacy-by-construction.
  Rejected by the project rule (don't add a weaker parallel path).

## 15. Recommendation

A lossless CST is the correct end-state for a language with first-class IDE tooling, and the
benefits are structural and compounding (resilience for *every* feature, all-errors diagnostics,
exact spans, faithful formatting, incremental reparse, a cleaner transform pipeline). It is a
**multi-week, multi-phase** investment whose risk concentrates in two well-bounded cores
(`de.rs`, defaults) that the existing test suites can gate. The dual-tree phasing makes it
incremental and reversible rather than a flag day, and P4 front-loads the IDE payoff.

Proceed **iff** the editor/LSP experience is strategic enough to fund the staged effort; if so,
build it *as the CST* and skip the resilient-RD bridge. This RFC is the scoping artifact for
that go/no-go — not a commitment to start coding before the decision.
