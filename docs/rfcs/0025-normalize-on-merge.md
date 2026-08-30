# RFC 0025 — Normalize-on-merge: deciding at merge time and deleting the composition plan

- **Status:** Proposed — **SEALED 2026-08-29** after five review rounds
  (split out of RFC 0023 after round 1; rounds 2–4 certification + an
  implementation dry-run; the round-5 certifier's verdict was SEAL, its
  two editorial edits folded). Next contact is the build, after RFC 0023
  lands. (renumbered from 0024 after a concurrent RFC took that
  number)
- **Date:** 2026-08-29
- **Crates:** nml-core (layers, identity, diff), nml-cli (a hidden dump
  flag for the oracle)
- **Depends on:** RFC 0019 (instance composition); RFC 0023 Parts B–C (the
  head rule, strip-by-name); RFC 0015 (nominal union annotations, the D2
  oracle; `Body::with_entries` for every derived body, 0015:38-52);
  RFC 0005 §10 (an item's own token beats a shared property, 0005:284-285)
- **Amends:** RFC 0019 (step 3/4 text, plan item 2, the NML2086 row,
  0019:771-772, errata E13/E14 — erratum E18); the error index entries
  NML2086 and NML2079
- **Origin:** the RFC 0019 union-compose review arc (rounds 12–19) and the
  architecture reviews of 2026-08-29. Every claim is grounded with a
  file:line against the working tree or a probe restated as a fixture.

## Summary

Composition today runs three walks over two representations of the same
bodies: a **plan** folded over raw, array-ref-inlined bodies
(`build_arm_plan`), a **deep normalization** of every layer under the plan's
variants (`normalize_inlined`), and a **merge** over the normalized bodies
that replays the plan's traces (`layers.rs:3609-3846`). Six cross-walk
invariants keep the two representations aligned. This RFC replaces them
with one walk: the merge **decides at each level over raw supplies**,
normalizes only the **survivors** under the decided variant, diagnoses
**discarded** bodies by subtraction under their own readings, and folds an
identity-item group **once, before** it materializes its token. The plan
and its guards are deleted; one normalizer with a depth policy serves the
merge, the seal backstop and defaulting; diagnostics are emitted through a
sink that orders them by a total key. A six-phase migration keeps the
1,580-test battery authoritative through an oracle built from a tagged
binary's dump, not a frozen copy of the engine.

## Motivation

Rounds 12–19 of the RFC 0019 review found one defect class six times:
the plan reading raw bodies while the merge read normalized ones —
supply-set parity, kind parity, discriminator-strip parity, normalization
vocabulary, sealed-unplanned, item scope. Each is now guarded
(`is_value_entry`, `is_discriminator_entry`, `UnionPlan::aligns`,
`surviving_indexes`, `PLAN_LOOKUPS`, `PLAN_TAMPER`, bracketed item paths).
Each guard is sound; together they are the symptom.

Two defects survive the guards because item scopes are **unplanned**
(`arm_plan_walk` asserts it, `layers.rs:1878`; the positionalizer passes
`path: None` inside items, `identity.rs:514-620`):

- **Positional injection under the wrong arm.** An item's `+` token
  materializes under the item's *own default arm* even when identity
  merging composes the group under another arm: a top item `- "h2"` gets
  `name = "h2"` injected under `stepA` while the group composes under
  `stepB`, and the composed view reports `unknown property 'name'`. The
  dotted twin is clean — the plan fixed it there only. (Fixtures `K`, its
  dotted twin `K2`, its zero-item twin `K3`.)
- **Owner attribution after a mid-layer switch.** `existing_layer` for a
  merged item stays the base's id (`layers.rs:5521, 5635`), so the fold's
  trace and NML2060's `with_source` can name the wrong file in a multi-file
  stack.

## Design

### 1. One normalizer with a depth policy

```rust
// layers/normalize.rs
pub(super) enum Descend { ThisLevel, Deep }

/// ONE normalizer.
/// ThisLevel — this body's own entries only: array and modifier
/// re-spelling; this entry's own zero-item verdict (NML2079, including the
/// verdict on a nested block's emptiness); the inline arm headers of this
/// body's `Arms`-typed fields (`map_inline_arm_bodies` one hop into the
/// field's block, header only, no recursion); a `.shared` line in a
/// non-list body is consumed, as today. Nested model, oneof and union
/// bodies, modifier-block items and `ListItem` bodies pass through RAW —
/// their own level normalizes them.
/// Deep — today's `normalize_for_scan` composition: positional
/// materialization, `.shared` distribution and spellings, recursing under
/// each nested position's OWN vocabulary (`own_vocab`).
pub(super) fn normalize_level<'i, 'a>(
    index: &'i SchemaIndex, vocab: Vocab<'i>, body: &Body,
    descend: Descend, sink: Option<(InstanceId<'a>, &mut ComposeSink<'a>)>,
) -> Body;

/// A body's own vocabulary at a position — today's `named_vocab`,
/// `item_vocab` and `oneof_vocab` (`layers.rs:3116-3236`) minus the plan
/// consult: a model directly; a oneof under the arm the body states, else
/// the schema default; a union under the variant its annotation or shape
/// selects; NONE when the D2 oracle calls it ambiguous or the stated arm is
/// unknown (a property `xs = []` is then unwarned and an undeclared
/// `|x = []` warned by shape — today's `oneof_vocab` fallback; the
/// validator's unknown-arm finding is the signal). `target` is the
/// position's declared type (`list_inner(effective_type(&f.field_type))`
/// for items), never an owned clone.
pub(super) fn own_vocab<'i>(index: &'i SchemaIndex, target: &'i FieldType, body: &Body) -> Vocab<'i>;
```

The deep normalizer cannot be deleted: the seal backstop asks "what would
the displaced compose carry?" and needs a deep, plan-free normalization
under a chosen arm (`normalize_for_scan`, `layers.rs:1731-1744`, used by
`displaced_group_seals_into` and `displaced_list_seals`), and defaulting
needs `apply_positional` (`defaults.rs:65`). Two normalizers would be the
two-representation seam again; one function with a depth policy is not.
`path` disappears from every normalization signature — only plan lookups
used it. Every derived body is rebuilt through `Body::with_entries`
(RFC 0015:38-52).

### 2. The merge decides

```rust
// layers/merge/mod.rs
impl<'a, 'd> Merger<'a, 'd> {
    /// Raw, array-ref-inlined bodies in; the composed body out.
    pub(super) fn compose_root(&mut self, root: &str, inlined: &[(InstanceId<'a>, Body)]) -> Body;
    fn merge_model_bodies(&mut self, path: &str, model: Option<&ModelDef>, raw: &[(InstanceId<'a>, Body)]) -> Body;
    //   step 0: each body → normalize_level(ThisLevel, Vocab::of_model(model)); then gather as today.
    /// Output-bound entries that pass no merge level (a sealed first write,
    /// an arm-set winner, a bare-list winner, passthrough items): Deep
    /// under their own vocabulary, interior only (their own level's facts
    /// were emitted by ThisLevel). An `Arms` target recurses per inline
    /// arm body under the arm target's own reading (`arm_body_vocab`,
    /// `:2689`; `positional_against`, `identity.rs:477-485`): positional
    /// and `.shared` only, no spellings — `Vocab` has no arms case and
    /// `normalize_spellings` never enters `Arm` entries (`:3548`), as today.
    fn emit_deep(&mut self, path: &str, target: Option<&FieldType>, layer: InstanceId<'a>, entry: BodyEntry) -> BodyEntry;
}
```

- **Per level.** `merge_model_bodies` normalizes each body `ThisLevel`
  under the level's model; nested bodies pass through raw to their own
  level, where `merge_union`/`merge_oneof_bodies` fold over the raw
  supplies and normalize the survivors under the decided variant. The
  kind-parity check disappears as a concept: one classification per body.
- **The folds are raw-safe at dotted positions.** `fold_arm_checked`
  (`layers.rs:1984-2019`) reads only `stated_discriminator` (a string
  `Property` named like the discriminator); `fold_variant_checked`
  (`:2904-3011`) reads `UnionSupply::classify` — the type annotation,
  `zero_item_body_at` (explicitly `.shared`-tolerant), the D2 oracle and
  `resolve_type_in_body` over `BodyShape`. Nothing normalization adds at a
  dotted position is read: injection reaches only item bodies
  (`identity.rs:531-612`) and inline arm bodies (`identity.rs:160-200`,
  `map_inline_arm_bodies`), `.shared` lives only in list bodies, and
  spelling normalization never turns a modifier into a `Property`.
  `build_arm_plan` already feeds the folds inlined bodies
  (`:1746-1788, 3786-3803`).
- **Arm headers are read by a seal judgment.** `merge_arm_set` judges a
  replaced set through `displaced_group_seals_into` over
  `normalize_for_scan`, which materializes nothing into the arm bodies it
  is handed (the walk starts inside them) — today
  the injected arm header is already present (fixture `AS1`: a `name
  string #sealed` on the arm target draws NML2060 on replacement; `AS0`,
  unsealed, is clean). An arms-typed field's `Arm` entries live inside
  the field's nested block, which the model level routes to
  `merge_arm_set` (`:4318-4320`; `:4106` sees only arms authored directly
  in a body), so `ThisLevel` at a model level injects the headers one hop
  down — `map_inline_arm_bodies` over each `Arms`-typed field's block,
  header only — where today's positionalizer does (`identity.rs:398-478`),
  before `merge_arm_set` judges the displaced set; `emit_deep` on the
  winning set carries them (the composed body keeps `name = "One"` today).
  An arms field inside a displaced oneof arm is judged by `Deep` from the
  containing model and already sees its headers (fixtures `AS2`/`AS3`).
- **Item scope needs `.shared` before the fold.** A top layer's `.kind = "b"`
  legitimately switches an identity-merged item's arm today because item
  bodies are shared-distributed before `merge_oneof_bodies` folds them
  (fixture `C2`). Section 3 orders it.
- **A `+` token is never a discriminator on valid input.** NML2043 forbids
  shorthand items on oneof- and union-element lists. On NML2043-invalid
  input the token switches an item's arm today (fixture `B2`: the composed
  view carries an NML2060 that stops firing once the fold precedes
  materialization). Allow-listed; NML2043 is the validator's verdict on
  that file either way.

