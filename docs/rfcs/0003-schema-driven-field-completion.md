# RFC 0003 — Schema-Driven Field Completion (nml-lsp)

- **Status:** Implemented (P1 + P2 — top-level/nested/list/`oneof`-variant field completion,
  the shared cursor-context walk, the `find_model_ref_type_at` refactor onto it, parse-once,
  and type+default `detail` hints; 11 new tests, `nml-lsp` green)
- **Builds on:** [RFC 0001 — Schema-Driven Defaulting](./0001-schema-driven-defaulting.md) (complete),
  [RFC 0002 — Shared Body-Aware Dispatch, `oneof` Defaults, Workflow Migration](./0002-visitor-unification-oneof-defaults-workflow-migration.md) (complete)
- **Crates touched:** `nml-lsp` (only) — consumes `nml-core` read-only
- **Supersedes:** the `nudge/TODO.md` item *"oneof: variant-aware LSP completion"* and the
  deferral noted in RFC 0002 §7b — reframed from a `oneof`-only special case to the general
  feature that subsumes it.

## 1. Summary

The language server completes model **names** where a model reference is expected
([`find_model_ref_type_at`]) and the `oneof` discriminator value set
([`find_oneof_discriminator_at`], RFC 0002 A4). It does **not** complete a model's **field
names** inside that model's body — for *any* model. So inside `provider X:` the editor never
suggests `type` / `model` / `temperature` / `baseUrl` / `apiKey`, and the `oneof`
variant-field completion deferred by RFC 0002 §7b has no foundation to land on.

This RFC adds **schema-driven field completion**: when the cursor is at a property position
inside a body whose model type is known, offer that model's not-yet-present fields (with type
and required/optional hints). The three "known body" contexts — a top-level block of a
declared model, a nested model-typed field, and a **resolved `oneof` variant** — are handled
by one mechanism, so variant-field completion falls out for free rather than as a bespoke
special case.

The hard parts already exist in `nml-core`: `SchemaIndex` field lookup and the body-aware
dispatch `resolve_type_in_body` / `resolve_field` → `FieldTarget` (RFC 0002 A1). What is
missing is purely LSP-side: detect *which model's body the cursor is in*, then list its
fields. This is the **dual** of `find_model_ref_type_at` (which resolves a field's value
type); the new detector resolves a body's model type.

## 2. Motivation

- **Capability parity.** Field completion is table-stakes for a typed config language;
  every comparable schema language (JSON Schema / YAML LSPs, Protobuf, CUE, Nickel) offers
  it. We have the schema in memory and don't surface it where authoring happens.
- **Closes RFC 0002 §7b correctly.** Variant-field completion was deferred *because* doing it
  `oneof`-only would duplicate the machinery the general feature needs. The general feature
  is the right home; the `oneof` case is one branch of body-model resolution.
- **Reuses, not rebuilds.** `resolve_type_in_body` (A1) already resolves a body's
  `FieldTarget` — including selecting a `oneof` variant from its discriminator. Field
  completion is "resolve the body's model, then list its fields." No new schema mechanics.
- **DRY with existing completion.** The value-position detector
  (`find_model_ref_type_at`) and this property-position detector share schema-snapshot
  acquisition, `find_enclosing_block_keyword`, and the `CompletionItem` push loop.

## 3. Current state

- **Value completions exist, field completions do not.** The completion handler builds one
  schema snapshot (`models_for_file`) and runs two value detectors: model-ref
  (`find_model_ref_type_at`) and discriminator (`find_oneof_discriminator_at`). Each pushes
  `CompletionItem`s. There is no detector that, given a *property-name* cursor position,
  returns the enclosing model so its fields can be offered.
- **`find_model_ref_type_at` is the near-dual.** It already: reads the cursor line, finds the
  enclosing block keyword (`find_enclosing_block_keyword`), looks up the `ModelDef` by
  keyword, finds the *field* under the cursor, and resolves the field's value type. The new
  detector needs the same enclosing-context walk but resolves the *body's* model rather than
  a field's value type, and triggers at a property position (no `=` before the cursor) rather
  than a value position (after `=`).
