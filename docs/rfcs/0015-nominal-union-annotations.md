# RFC 0015 — Nominal Union Annotations (`as <Variant>`)

- **Status:** Implemented
- **Date:** 2026-07-23
- **Crates:** nml-core (parser, lowering, schema_index, de, diff), nml-fmt,
  nml-lsp, nml-validate

## Summary

A block instance of a union-typed field may name its variant explicitly:

```
host H:
    slot as modelB:
        b = "x"

    slots:
        - one as modelB:
            b = "x"
```

Same-class unions — two or more *model* variants whose instances are all
keyed blocks (`(modelA | modelB)`) — were previously resolved by structural
first-wins, which silently guessed. This RFC makes the variant **stateable**
(`as <Variant>`), and makes the ambiguous case **unrepresentable**: a
same-class instance with no annotation whose body shape cannot choose is a
hard error (D2, `NML2052`), never a guess.

## Design

**The annotation rides the `Body`.** Lowering stores it as
`Body.type_annotation` — the single input `resolve_type_in_body` already
takes — so the one canonical resolver honors `as` and every union-variant
consumer (validator, defaulter, identity walk, LSP, differ) selects through
it with no per-consumer threading. The deserializer and formatter read the
field directly for their own concerns (a synthesized serde tag; re-emitted
`as` text); they don't resolve variants, so they cannot diverge.

**Two constructors, one invariant.** `Body::fresh(entries)` is for *fresh*
bodies only (empty seeds, synthesized instances); a body derived **from an
existing body** must rebuild through `Body::with_entries(entries)`, which
preserves the annotation. This is load-bearing: the deserialize pipeline
(`apply_positional → apply_shared_properties → apply_defaults → resolve_body
→ from_block`) rebuilds bodies at every pass, and a single fresh construction in
that chain silently strips the annotation — the defaulter then defaults the
*wrong* variant and the deserializer never sees its tag. (This was a real
found-in-review bug: the unit tests passed because they ran each pass on a
freshly-parsed body; only the full-pipeline test caught it.) The invariant
is pinned by `annotated_union_survives_the_full_pipeline`, which
mutation-verifiably fails if any on-path transform reverts to a fresh
construction. The fresh constructor is deliberately named `fresh` — not `new` —
so the attractive-default name that caused the original bug does not exist.

**One nominal-selection primitive.** `SchemaIndex::select_variant_by_type_name`
selects exactly (declared type name, membership-checked); the resolver
selects with it, the validator checks membership with it, and completion and
the did-you-mean draw from its counterpart `nameable_variant_names` (source
order). Only *nameable* variants qualify — model/`oneof` refs; disjoint
list/scalar variants are structurally unambiguous and deliberately not
nameable.

**`as` narrows, never casts.** The annotated body is *validated against* the
named variant (`nominal_annotation_is_checked_not_a_cast`); an unknown
variant is `NML2051` with a machine-applicable did-you-mean; a stray
annotation on a non-union field or element is `NML2053` — flagged, never
silently ignored (`union_variants` unwraps modifier wrappers so a
modifier-typed union never false-positives).

**Fail-closed ambiguity (D2).** With ≥2 nameable variants and no annotation,
a keyed/empty body is `NML2052`. A shape matching *no* variant (a list body
under a model-only union) is `NML2032`, closing the silent-drop hole.
Disjoint unions (e.g. the shipping `(step | []step)`) have one nameable
variant and never trigger D2 (`disjoint_union_never_triggers_d2`). After any
union-level error (2051/2052/2032) the instance is **not** validated against
a guessed variant — one finding, no unknown-property pile-on (the
`materialize_item` no-noise rule). Modifier-wrapped unions (`|slot (a | b)`)
resolve through the same body-aware path — `resolve_type_in_body` unwraps
`Modifier` exactly as `resolve_type` does.

**Deserialization: a synthesized external tag.** An annotated block presents
itself to serde as the single-variant enum access `{ <Variant>: body }`, so
a block-valued union deserializes into a Rust enum *as the annotated
variant* — standard externally-tagged handling, `#[serde(rename)]`
respected, unknown tags erroring canonically — replacing untagged try-first,
which could not honor the annotation
(`deserialize_honors_nominal_annotation_as_external_tag`).

**The differ sees variant switches — at both levels, for every nameable
pair.** The annotation is part of a body's structural identity, and each diff
side resolves its own variant. One owner (`diff_variant_switch`) serves the
field level and paired list elements: a switch between any two nameable
variants — model↔model, model↔oneof, oneof↔oneof — emits an explicit
`as A → as B` witness (absent sides get no phantom witness, the
oneof-flip rule) plus precise per-side removes/adds through each side's own
target, even when both bodies are entry-less or the variants differ only in
defaults. Shape transitions (keyed ↔ list ↔ arms ↔ inline value) surface the
orphaned side in its own representation before the winning form renders
(`orphan_side_visibility`). An entry-less annotated body still counts as a
representative when choosing the resolution body.