### 3. Items: gather, then compose — one fold per group

`merge_items` is pairwise today (`resolved[pos] = merged`, `layers.rs:5480-5628`):
the same body would be re-normalized at every layer it survives, re-emitting
NML2079 (dedup'd only at `compose_file`) — n-ary is mandatory, not
optional. Its input gains each layer's list-level `.shared` lines beside
its items (`per_layer: &[(InstanceId, Span, Vec<SharedProperty>, Vec<ListItem>)]`;
`merge_list`'s loop builds the tuple, `:5254-5265`; `items_of` (`:1112`)
drops `SharedProperty` entries, so a `shared_of` extractor joins it; a
`ModifierValue::Block` carries none), and `distribute_shared_level` is an
item-level primitive (`apply_shared_properties` is body-level,
`resolve.rs:341`).

```rust
struct ItemGroup<'a> { key: ItemKey, members: Vec<(usize /* layer index */, ListItem)> }
// head and survivors are the fold's output, returned in the level's `Composed`.
```

- **Gather** (one pass, layer order, first-seen group order; the prehash
  buckets `:5333-5349` stay): a second same-key item in one layer under
  `#identity`/`#identity #append` → NML2063 duplicate (item dropped);
  same-kind group lookup, else a token-equal cross-kind match → NML2063
  (dropped) — both only once a lower layer has supplied at least one item
  (`base_established`, `:5350-5356`); within the supplying base itself a
  token-equal pair of different kinds is legal and both are kept
  (`:5384-5402`, `:5436-5447`); no group and the base is established under
  `#identity` →
  NML2067 (dropped; a dropped member never anchors a group), else a new
  group; `#append`: a scalar same-key item → a new singleton group
  (concatenation, never paired), a non-scalar → NML2063; a `Reference`/`Role`
  restatement joins its group (no body; a no-op).
- **Compose** (per group), owned by the level's authority — **one fold**:
  `members' = members.map(|m| distribute_shared_level(list_shared_of(m.layer), raw_body(m), mask = token_field))`
  — each layer's list-level `.shared` into that layer's items
  (RFC 0019:412-416: `.shared` never applies to items another layer adds;
  a bodiless scalar member takes a fresh body for the distribution exactly
  when its own reading materializes it — a `+` field, not ambiguous:
  today's pipeline gives precisely those members a body before the shared
  merge, probed by a base `.tag` reaching a bodiless `- 1`'s composed item
  — while Reference/Role, dropped-key and ambiguous bodiless members pass
  through as `merge_shared_into_item` does today, `resolve.rs:468-493`),
  **yielding to the member's token field** — the `+` field of the member's
  own reading (`own_vocab`: a model directly; a oneof under the arm the
  member states, else the default; a union under its annotation or shape;
  nothing when ambiguous) for a **scalar** key, nothing for a Named,
  Reference or Role key: exactly the field today's positionalizer injects
  before `apply_shared_properties` (`identity.rs:558-598`), so a `.shared`
  naming it never writes it (RFC 0005 §10; fixtures `P2`, `P4b`, `P5b`:
  today a top layer's `.name = 5` on a `+` field neither reaches the
  composed item nor draws NML2060, and that survives; `P3`: a shared value
  does reach a tokenless item). A Named key's `name` is **not** a token:
  composition never materializes it — the validator and `de` inject it,
  leniently — so a list-wide `.name` reaches a Named item today (`Q2`,
  `Q7`, `Q2u`) and keeps doing so; masking it only in composition would make
  a composed file deserialize differently from the same body authored
  plainly. (An authored or shared write over a sealed Named `name` is
  silent today — a pre-existing RFC 0005 §10 gap, filed separately, not
  changed here.) Then `merge_item_bodies(item_path,
  token, members', anchors)` → `merge_oneof_bodies`/`merge_union_bodies`
  fold **once** over `members'` and normalize the survivors under the
  decided variant, materializing a **scalar** key's token (`materialize_item`
  under `decided_model` — a model target directly; a oneof's
  `variant_model_of` the decided arm; a union's named variant's model or
  that variant's arm; `None` for an ambiguous or structural establishment;
  Shorthand members only, as today's `list_item`, `identity.rs:585-598`)
  into the **lowest surviving body only, before the body merge** (after it,
  an explicit `name = "other"` above a sealed `+` would bypass write-once;
  into every survivor is today's strip-requiring design), then
  `ThisLevel`. The token reaches the body merge through the signatures:
  `merge_oneof_bodies(path, oneof, layers, token: Option<&ItemToken>)`,
  `merge_union_bodies(path, union_ty, layers, anchors, token)` and
  `merge_variant_group` — applied to `members'[survivors[0]]` under the
  decided model after the fold and before `merge_model_bodies`.
  `ItemToken` is new (identity.rs): the group's scalar key **with its
  source span** — `struct ItemToken { value: SpannedValue }`, taken from
  the key member's `Shorthand` value (`ItemKey::Scalar` carries the
  `Value` but not the span, `:6168`; the injected property must keep the
  token's span — `inject`, `identity.rs:226`). `materialize_item`'s
  Shorthand arm (`identity.rs:87-147`) is extracted as
  `materialize_token(&ItemToken, &Body, &ModelDef) -> Materialized` so the
  merge can inject into a survivor body the item does not own;
  `materialize_item` re-expresses over it. `SurvivorSet` is `Vec<usize>` —
  `surviving_indexes`' existing return.
  `merge_items` neither folds nor normalizes. `merge_item_bodies`'
  one-anchor-per-body contract (`:4953`) survives with N anchors.
- **Consequences.** The token-restatement strip (`:5535-5615`) is deleted:
  it exists only because today's positionalizer injects the token into
  every layer's copy (its own comment says so). An *authored* restatement
  of a sealed `+` field, stripped today when `semantic_eq` to the token
  (`P6`), becomes NML2060 equal-value (correct). The head rule (RFC 0023
  Part B; `[existing, item][head]` becomes `members[head]`) fixes the owner
  attribution above. Bare-overlay list winners use `emit_deep`, never
  singleton groups — a bare list has no identity semantics, and same-key
  items within one winning layer are legal there ("set dedupe rides
  validation", `:5330`).

### 4. Discarded bodies: own readings, by subtraction

**The rule.** *A discarded body is diagnosed under the vocabulary it was
authored for — its own stated or inferred variant at every level: the
stated discriminator else the schema default, the authored `as` else the
shape, no vocabulary when the D2 oracle calls it ambiguous.* Today's
behaviour is scope-dependent, probed:

| Loser | Today's vocabulary | Fixture |
|---|---|---|
| dotted- and root-scope rejected switch | the survivor's (the plan's final arm) | `A`, `J`, `H2`, `R1` (`extras = []`, a list only in the rejected body's own arm, is not warned) |
| dotted- and root-scope switch-displaced group | the survivor's (the new arm) | `S1`, `S2` |
| item-scope rejected / displaced | its own stated arm | `NA1`, `S3` |
| sealed-position loser | its own reading, stated-else-default (E13) | `F2` |
| NML2085 discard over a structural establishment | its own inferred variant | `E` |
| bare-list, unmatched, duplicate, cross-kind, append losers | its own (the element type per item) | `D`, `G`, `L1`–`L3` |

The rule equals today at rows 3–6 and differs only at rows 1–2 — exactly
the plan artifact the previous draft already disowned (the rejected top in
`H2` wrote `kind = "b"` and `extras = []`; telling it about `xs` "as a
list" is the survivor's reading, not the author's). It is what a compiler
does with dead code (rustc checks an unreachable branch in its own context,
never the surviving branch's); it makes a discarded body's diagnostics a
pure function of the body — stable under the one-home key across every
dependent that discards it (`FindingKey`, `layers.rs:3861-3869`); and it
makes discard diagnosis **context-free**, computable from `own_vocab` alone.
Option (c), "dead bodies are silent", would delete the rule and the
subtraction at the cost of two-step discovery and an erratum to RFC
0019:416-421 — rejected.

**By subtraction.** RFC 0023's `Composed { body, head }` gains
`survivors: SurvivorSet` (the surviving contribution indexes), every level
authority returns it, and the subtraction runs where a level's members are
known — **three call sites of one helper** that owns the loop and the
`debug_assert!(survivors ⊆ members)`, each site supplying a
loser-projection closure `Fn(&M) -> Option<(InstanceId, &Body, Vocab)>`
(`None` = a bodyless member: Reference/Role items, scalar entries): the
root projects the whole layer body under the resolved root nameable; the
model level projects a losing entry's nested body under
`own_vocab(field type, body)`; the item level projects the item body under
`own_vocab(element, body)`: `compose_root` (today's `merge_root`,
`:4018`, renamed; a oneof root's rejected and switch-displaced layers,
`:4023` — a model root subtracts only at the model level, a oneof root at
both), `merge_model_bodies` (entry losers, once per level), and
`merge_items` (item losers: gather drops and a group's fold losers — a
dropped item lives inside a surviving list entry, so no per-`(layer,
entry)` subtraction reaches it). A forgotten survivor
then produces a *false* dead-body warning instead of a silently missing
one; the `DISCARDS` seam (§6) is the observable, since only fixtures with
zero-item entries make it loud. **Nothing twice:** at a model level
`ThisLevel` owns every entry's own-level facts (spelling, its own NML2079),
losers included, so the subtraction there normalizes only the losing
entry's nested body; at the root and item homes a loser is a whole body
the fold read raw and no level normalized, so the subtraction runs `Deep`
over all of it (once per loser, O(entries)) — either way nothing is emitted
twice (`resolve_layers` returns raw emission; only `compose_file` dedups,
and the exact-vector pins would see a duplicate). The only diagnostic normalization
emits is NML2079 (`normalize_spellings` pushes only `zero_item_warning`;
the positionalizer discards `Materialized.diagnostics`;
`apply_shared_properties` emits nothing), which bounds the stakes.

**Survivors, per authority** (indexes into each function's input; a
*contribution* is one `(layer, value entry)` from `merge_model_bodies`'
gather, `:4064-4105`):

| merge | survivors |
|---|---|
| `merge_model_bodies` | every layer (nothing whole-body is discarded here) |
| `merge_sealed` | the first real writer (`:4242`); an all-zero stack: the last entry |
| `merge_overlay` scalar / `scalar_overlay` | every contribution (the loser is a scalar with no interior; NML2084 is its own) |
| `merge_overlay` object-typed | every nested contribution (a model target; a variant target passes the group's through — the `merge_variant_group` row); the nested level subtracts its own; a non-nested spelling is a scalar loser with no body — silent today (`:4346`), unchanged |
| `merge_overlay` bare list, `merge_modifier` overlay, `structural_overlay` Items | the winner only (losing lists' interiors are diagnosed today — `U1`, `U2`) |
| `merge_arm_set`, model-level `arm_sets.last()` | the effective set; losers silent (`Arm` interiors are never normalized, `:3548`) |
| `merge_list` / `merge_modifier` (list policy) | every item-bearing contribution; a non-item spelling is a scalar loser with no body — silently skipped today (`:5257-5259`, `:5204-5206`), unchanged |
| `merge_variant_group` | pass-through (Model: every body; OneOf: `surviving_indexes`) |
| `merge_union_bodies` zero-item | every layer (`:4966-4970`) |
| `merge_union` Named / Ambiguous | `replay.group` |
| `merge_union` Items / Value | the bare-list winner / the last of `replay.structural` |
| `merge_union_bodies` Named / Ambiguous / structural | `replay.group` / the **whole** `replay.structural` slice (a model-less deep merge, `:5009-5015`) |
| `merge_oneof_bodies` | the group after the last accepted switch — `surviving_indexes(trace)` under the oneof FACE: `Pinned`/`Discarded` excluded (§6; the union face keeps `Pinned`, `:1809`), so the rule gains the face as a parameter, and the head (RFC 0023) derives from the same filtered set |
| `merge_items` | per group, the level's survivors; gather drops (NML2063/2067) are losers with no group |

Neither survivors nor discards, never subtracted: declarations (E12, the
`declarations` map, `:4061`), passthrough `SharedProperty`/`FieldDefinition`/
`ListItem` entries (`:4109-4113`), model-level `Arm` entries. Discard sites
covered, with today's behaviour: `merge_sealed` upper writes (`:4253-4297`,
diagnoses); `merge_overlay` bare-list losers (`:4331-4335, 4396-4408`,
diagnoses); `replay_union` rejections and discards (`:4644-4675`, both
faces, diagnoses) and the **switch-displaced group** (`replay.group.clear()`,
`:4677`, diagnoses under the survivor today — the most common discard);
`merge_oneof_bodies` rejections (`:5754-5773`) and switch-displaced groups
(`:5787-5799`); `merge_union_bodies` structural non-survivors (`:5009-5015`);
`merge_modifier` overlay losers (`:5183-5190`); `merge_items` duplicates,
unmatched, cross-kind and append losers (all **NML2063** except NML2067,
`:5363-5487`). RFC 0019's "always diagnosed, never silently ignored"
(`0019:416-421`, NML2079's contract) holds inside every discarded body
exactly as today.

### 5. The order contract

Nothing sorts today: `resolve_layers` appends per-layer normalization in
stack order, then merge findings (`:3811-3827`); `compose_file` only
dedups; the CLI prints emission order. Level-wise emission changes
same-layer interleavings. Seven pins assert an exact multi-code order —
`[2079, 2060]` ×3 (`layers.rs:11696, 11976, 12023`), `[2060, 2085]`
(`:9992`), `[2085, 2084]` (`:10054`), `[2079, 2079]` ×2 (`:11149, 11837`) —
each pair in different layers, in **stack** order: the primary key. Same-
layer interleavings flip to span order (fixtures `H`, `H3`, `NA1`), pinned
as order-changing in Phase 0.

```rust
/// Compose-time findings, each stamped with the stack position of the
/// layer it was found in — a total key, so emission order is irrelevant.
pub(super) struct ComposeSink<'a> { stack: Vec<InstanceId<'a>>, items: Vec<(usize, Diagnostic)> }
impl<'a> ComposeSink<'a> {
    /// Stamps the position of the OFFENDING contribution's layer — every
    /// emitter already holds it and already sets `with_source` (13 merge
    /// sites, 5 normalization sites); `emit` never re-sources.
    pub(super) fn emit(&mut self, layer: InstanceId<'a>, d: Diagnostic);
    /// Sorted by (position, source, span, code, message).
    pub(super) fn finish(self) -> Vec<Diagnostic>;
}
```

Deriving the owner afterwards is not possible (`Diagnostic.source` is a
file, shared by base and top; `BlockDecl` has name and keyword spans but no
whole-block span). The sink owns its stack (a short `Vec` of `Copy` ids —
the stack `Vec` is local to `resolve_layers`). Linearization and grant
findings never enter the sink: they fail closed and `resolve_layers`
returns before the merge — and so before the sink — exists
(`:3683-3775`); the assembly is those pre-sink findings followed by
`finish()` (`Code` and `Span` have no `Ord`; the key maps them); `compose_file`'s dedup is
unchanged. `Merger.diags` becomes `&mut ComposeSink`; `normalize_level`'s
sink carries the layer and reborrows through recursion. Multi-file stacks
after RFC 0020 are covered by `InstanceId` positions plus `(source, span)`.

### 6. Seams

- `FOLD_TAMPER: Cell<Option<fn(&str, &mut DecisionTrace<'_>)>>` — one
  take-once hook after both merge-level folds, path-aware (the first fold
  executed wins the take; the liveness test targets its fixture's first
  folded position). The test corrupts a oneof trace entry to `Pinned` →
  exactly one NML2086 ("a union-only verdict at a oneof position",
  `layers.rs:5776-5786`) → the boundary `debug_assert!` (`:3838`, reading
  the same sink) fires in debug builds. (Corrupting a union trace to
  `Discarded` at index 0 yields `[NML2086, NML2085]` and elsewhere only
  NML2085 — the wrong target.) The oneof face's survivors exclude
  `Pinned`/`Discarded` (`surviving_indexes` keeps `Pinned` for the union
  face, `:1809`), so a tampered layer diagnosed "not composed" is not
  composed. `surviving_indexes` takes the face as a parameter rather than
  growing a second function — its doc records four prose-tied copies as
  the disease — and RFC 0023's `surviving_indexes().first()` becomes
  `survivors.first().copied().unwrap_or(0)` over the filtered set:
  identical on every reachable trace (index 0 is Switch or Join,
  `:1990-2014`; `Pinned` at a oneof position is tamper-only), and under
  tamper the head falls back to 0 instead of naming a layer the face just
  diagnosed.
- `FOLD_LOG: RefCell<Vec<String>>` — every path folded, dotted and
  bracketed (replaces `PLAN_LOOKUPS`).
- `DISCARDS: RefCell<Vec<(String, String)>>` — every `(path, layer)` the
  subtraction diagnosed.
- NML2086 stays allocated and emitted: the `Dropped` sites (`merge_union`
  `:4544, 4558`, `replay_union` `:4663`, `discarded_union_contribution`
  `:4883`, `merge_union_bodies` `:4986`, `merge_oneof_bodies` `:5776-5786`)
  and the editor's never-dark guard (`nml-lsp/src/diagnostics.rs:88-110`).
  `InvariantOutcome::Refolded` (`:2422, 2474, 4489`) retires.

### 7. Deleted and kept

**Deleted.** `ArmPlan` and its methods (`planned_arm`,
`planned_union_variant`, `union_at`, `aligned_decisions`, `:1668-1695`),
`UnionPlan`, `UnionPlan::aligns`, `trace_aligns` (`:1662`), `SupplyKind` and
`UnionSupply::kind`, `build_arm_plan`, `arm_plan_walk` and its
hidden-discriminator filter (`:1828-1870` — RFC 0023 Part C's strip-by-name
is restated single-sided: the merge strips, there is no plan side),
`surviving_entries` (`surviving_indexes` stays for the head), `Merger.plan`
(`:4004`), the `plan`/`path` threading through `oneof_vocab`, `named_vocab`,
`item_vocab`, `normalize_item`, `normalize_spellings` (`:3116-3300`),
`normalize_inlined`, `apply_positional_planned` and the positionalizer's
plan consults (`identity.rs:349-372, 421-436, 492`), `PLAN_LOOKUPS`,
`PLAN_TAMPER`, the token-restatement strip (`:5535-5615`), and the tests
that reach into the plan (the direct-`Merger` corruption test, the
`planned_arm`/`decisions` observables, the three `PLAN_LOOKUPS` tests, the
`PLAN_TAMPER` test — `:12030, 12642-12737, 13568-13574, 13836, 14322`),
rewritten or deleted in Phase 3.

**Kept.** `normalize_for_scan` (plan-free already), `apply_positional`
(defaulting; the positionalizer loses its plan field), `materialize_item`,
the folds, `union_verdict`, `Establishment`, `UnionSupply`,
`Decision`/`DecisionTrace`, `surviving_indexes`. Nothing outside
`layers.rs`/`identity.rs` references the plan (nml-validate, nml-lsp,
`defaults.rs`, `resolve.rs`: zero).

### 8. Complexity

Same order as today: each body is `ThisLevel`-normalized at exactly one
level, each loser's interior `Deep`-normalized once, one fold per level and
per item group; the backstop's memo per group version is untouched. Named
traps: never call the recursive `.shared` distribution per level (one-level
only); never `apply_positional` deep per level (materialize only the
group's lowest survivor); never diagnose a body that also recurses. Gate:
five generated fixtures under `tests/fixtures/layers/perf/` (deep 20×16,
deep 40×8, wide 50×8×7, nest 2000, nest 8000; 0.06 s, 0.10 s, 0.06 s, 0.33 s,
1.35 s in release on 2026-08-29 — absolutes drift ±50% across sessions;
the gate is the same-session two-binary ratio) — `#[ignore]` in-process tests in the
layers test module asserting a coarse bound (`cargo test --release --
--ignored`), and a `just` recipe running `hyperfine --warmup 3 --min-runs 20`
over `nml check` as the precise cross-check (failing loudly when
`hyperfine` is absent), bounded at ±20% for the two cases above 0.2 s
(nest 2000, nest 8000) and 2× for the three faster cases (wall-clock
noise).

### 9. Migration

- **Phase 0 — order contract and fixtures.** `ComposeSink` + the order
  test; the seven mixed-code pins' code-order assertions unchanged (the
  `:12023` pin's test loses its plan assertion, `:12030`, in Phase 3); `H`/`H3`/`NA1` pinned as
  order-changing; the timing fixtures and tests; `K`/`K3` pinned as today
  with `// TODO(0025): flips`. Gate: battery green.
- **Phase 1 — the oracle.** No frozen copy of the engine (a `#[cfg(test)]`
  shadow of ~4,400 lines would drift and share none of the helpers Phases
  2–3 change). Instead: (i) a `#[cfg(test)]` hook in the `compose` test
  helper (`layers.rs:6251`, the funnel for 209 sites; `compose_with`'s
  custom-grant callers, the `compose_file` tests and the direct-`Merger`
  tests are not harvested — the CLI dump covers the fixtures) that, under
  `NML_CORPUS_DUMP=<dir>`, writes every test's `(name, schema, source,
  root, declaration)`; (ii) a hidden `nml check --dump-compose` printing the
  composed `Body` as JSON (`Serialize` exists), the origins (`ComposedFile`
  gains `origins: Vec<(usize, ProvenanceTable)>` — today `compose_file`
  drops them, `:3964` — and `Origin` gains `Serialize`; the harvested test
  name comes from `std::thread::current().name()`), and the rendered
  diagnostics in the sink's order; (iii) a comparison over the harvested
  corpus, the layer fixtures and generated deep/wide/nested/fuzz stacks
  between a binary **tagged at the end of Phase 1** (sink and dump on both
  sides) and the working tree, with a per-test allow-list (`K`, `K3`,
  `B2`, rows 1–2 of the loser table, `M`, `P6`, and the
  mid-layer-switch owner-attribution fixtures — RFC 0023 Part B's
  switching twins).
  Zero engine duplication, exact, and a permanent two-build comparison
  tool.
- **Phase 2 — one normalizer.** `Descend` introduced; `normalize_inlined`
  and `normalize_for_scan` call `normalize_level(Deep)`. Pure refactor.
- **Phase 3 — the merge decides.** (a) `merge_union`/`merge_oneof_bodies`
  always fold locally; the plan feeds normalization only — a standing
  corpus gate asserts the plan's raw fold and the local fold produce equal
  traces at every position (`ArmDecision`, `Establishment` and `SealHit`
  derive `PartialEq`). (b) `compose_root` takes raw inlined bodies;
  `ThisLevel` at the top of `merge_model_bodies`; `Composed.survivors` and
  the three subtraction homes; `emit_deep`; n-ary items with one fold per
  group; `normalize_inlined` deleted. Expected diffs: `K`/`K3` (flip the
  Phase 0 pins), `B2`, rows 1–2, `M`, `P6` (allow-listed). (c) Delete the
  plan and its seams; add `FOLD_TAMPER`, `FOLD_LOG`, `DISCARDS`. Gate:
  oracle, timings, lint.
- **Phase 4 — split** (below). **Phase 5** — keep the corpus comparison as
  a permanent tool; the allow-list becomes the changelog of intended
  differences.

### 10. Module split

A pure move after Phase 3, verified with `git diff -M --color-moved=dimmed-zebra`
and a test count unchanged from the end of Phase 3 (after the plan tests
are deleted): `layers/mod.rs` (API, `resolve_layers`, `compose_file`,
`ComposeSink`), `grants.rs`, `instances.rs`, `policy.rs`, `linearize.rs`,
`entries.rs`, `decide.rs` (folds, verdicts, `Establishment`,
`UnionSupply`, the seams), `seal.rs`, `normalize.rs`, `merge/{mod, union,
items, oneof}.rs`, and `layers/tests/*.rs` by battery (`#[cfg(test)] mod
tests;` under `layers/mod.rs`, each file `use super::super::*`). Items
crossing the `merge/*` boundary are `pub(in crate::layers)`; the public API (`compose_file`, `finding_key`, `FindingKey`,
`OpenContext`, `validate_merge_policies_over`, `check_uses_refs`, among
the module's other pub items) is untouched.

## Errata to RFC 0019 (E18)

`0019:399-411` mandates the discriminator pre-pass "computed before
per-layer materialization … without the pre-pass, step 3 would circularly
depend on step 4's accumulator". The circularity is an artifact of
whole-layer normalization: the RFC's own fold input is the array-ref-
inlined (raw) bodies, so the dependency is one-way. Replacement text:
*array-reference inlining still runs first, per layer, against that layer's
own document (inlining is arm-independent and can itself introduce a
discriminator); each position is then decided at merge time by folding its
inlined supplies (a list level distributes each layer's `.shared` into that
layer's items for the fold's input, yielding to an item's identity token);
survivors normalize under the decided variant, an item's token
materialized into the lowest surviving body before the body merge;
discarded bodies are diagnosed under their own readings at every level.*
Also amended: `0019:205-213`; `0019:370-384` (the step order); plan item 2
(`0019:1521`, "the seal backstop and the discriminator pre-pass");
`0019:771-772` ("`SharedProperty` is consumed by its own layer's
normalization (step 3)" → "at its own list level"); the NML2086 row (drop
"fell back to a local fold and the plan was ignored"); E13's "under a
bracketed item path the plan never writes" → "at their own merge level";
E14's "on every pass — the plan, …" → the merge-time statement. No other
0019 sentence contradicts E18 (searched: pre-pass, plan, step 3,
materializ).

## Diagnostics and error-index changes

| Code | Change |
|---|---|
| NML2086 | second paragraph drops "a planned union position whose supplies no longer match its plan" and the refold tail; the code stays allocated (the editor guard, the `Dropped` sites); hover summary unchanged |
| NML2079 | unchanged contract; the discarded-body rule stated in one sentence ("a discarded body is diagnosed under its own readings") |

## Documentation

- The two index entries; `CHANGELOG.md` (the plan-authority and
  boundary-assertion narrative is superseded); RFC 0019 text per E18;
  `layers.rs` module and function docs that name the plan (`ArmPlan`'s
  doc, `resolve_layers`' step comments, `internal_invariant`'s boundary
  sentence, E13/E14 citations in comments); the `just` recipe for the
  timing cross-check.

## Security considerations

- `.shared` distribution yields to a scalar item's `+` token (§3): a base
  list's `.shared` naming the `+` field would otherwise rewrite every
  item's identity-token field silently. A Named key's `name` stays the
  lenient, validator-injected field it is today — composed and plain files
  keep deserializing alike.
- Arm headers are injected into each arms-typed field's block before the
  arm-set seal judgment reads the displaced set (§2): the judgment keeps
  seeing the injected `name` (`AS1`).
- Loser diagnostics are stack-independent (§4): no dependent's findings
  reveal which arm *another* stack composed a shared base under.
- The seal backstop's judgment path is unchanged; the boundary assertion
  stays provably live (`FOLD_TAMPER`); the editor's never-dark guard is
  untouched.

## Compatibility

- `ComposedFile` gains `origins` (a public struct, for the oracle dump);
  `Origin` gains `Serialize`; `ArmDecision`, `Establishment` and `SealHit`
  derive `PartialEq` (private types). `ArmPlan` is `pub(crate)`; no other
  public change.
- Diagnostic emission order for composed files becomes the sink's total
  order (stack position, then source, span, code, message); the CLI prints
  in that order.
- NML2086's index prose changes; the code and the editor guard stay.
- Behaviour changes, all pinned: `K`/`K3` (item-scope tokens and zero-item
  verdicts under the composed arm), `B2` (NML2043-invalid token
  discriminators no longer switch), rows 1–2 of the loser table (rejected
  and switch-displaced dotted and root bodies diagnosed under their own
  arm), `M`
  (a loser's nested positions under its own readings), `P6` (the authored
  restatement of a sealed `+` is NML2060), owner attribution after a
  mid-layer item switch.

## Test plan

Labels name fixtures under `tests/fixtures/layers/` and the test each
becomes: `K` `item_token_materializes_under_the_composed_arm`, `K2` (the
dotted twin, unchanged), `K3` `item_zero_item_verdict_under_the_composed_arm`,
`C2` `a_shared_discriminator_switches_an_items_arm`, `B2`
`an_invalid_token_discriminator_never_switches`, `P2`/`P3`/`P4b`/`P5b`
`an_items_token_beats_a_list_wide_shared` (asserting the composed value,
not a diagnostic — a `uses` layer's `.shared` NML2008 never fires today for
a scalar-keyed list; it does for a Named-keyed one), `Q2`/`Q7`
`a_list_wide_shared_reaches_a_named_items_name` (unchanged behaviour),
`Q2u` `a_shared_write_to_a_named_items_typed_field_reaches_and_mismatches`
(value + NML2008),
`P6` `an_authored_restatement_of_a_sealed_token_is_nml2060`, `AS0`/`AS1`/`AS2`/`AS3`
`arm_set_replacement_still_sees_the_injected_header_seal`, `U1`/`U2`
`a_losing_lists_items_are_still_diagnosed`, `S1`–`S3` and `A`/`J`/`H2`/`NA1`
(one pin per loser row), `F2` `a_sealed_loser_diagnoses_under_its_own_default`, `R1` (an
oneof-root rejected switch: the loser's `extras = []` warned under its own
arm — the `compose_root` home's pin),
`E`/`D`/`G`/`L1`–`L3` (one pin per discard site), `M`
`a_losers_nested_positions_use_its_own_readings`, `H`/`H3`/`NA1` (order
flips) and `L`/`N` (the order contract), plus: the seven mixed-code pins
unchanged; the `FOLD_TAMPER` liveness test; `FOLD_LOG` has `slot[w].x` and
never `slot.x` for the list-established shape (a named establishment
legitimately folds `slot.x`); the standing raw-fold-equals-local-fold
corpus gate; the five timings; the oracle comparison with its allow-list;
`ComposedFile.origins` dumped.

## Decisions

- The discarded-body vocabulary rule is normative: own readings at every
  level. A merge-side decision log reproducing the plan artifact would
  reintroduce cross-walk key parity — the class this RFC removes.
- The oracle is a tagged-binary dump comparison over a harvested corpus, not
  a frozen engine copy.
- Discard diagnosis is a per-level subtraction at three homes, not twelve
  call sites; `.shared` yields to the identity token instead of the
  token-strip; one fold per item group, owned by the level's authority.