- **The schema dispatch is ready.** `SchemaIndex::resolve_type_in_body(ty, body)` returns a
  `FieldTarget` (`Model` / `OneOf` / `ListOf` / `Object` / `Union` / `Leaf`), resolving a
  `oneof` to its concrete variant via the discriminator in the body. `resolve_ref(name)` maps
  a top-level keyword/model name to its target. `ModelDef { name, fields: Vec<FieldDef> }`;
  `FieldDef { name, optional, field_type, default_value }`.

## 4. Design

### 4.1 Body-model resolution (the one new primitive)

Add an LSP-local detector — the dual of `find_model_ref_type_at`:

```rust
// nml-lsp
/// When the cursor is at a property-name position inside a model body: the `ModelDef` whose
/// fields are valid there, **and the body itself** (so the caller excludes already-present
/// fields without re-walking). Resolves the enclosing context to a concrete model — a
/// top-level block (`resolve_ref(keyword)`), a nested model-typed field, or a `oneof` variant
/// selected by the body's discriminator (`resolve_type_in_body`). `None` when the body has no
/// schema (free-form `object`, unknown keyword, or an unresolved `Union`). Takes the
/// already-parsed `&File` (parse-once, §5) — it does not re-parse.
fn find_model_body_at<'a, 'f>(file: &'f File, pos: Position, index: &'a SchemaIndex)
    -> Option<(&'a ModelDef, &'f Body)>;
```

- **Trigger discrimination — an explicit `=` gate, not "value detectors didn't match."** Field
  completion fires iff the cursor is at a **property position**: `line[..cursor].find('=')`
  is `None`. The value detectors fire iff it *succeeds* (`find_model_ref_type_at` opens with
  exactly that check). These are the two halves of one dichotomy, so they never both fire.
  Crucially the gate must be this explicit check, **not** "no value detector matched": at a
  *scalar* value position (e.g. `model = "x|"`) the `=` check succeeds but no value detector
  matches (a `string` field has nothing to complete) — and field completion must **not** fire
  there. Gating on `=`-absence is correct; gating on "value detectors empty" would wrongly
  suggest field names inside a string value.
- **Context walk.** This is the substantive new work, not a thin reuse:
  `find_enclosing_block_keyword` resolves only the **top-level** block a cursor line sits in
  (it iterates `file.declarations` and never descends), so it serves the depth-0 case
  (`provider X:` → fields) directly but nothing nested. The nested/`oneof` cases need a new
  **recursive body-walk** that, from the parsed `File`, descends to the *innermost* body
  containing the cursor while tracking the field that owns each child body — the read-dual of
  the validator's and defaulter's write-walks (same dispatch, no new schema mechanics). It
  then resolves top-down: `resolve_ref(top_keyword)` → at each level, `resolve_field` /
  `resolve_type_in_body` on the owning field → the child body's target. The terminal
  `FieldTarget`:
  - `Model(m)` → return `m`.
  - `OneOf(o)` → resolve the variant from the body's discriminator (this is exactly what
    `resolve_type_in_body` does) → its `Model`. **This is variant-field completion.**
  - `ListOf(inner)` → unwrap to the item target (a list of models completes the item's
    fields).
  - `Object` / `Union` / `Leaf` → `None` (no schema fields to offer — free-form or ambiguous).
- **No new schema API.** The walk composes existing `SchemaIndex` methods. `FieldTarget`
  is unchanged (RFC 0002 §4 deliberately kept it concrete-target-returning, which is exactly
  what a completion provider wants).

### 4.2 The completion branch

In the handler, after the value detectors, add one branch:

```rust
if let Some((model, body)) = find_model_body_at(&file, pos, &index) {
    let present = present_field_names(body);                 // reuses the walk's body — no re-walk
    for field in model.fields.iter().filter(|f| !present.contains(&f.name)) {
        items.push(CompletionItem {
            label: field.name.clone(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(field_detail(field)),                // e.g. "string" / "secret?" / "providerType"
            sort_text: Some(sort_key(field)),                 // required first, then schema order
            insert_text: Some(field_insert_text(index, field)), // `<f> = ` (scalar) / `<f>:` (model/block)
            ..Default::default()
        });
    }
}
```

- **Exclude already-present fields.** A field set once should not be re-suggested. `present`
  is the set of property names already in the cursor's body — read from the body the walk
  already returned (the document must parse for any of this; §4.3).