**Set identity is the NAME, not the annotation.** In `set<(a | b)>`, two items
with the same name are duplicates (`NML2030`) regardless of annotation
agreement, and the differ pairs elements by name — an annotation change on a
set element is an in-place content change (and an explicit `as a` → `as b`
switch entry), never an add/remove pair. One identity rule across validator
and differ.

**Formatter round-trip is load-bearing.** fmt reconstructs from the AST;
dropping the annotation would silently re-disambiguate a same-class union.
One emission point serves field and list-element headers; pinned by
`roundtrip_preserves_nominal_annotation`.

**Editor support.** In the `as`-type slot (`<field> as <partial>`), the LSP
completes the union's nameable variants — the same candidate set the
validator checks — via a pure, unit-tested line detector
(`as_position_field`).

**The ambiguity UX (F4) — one oracle, three tiers.** The D2 rule lives in ONE
place, `SchemaIndex::ambiguous_union_variants` (annotation-less +
`keyed_or_bare` shape + ≥2 nameable variants, models AND oneofs via
`NameableVariant`), consumed by both the validator and completion so editor
and checker cannot disagree. On it ride three tiers, with the validator the
sole authority:
*discover* — inside an ambiguous body (including the just-typed EMPTY one;
nested-block descent uses editor-grade containment) completion offers the
UNION of all candidates' fields: variant-unique fields first, grouped by
variant with provenance; shared fields after, scaffolded only on
type-AND-form agreement (modifier-ness is compared explicitly — the type
Display erases the `|` wrapper); a oneof candidate contributes its
discriminator. Value positions (`name = ⌖`) are governor-MERGED the same
way: every declaring candidate's field contributes its enum variants and
model-ref declarations (first-wins would let one variant's `string`
suppress another's enum), a oneof candidate's discriminator completes its
arm keys, and a union-typed field's own value admits every model-ref
member's declarations. Modifier fields participate as authored: `|vis = ⌖`
strips the sigil for the lookup but requires the declared form to match
(sigil ⟺ modifier-typed, fail-closed both ways).
*resolve by choice* — a variant-unique pick carries an `additionalTextEdits`
inserting ` as <Variant>` at the header name (auto-import pattern; announced
via `labelDetails`, capability-gated; eager-safe because the edit lies
strictly above the cursor).
*repair* — D2 carries one machine-applicable `Fix` per candidate (capped at
8) anchored at the name token — but ONLY where the annotation is grammatical
and meaning-preserving (field headers, Named items); reference/shorthand/role
items get the block-form message instead. Fix alternatives are mutually
exclusive and never `isPreferred` (an editor auto-applying one would
resurrect the forbidden guess); a typo'd annotation (the limbo state) routes
through the same union-of-fields for keyed/empty bodies — a list-shaped body
is structurally resolvable regardless of its bad annotation and gets no
field discovery — never first-wins.
The suggestion channel itself widened for this: `Diagnostic.suggestions` is a
Vec with `SuggestionKind::{DidYouMean, Fix}` — the kind's axis is
exclusivity, singular did-you-mean rendering unchanged.

**Every unresolved body is a discovery moment — and every announcement is
honest.** A PRE-DISCRIMINATOR `oneof` body (plain field, `[]oneof` element,
or a union body annotated/resolved to a oneof variant) surfaces the oneof as
its single candidate instead of dying: the discriminator field is offered
(sorted first — the most discriminating pick a oneof has) and its arm keys
complete at the value position, including the typo state (`kind = lgo`
offers the repair) and the SWITCHING state (a valid authored `kind = "log"`
still completes every arm — the resolved body keeps its oneof context). A body resolved through a oneof's DEFAULT keeps the
discriminator visible as a settable knob (`discriminator of `` `mail` `` =
"log" (default)`, sorted after declared fields) — field parity: completion
shows defaulted knobs exactly as it shows defaulted fields — yielding to any
authored entry form and to a variant field shadowing the name. The `as`
announcement obeys one rule at every site: it appears exactly when an
annotation edit is attached — never where `as` would be a stray (a plain
oneof field or element, NML2053), and never as a byte-identical no-op (an
annotation already naming the variant).

**Body shape is classified once.** `BodyShape` (arms / list items / keyed) is
the single constructor behind the resolver's variant selection, the
validator's gate, and the ambiguity oracle — the facts are shared; consumers
keep their own dispatch.

**Grammar.** `as` is a contextual keyword (as in `oneof … as <enum>`),
recognized in the header type slot before `at_field_type` — reserving `as`
as a field *type* name (deliberate), while a field or property *named* `as`
is unaffected. On a list element, an annotation marks an inline instance, so
`- Name as V` lowers as Named (empty body) even without a colon-body — the
annotation is never dropped into a bare reference.

## Codes

`NML2051` unknown union variant (did-you-mean) · `NML2052` ambiguous union
instance (D2) · `NML2053` stray type annotation — all with CI-verified index
examples.

## Documentation

Error-index sections for 2051–2053 (this commit). Language-guide and spec
sections for `as` remain to be written — tracked, not silently skipped.