- **Type/required hints in `detail`.** Render `field.field_type` (+ `?` when `optional`),
  matching how the schema is written — so the author sees `temperature number?`, `apiKey
  secret?`, etc. A field with a `default_value` shows it (`outputFormat string = "text"`).
- **Required-first ordering.** `sort_text` puts non-optional fields first, then schema
  declaration order — the author is guided to the mandatory fields.
- **Type-aware `insert_text`.** A scalar/leaf field is authored `field = value`, but a
  **model-typed field is a block** (`prompt:`, `when:`) and a list/object opens a block too —
  so `field_insert_text` resolves the field (`resolve_field` → `FieldTarget`) and emits
  `"<field> = "` for `Leaf` and `"<field>:"` for `Model`/`OneOf`/`ListOf`/`Object`. A blanket
  `= ` would be wrong for every nested-block field. For the scalar case this lands the cursor
  at the value position, where the existing value completions (model-ref / discriminator /
  enum) then fire — the two features compose.

### 4.3 Error tolerance

Like all the LSP's schema-aware completions, this needs the document to **parse** — every
detector opens with `nml_core::parse(source).ok()?` and `find_enclosing_block_keyword`
operates on the parsed `&File` (there is **no** line-scan fallback today; the draft claim of
one was wrong). So when the file doesn't parse, field completion yields nothing — *identical*
to today's model-ref/discriminator behavior, no crash. The practical consequence is shared by
all completions: while a field name is being typed the line may be momentarily unparseable
(`provider X:\n    ty`), suppressing suggestions until it parses. The fix is **error-tolerant
parsing** — a single shared enhancement that would lift all three detectors at once — and it
is explicitly **out of scope here** (it belongs in the parser/`nml-core`, not this LSP
feature). This RFC neither adds nor removes that limitation; it inherits it.

## 5. Architecture, encapsulation, DRY

- **One new LSP-local function** (`find_model_body_at`) plus small `present_field_names`,
  `field_detail`/`sort_key`, and `field_insert_text` helpers. No new public `nml-core` surface.
- **Parse once per completion, not per detector.** Today each detector calls
  `nml_core::parse(source)` independently (`find_model_ref_type_at` does; there are ~10
  `parse(source)` sites across the server) — so the completion path already re-parses the
  document several times per keystroke. This RFC threads a **single parsed `&File`** (and the
  one schema snapshot) into all three detectors (`find_model_ref_type_at`,
  `find_oneof_discriminator_at`, `find_model_body_at`) rather than adding a fourth parse. That
  is both the clean way to add the new detector and a strict improvement to the existing path
  — the redundant per-detector parses are legacy to remove, not preserve.
- **Reuses every existing primitive**: schema snapshot (`models_for_file`),
  `find_enclosing_block_keyword`, `SchemaIndex::{resolve_ref, resolve_field,
  resolve_type_in_body}`, `FieldTarget`. The body-model walk is the read-dual of the
  defaulter/validator's write-walks — same dispatch, no re-derivation.
- **`nml-core` stays read-only and unchanged.** `FieldTarget` already returns the resolved
  concrete target (RFC 0002 §4), which is exactly the shape a completion provider consumes —
  no enrichment needed, no lifetime entanglement.
- **One shared cursor-context primitive — and it lifts the *existing* value completion too.**
  `find_model_ref_type_at` is today **top-level only**: it resolves the enclosing context via
  `find_enclosing_block_keyword` + a flat `models.find(name == keyword)`, so model-ref value
  completion silently does nothing inside a nested body. But that function *is* "resolve the
  enclosing model → find the field on the cursor line → take its value type" — i.e. it is
  `find_model_body_at` (which resolves the enclosing model at **any** depth) plus a
  field-lookup. So `find_model_body_at` is the shared primitive: `find_model_ref_type_at` is
  refactored to call it and then look up the cursor-line field's `FieldType`. This (a) DRYs
  the two detectors onto one walk, (b) **removes** the legacy top-level-only context path in
  favor of the recursive one (better implementation, ≥ capability — the directive's rule), and
  (c) gives nested model-ref value completion *for free* as a side effect. The walk is the one
  place cursor-context is resolved.
- **The `oneof`-only framing is removed, not preserved.** The superseded TODO/§7b described a
  bespoke `oneof` completion path; this general design subsumes it with equal-or-greater
  capability (it also completes plain model/nested-field bodies), so the special case is not
  built.

## 6. Security considerations

- **Read-only, in-process.** Completion reads the open document and the in-memory schema
  snapshot; it resolves no `$ENV`, runs no I/O, and exposes no values — it offers *field
  names from the schema*, which are already visible in the schema files. No new surface.
- **Bounded.** The context walk is bounded by block-nesting depth, and field resolution
  reuses `SchemaIndex`'s existing depth/lookup bounds (RFC 0002). A hostile or deeply-nested
  document degrades to fewer suggestions, never unbounded work.
- **No secret exposure.** Unlike value completion, field completion never touches a value, so
  the `$ENV`/secret machinery is irrelevant here by construction.

## 7. Testing strategy

- **Top-level block fields**: cursor in `provider X:` body offers `type`/`model`/… ; a field
  already present is excluded; required fields sort first.
- **Nested model-typed field**: cursor in a `prompt:`/nested block offers that model's fields.
- **`oneof` variant fields**: with the discriminator bound (`by provider = "log"`), cursor in
  the body offers the resolved variant model's fields; with an *unset/ambiguous* discriminator
  (`Union`), no field suggestions (graceful).
- **List-of-model**: a list item body offers the item model's fields.
- **Non-schema bodies**: free-form `object` and unknown keywords offer nothing.
- **Trigger exclusivity**: at a value position (after `=`) field completion does *not* fire —
  including a **scalar** value position (`model = "x|"`) where no value detector matches
  either (the `=`-gate, not "value detectors empty"); at a property position the value
  detectors do *not* fire.
- **Parse-failure**: an unparseable document yields no field suggestions (no crash) — matching
  the existing detectors; pins that we inherit, not worsen, the shared limitation.
- **`detail`/`sort` formatting**: `optional` renders `?` (the NML type via `FieldType`'s
  `Display`), the schema default renders as `= <value>` when present (`render_scalar` — schema
  defaults are always scalars), and required-first ordering holds.

## 8. Phasing

No migration, no cross-crate change — but two honestly-sized steps, because the depth-0 and
nested cases differ in cost:

- **P1 — top-level field completion. DONE.** The property-position `else` branch in the
  completion handler builds one `SchemaIndex` from `models_for_file`, parses once, and calls
  `find_model_body_at` → offers the resolved model's not-yet-present fields with
  `field_insert_text` (type-aware `= `/`:`), `field_detail` (type + `?`), `field_sort_key`
  (required-first), and `present_field_names`. 5 tests.
- **P2 — nested + list + `oneof`-variant field completion, on a shared walk. DONE.**
  `find_model_body_at` recursively descends (`descend_to_cursor`) through nested model-typed
  fields, list-of-model items (`steps:` → `- step:`), and `oneof` variants
  (`resolve_oneof_variant` reads the body's discriminator/default → the arm's variant model) —
  closing the RFC 0002 §7b variant-field deferral. In the same step, `find_model_ref_type_at`
  and `find_oneof_discriminator_at` were **refactored onto the shared walk / `SchemaIndex`**
  (parse-once; the value branch parses + indexes once for both), removing the top-level-only
  `find_enclosing_block_keyword` + `models.find` legacy and lifting model-ref value completion
  to nested contexts. 5 tests incl. the nested-body model-ref regression.

The schema foundations (`SchemaIndex`, `resolve_type_in_body`, `FieldTarget`) were built by
RFC 0002 precisely so the *resolution* is free; the LSP work is the context-walk and the
completion plumbing. P1 is genuinely small; P2's recursive walk is the real (still modest)
implementation cost — framing it as one trivial phase would have undersold it.

## 9. Risks

- **Trigger detection** (property vs value position) must be precise so field and value
  completions don't both fire. Mitigated by the mutually-exclusive `=`-on-line rule and a
  test pinning both directions (§7).
- **Deep-nesting resolution** depends on the document parsing — as do *all* existing
  schema-aware completions (they share the `parse(source).ok()?` requirement). On parse
  failure none fire; this is inherited behavior, not a regression (no field completion exists
  today), and the shared fix is error-tolerant parsing (out of scope, §4.3).
- **Scope creep into error-tolerant parsing.** Explicitly out of scope: error-tolerant
  context detection is a separate enhancement that would improve all three detectors together
  and should not be bundled here.
