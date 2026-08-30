# RFC 0005 — Name-Injection Consistency and the Positional Field Marker (`+`)

- **Status:** Implemented (incl. Revision 1, 2026-07-04 — the shipped sigil is `+`, not the
  `!` this document's prose originally used; see the revision note below)
- **Builds on:** [RFC 0001 — Schema-Driven Defaulting](./0001-schema-driven-defaulting.md) (complete),
  [RFC 0002 — Shared Body-Aware Dispatch + `oneof` Defaults](./0002-visitor-unification-oneof-defaults-workflow-migration.md) (complete)
- **Crates touched:** `nml-core` (lexer/`syntax`/parser/`cst::ast`/`cst::extract`/`model`/`schema`, a new
  identity-materialization pass), `nml-validate` (honor name-injection — fixes the false positive),
  `nml-fmt` (render the `!`/`?` suffixes). Downstream consumers (`nudge`) gain the `!` marker and may
  delete bespoke scalar-shorthand serde adaptors.
- **Keeps (deliberately):** `de`'s `NamedItemDeserializer` — it is the **name**-injection mechanism, a
  different job from `!`, and the runtime fallback for models that don't declare `name`.
- **Removes (legacy):** the never-implemented `<shorthand>` field annotation.
- **Fixes:** the false `missing required field 'name'` on named items (a *name*-injection gap); the
  absence of any working scalar shorthand (`<shorthand>` never worked).
- **Related, intentionally separate:** position-aware go-to-definition in `nml-lsp` (§14).

> **Revision 1 (IMPLEMENTED 2026-07-04): the positional marker is `!` → `+`.** The mechanism is unchanged;
> only the sigil moves. Wherever this document writes `!` (a positional marker) or `?!` (optional
> positional), read `+` and `?+`. Rationale, feasibility (lexer collision check), and the exact migration
> are in [§16 — Revision 1](#16-revision-1--positional-marker-change).

## 1. Summary

This RFC cleanly **separates two mechanisms that were previously conflated**:

- **Name (identity).** A *named* declaration — a list item `- editor:` or a block `resource Foo:` —
  carries a name, which fills the model's `name` field. `de` already does this
  (`NamedItemDeserializer`); the **validator does not**, which is the false-positive bug. Fix: make the
  validator honor the same rule. The `name` field is an ordinary field — the named form fills it.

- **Scalar shorthand (`!`).** A *new* type suffix marks the one field a **bare scalar** list item
  (`- "/api"`) fills, so the common case is terse. This replaces the dead `<shorthand>` annotation. It
  is materialized into an explicit property by a schema-driven pass, so validation and `de` agree.

They are **orthogonal**, and a model may use both — the case that forced the separation:

```
model resource:
    name string?          # ← named form fills this:  resource MyAppResource:  → name = "MyAppResource"
    path path!            # ← scalar form fills this:  - "/app/resource"        → path = "/app/resource"
    method httpMethod = "GET"

resources:
    - "/app/resource"                 # path only — anonymous, concise
    - MyAppResource:                  # name = "MyAppResource"
        path = "/app/resource"
```

`name` (the handle) and `path` (the shorthand) are *different fields*. They coincide only when a model's
identity *is* its name (e.g. `role`, `step`), where you put `!` on `name` so the scalar form fills it
too (`name string!`).

## 2. Motivation

- **Correctness (name).** `- editor:` against `model role { name string … }` reports
  `missing required field 'name'` because the **validator** discards the item's name — yet `de` injects
  it and the same input deserializes fine. The two layers must honor one rule.
- **Capability (shorthand).** Terse list authoring — `- "/api"` instead of a named block — is real
  ergonomics, but `<shorthand>` never worked (it doesn't even lex). `!` makes it explicit, declared,
  and tooling-visible.
- **Separation of concerns.** A handle (`name`) and a terse scalar value (`path`) are different things;
  a resource wants the path terse *without* forcing a name. One marker for each, not one for both.
- **Remove dead syntax.** `<shorthand>` is replaced by a working, general marker.

## 3. Current state (as of this RFC)

- **`nudge` consumes NML through `de` (serde).** `nudge/src/types.rs` is wall-to-wall
  `#[derive(Deserialize)]` (with hand-rolled adaptors at types.rs:1077–1127), so `de`'s behavior *is*
  nudge's runtime behavior.
- **Name injection lives only in `de`.** [`NamedItemDeserializer`](../../crates/nml-core/src/de.rs#L383-L435)
  injects a named item's key into a property **`name`** when the body has none (`has_explicit_name`,
  de.rs:396-409). The **validator** ([`validate_array`](../../crates/nml-validate/src/schema.rs))
  validates the *body* and discards the name — so its required-field scan flags `name` as absent. That
  asymmetry is the bug.
- **`Shorthand(val)` deserializes to the bare value** ([de.rs:366](../../crates/nml-core/src/de.rs#L366)) —
  there is no schema-directed target, so a scalar can't fill a named field today.
- **`<shorthand>` is not real.** `path path <shorthand>` produces five parse errors (`<`/`>` hit the
  lexer catch-all) and `cst::extract` silently drops it.
- **No model-level marker.** [`FieldDef`](../../crates/nml-core/src/model.rs) carries no shorthand flag.

## 4. Goals / Non-goals

**Goals**
- **Part A:** make name-injection (named key / block name → `name`) consistent across `de` *and* the
  validator, fixing the false positive. Keep `NamedItemDeserializer`.
- **Part B:** a `!` suffix marking the single field a **bare scalar** list item fills; materialized by a
  schema-driven pass so validation and `de` agree. `?!` = optional shorthand.
- `name` and `!` are independent fields; a model may declare both.
- Delete the dead `<shorthand>` annotation.

**Non-goals**
- Coupling `de` to the schema for value/secret resolution — `de` stays value/secret-agnostic
  (RFC 0001 §5). The shorthand pass injects *structure*, not resolved values.
- More than one `!` field per model (a scalar fills one field — §8).
- Go-to-definition precision (`nml-lsp`) — §14.

# Part A — Name-injection consistency

## 5. The name mechanism

A **named declaration** carries a name that fills the model's `name` field:

- list item `- editor:` → `name = "editor"`
- block `resource MyAppResource:` → `name = "MyAppResource"`

`name` is an **ordinary field** (`string`, optionally `?`). Nothing special about its type — only that,
by convention, it is the field the named form fills.

**Where name-injection happens — one source of truth, one fallback (state this plainly).** Name
injection is schema-aware, so its **source of truth is the materialization pass** (§10): for any model
that *declares* `name`, the pass injects the identity into it — from a **list item key** (`- editor:`)
*and* from a **block declaration name** (`resource Foo:`) — so the validator and `de` both see `name`
as a present property and agree by construction. `de`'s `NamedItemDeserializer` is **kept** as the
runtime **fallback** for the one case the pass cannot serve: models that declare **no** `name` field
(e.g. today's `model step`, whose name is read at runtime for `goto`/`next` but never declared). Where
`name` *is* declared, the pass fills it first and `NamedItemDeserializer` no-ops (`has_explicit_name`).

This is a deliberate DRY tradeoff, not an oversight: keeping the fallback avoids forcing every named
model to declare `name` and avoids a risky deletion (§14), at the cost of the "named key → `name`" rule
living in two implementations. They are held in agreement by the broad agreement test (§11.10), which
**must cover pass ↔ `NamedItemDeserializer` parity on the injected name**, not only validator↔`de`.

**Block-name injection is net-new.** `NamedItemDeserializer` handles **list items only**; `de` has no
block-name injector today. So `resource Foo:` → `name = "Foo"` is the **pass's** job (it is schema-aware
and covers list items *and* block declarations uniformly — the strongest reason the pass owns name at
all). `de`'s block path sees the pass-injected `name` because it runs **post-pass**; no `de` change is
needed for blocks.

**An explicit value wins — injection is lenient, not a double-set error.** Today `has_explicit_name`
*silently prefers* an explicit `name` (de.rs:396-409). The validator/pass does the **same**: the
identity token (key, block name, or scalar) is a *default* the author may override, so a body that sets
`name` (or the shorthand field) keeps its explicit value and the token is dropped — **no error**. This
is deliberate: a strict double-set error would make the *validator stricter than `de`* (rejecting input
`de` accepts), breaking the validator/`de` agreement this RFC depends on (§11.10), and it would remove
the legitimate "identifier as a reference handle, distinct from a `name` field" pattern (`widget Gizmo:`
with an explicit `name = "gizmo"`). Lenient keeps both in agreement and preserves that capability.

Net: this is the entire false-positive fix — for a declared-`name` model the pass makes `name` present,
so the validator's required-field scan passes. Undeclared-`name` models keep working unchanged via the
fallback; they simply aren't *validated* on the name until they declare it (a `name string` one-liner
buys completion/validation/go-to-def on references — recommended, not forced).

# Part B — The scalar shorthand marker `!`

## 6. The `!` suffix

A `!` after a field's type marks it the model's **scalar-shorthand field** — the one field a bare
scalar list item fills. It is a sibling of `?`, and the two compose:

| Field form | Meaning                                              |
| ---------- | ---------------------------------------------------- |
| `f T`      | required                                             |
| `f T?`     | optional                                             |
| `f T!`     | shorthand (required)                                 |
| `f T?!`    | shorthand **and** optional (§7)                      |

**Semantics.** A bare scalar item fills the `!` field:

- `- "/api"` → `path = "/api"` (rest default)
- `- "/admin": method = "POST"` → `path = "/admin"`, body fills the rest

The marked field is **not** `name` in general — it is whatever carries the terse value (`path` for
resource). It coincides with `name` only when the model's identity *is* its name (`name string!`), so
the scalar form `- "viewer"` fills `name` just as `- viewer:` does. The scalar is type-checked against
the field's type by the ordinary path (no second implementation).

## 7. `?!` — optional shorthand

`?` and `!` answer independent questions, so they compose — and the separation in Part A is what makes
`?!` meaningful:

- `!` — "which field does a bare *scalar* item fill?"
- `?` — "may a *named* item omit it?"

Because a named item fills `name`, **not** the `!` field, the `!` field is naturally absent for a named
item — so `?` can fire:

```
model step:
    name string?
    command string?!        # scalar fills command; a named step may omit it
    next step?
steps:
    - "ls -la"               # command = "ls -la"  (anonymous)
    - build:                 # name = "build", command ABSENT (routing-only)
        next = "deploy"
```

`path path!` (required) means a named instance must still set the field in its body; `command string?!`
(optional) means it may omit it. `?!` is **canonical**; `optional` and `shorthand` are independent
booleans, so the AST is order-free and `fmt` always emits `?!` with zero normalization code. (No
keyless-block grammar is needed — the *named* form is the form that omits the shorthand.)

## 8. Constraint — one `!` field per model

A bare scalar supplies one value, so a model may declare **at most one** `!` field. A second is a
schema-load error, beside the existing inheritance/`oneof` checks (`nml-core::schema`):

> `model 'X' declares more than one shorthand field ('a', 'b'); a bare scalar fills a single field`

Inheritance is resolved before the check. (`name` is unaffected — it is not a `!` field unless the
model explicitly marks `name string!`.)

# Shared mechanics

## 9. Inline items and the grammar

The two inline-*definition* kinds carry a key that routes by **type** — an *identifier* names (→ `name`,
Part A), a *scalar* is data (→ the `!` field, Part B). The only grammar/AST change is that the **scalar
kind gains an optional body** so `- "/admin": method = "POST"` becomes expressible. `Named` is
unchanged:

```rust
Named     { name: Identifier, body: Body }              // - editor:        (body always present)
Shorthand { value: SpannedValue, body: Option<Body> }   // - "/api"  /  - "/api": <body>
Reference (Identifier)                                  // - editor         (link)
Role      (String)                                      // - @role/admin    (link)
```

**Why two kinds, not one merged `InlineItem { key, body? }` (decided against during implementation).**
Merging them would *not* eliminate the ident-vs-scalar distinction — every routing consumer (de's
name-injection, `materialize_item`, the union-shorthand check, fmt) would just match it one level
deeper, inside a `key` enum. Worse, a merged shape can't express the grammar truth that an *ident* key
**always** has a body while a *scalar* key's is optional: a single `body: Option<Body>` makes the
illegal state `Ident + None` representable (a dead match arm in every ident consumer), violating
"make illegal states unrepresentable." The only type-precise merge re-creates the two kinds inside a
wrapper. So the two-kind shape is *more* type-precise and flatter to consume, for the identical
capability and far less churn (Shorthand-only sites; the LSP/de `Named` paths untouched).

`Reference` (`- editor`) and `Role` (`- @role/admin`) stay distinct — they are *links*, never
materialized. The **colon disambiguates definition from reference, exactly as today**
([ast.rs](../../crates/nml-core/src/ast.rs)):

```
ListItem  := Named | Shorthand | Reference | Role
Named     := "-" Ident ":" NEWLINE INDENT Body                  # ident def   - editor:   → name
Shorthand := "-" ScalarLiteral ( ":" NEWLINE INDENT Body )?     # scalar def  - "/api"    → ! field
Reference := "-" Ident                                         # link         - editor
Role      := "-" "@" QualifiedName                             # role ref     - @role/admin
```

**Routing follows the key type:** an **ident** key (`- editor:`) → the `name` field (Part A); a
**scalar** key (`- "/api"`) → the `!` field (Part B) — "identifiers name, literals are data." This is
the one implicit rule, so make it visible on a single model where the two diverge:

```
model resource:                        # name string?  +  path path!
    name string?
    path path!

resources:                             # instances under a []resource field:
    - MyApiResource:                   # ident key  → name = "MyApiResource"
        path = "/api"
    - "/health"                        # scalar key → path = "/health"  (anonymous)
```

(They *coincide* only on a model whose `!` is on `name` — `model role { name string! }` — where
`- editor:` and `- "editor"` both land in `name`.)

## 10. The materialization pass

A schema-driven normalization pass in `nml-core`, reusing `SchemaIndex` and the **bounded** body-aware
dispatch of RFC 0002 to resolve a list field's element model. It injects an instance's identity into the
declared field, as an explicit property, before validation and `de`:

> - **ident key / block name** → the element model's `name` field, if declared;
> - **scalar key** → the element model's `!` field, if declared.
>
> Primitives, references/links, and a token whose target field isn't declared are left untouched.

**Why this, not `de`-only:** materializing upstream means the **validator** sees `name`/shorthand as
present (fixing the false positive and giving shorthand a real target) and `de` deserializes the
ordinary body→map shape, staying value/secret-agnostic. The pass-is-source-of-truth /
`NamedItemDeserializer`-is-fallback split, and why it is kept, is spelled out in §5.

**Invariants (load-bearing — do not weaken):**

- **Canonical entry, not optional.** The pass runs *inside* the single `normalize → validate` /
  `normalize → deserialize` choke point all callers funnel through (the entry RFC 0001's defaulting
  already lives in, defaults.rs:63-64). Make `apply_shared_properties` / `apply_defaults` / this pass
  **crate-private** and export only the entry, so no caller can validate/deserialize an un-normalized
  AST. (Verified: nothing outside `nml-core` calls those passes.)
- **Order encodes precedence:** `materialize → apply_shared_properties → apply_defaults → resolve`. The
  instance's own token beats a shared property beats a default; defaulting never sees the field absent.
- **An explicit value wins — injection is lenient (no double-set error).** When a body already sets the
  target field, the identity token is a default that yields to it (`- "/api": path = "/other"` → `path
  = "/other"`; `widget Gizmo:` + `name = "gizmo"` → `name = "gizmo"`). This matches `de`'s
  `has_explicit_name` exactly, so the validator never rejects input `de` accepts (preserving §11.10
  agreement) and the "identifier as a reference handle, distinct from `name`" pattern stays expressible.
  See §5.
- **Dropped-key diagnostic — scalars only.** A **scalar** item whose element model declares no `!` field
  emits `the value has no shorthand field on model 'M' and would be dropped` — a genuine loss with no
  fallback. An **ident** item whose model declares no `name` is **not** a drop: it is the intended
  `NamedItemDeserializer` fallback (e.g. `model step`), so it is silent (the name is still injected at
  runtime). Declaring `name` is an opt-in upgrade (tooling/validation), never forced by a diagnostic.
- **Token span preserved.** The injected property carries the token's source span, so type errors point
  at the item.
- **Bounded + semantic-AST-only.** It inherits RFC 0002's `MAX_DEFAULT_DEPTH` dispatch (untrusted
  input, no fresh recursion) and mutates only the lowered AST — the CST is untouched, so `fmt`
  round-trips `- "/api"` verbatim.
- **`oneof` elements out of scope (v1).** A union element's variant isn't known until the discriminator
  is resolved (in `apply_defaults`, after this pass), so a token on a union-typed list is a diagnostic
  (`shorthand is not supported on union-typed lists; specify the variant explicitly`), not an undefined
  interaction.

## 11. Implementation across the pipeline

1. **Lexer / `SyntaxKind`:** add a `Bang` token for `!` (today it hits `ErrorToken`).
2. **Parser:** accept `?`/`!`/`?!` after a field type; accept `- "scalar": <body>` (scalar-key-with-body);
   keep a bare `- ident` a `Reference`.
3. **`cst::ast` / `model::FieldDef`:** expose `shorthand()` beside `optional()`; add `shorthand: bool`.
4. **`cst::extract`:** set `shorthand` from `!`.
5. **`nml-core::schema`:** add the at-most-one-`!` check (§8).
6. **`nml-core` — the pass (§10):** materialize ident→`name` / scalar→`!`, first in the pipeline,
   crate-private behind the canonical entry.
7. **`nml-validate`:** validate the **post-pass** AST. Because the pass has *injected* `name`/shorthand
   as present properties (lenient — an explicit value wins), the required-field scan needs no bespoke
   name handling — the discard is gone and the false positive with it. The validator surfaces the pass's
   only diagnostic, **dropped-key**.
8. **`nml-core::de`:** **keep** `NamedItemDeserializer` as the runtime name fallback (undeclared-`name`
   models); it no-ops where the pass already injected (`has_explicit_name`), and its lenient
   silent-preference for an explicit `name` is unchanged. A **bodyless** `Shorthand` deserializes as its
   value (as before); a `Shorthand` **with a body** errors until the shorthand pass normalizes it (de is
   schema-blind, so it cannot place the scalar itself).
9. **`nml-fmt`:** render `?`/`!` from the field's flags (CST-based; materialization never reaches it).
10. **Agreement test, broadly:** for every model using `name` and/or `!`, an instance that validates
    clean also deserializes clean with identical fields populated. Two name-specific cases guard the
    twin injectors (§5): for a **declared-`name`** model, the pass-injected `name` must equal what
    `NamedItemDeserializer` would inject (parity; `de` no-ops); for an **undeclared-`name`** model
    (`step`), `NamedItemDeserializer` must still inject the name at runtime (fallback intact, even
    though validation has no `name` field to check).

### 11.1 Implementation status

**Landed** (workspace green, clippy-clean):
- The `!` marker end-to-end — `SyntaxKind::Bang`, lexer, parser (`?`/`!`/`?!`), `cst::ast::FieldDef::shorthand()`,
  `model::FieldDef.shorthand`, `ast::FieldDefinition.shorthand`, set in `cst::extract` + `cst::lower`.
- Schema-load `find_shorthand_errors` (at most one `!` per model, post-inheritance) — `nml-core::schema`, wired in the loader.
- `nml-core::identity` — `materialize_named` (named key / block name → `name`) and `materialize_item`
  (+ scalar → `!`) → `Materialized { body, diagnostics, validatable }`, the single rule. Injection is
  **lenient** (explicit value wins, matching `de`); the only diagnostic is **dropped-key**. Unit-tested.
- Validator — **list-item *and* block-declaration** identity materialization before the required-field
  scan (`validate_array`/`validate_list_item` + `validate_block`, via one `apply_materialization`
  helper): false positive fixed, dropped-key emitted without noise (`validatable=false` skips the scan),
  scalar-on-union flagged. Tested (incl. block-name, explicit-wins, coincide cases).
- `nml-fmt` renders `?`/`!`/`?!`; round-trips.
- **Scalar-key-with-body** — `Shorthand { value, body: Option<Body> }` (the *minimal*, type-precise
  alternative to the §9 merge), parser accepts `- "/api": <body>`, `materialize_item` injects the scalar
  into the item's body, fmt round-trips it. Tested end-to-end (parse → validate fills `path` + checks the
  body → fmt).
- **de-path** — `nml-core::identity::apply_positional` (the schema-driven pass) runs **first** in
  `from_body_defaulted`, materializing every scalar list item into a body; `de` then deserializes a
  materialized scalar's body as a struct (via `NestedBlockDeserializer`). So `- "/api"` and
  `- "/api": <body>` deserialize into the element struct. The transitional "validates but `de` errors"
  gap is **closed** — guarded by the broad validator/`de` **agreement test** (same instance validates *and*
  deserializes with matching fields).
- **Fixture** `core.model.nml` repurposed to valid RFC-0005 syntax (`path path!`, `name string!`,
  `run string?!`) and guarded by a parse+extract test.

**Status: complete.** Every layer (lexer → parser → extract → schema-load → materialization → validator
→ fmt → de) is implemented and green; the RFC is fully realized.

## 12. Examples

```
# Separate name + shorthand (the case that forced the split)
model resource:
    name string?
    path path!
    method httpMethod = "GET"
resources:
    - "/health"                # path = "/health"        (anonymous, terse)
    - MyAppResource:           # name = "MyAppResource"
        path = "/app/resource"

# Identity IS the name → put ! on name; scalar and named both fill it
model role:
    name string!
    description string?
[]role roles:
    - editor:                  # name = "editor"
        description = "Editing"
    - "viewer"                 # name = "viewer"
    - @role/admin              # role reference

# Optional shorthand
model step:
    name string?
    command string?!
    next step?
steps:
    - "ls -la"                 # command = "ls -la"
    - build:                   # name = "build", command absent
        next = "deploy"
```

## 13. Migration

- **`label → name`.** `role.label string?` → `role.name string!` (identity = name; scalar form also
  fills it). For models like `resource`, add `name string?` only if a handle is wanted; the shorthand is
  `path path!`.
- **Keep `NamedItemDeserializer`; teach the validator the same rule.** The false positive is fixed by
  the validator honoring name-injection (via the pass) — *not* by removing `de`'s injector. No risky
  deletion, no forced `name string!` everywhere. `model step` keeps working as-is; declaring
  `name string` later is an opt-in tooling upgrade flagged by nothing (its name is read at runtime via
  the fallback).
- **Adopt the pass in nudge's pipeline.** Once present, scalar shorthand deserializes with no bespoke
  serde; audit `string_or_seq_socket_addr` / `StringOrSeq` and delete any that only bridged
  scalar-or-struct list items.
- **The "duplicate `role`" is not an authority contest.** `nudge/src/schema/project.model.nml` is
  authoritative; `nml/tests/fixtures/valid/models/core.model.nml` is an **orphaned, non-parsing**
  fixture (no test loads it; still has the dead `<shorthand>`). Repurpose it to demonstrate this RFC's
  syntax (`name string?`, `path path!`, `command string?!`, a bare-name reference) and wire it into the
  fixture-parse test (or delete it).
- **No instance-file churn.** Existing `- named:` and `- "scalar"` items keep working — now validated
  *and* deserialized consistently.

## 14. Alternatives considered

- **Unify name and shorthand into one marker.** The earlier draft of this RFC. Rejected: `resource`
  needs a name (`MyAppResource`) *and* a separate terse path (`/app/resource`); one marker can't fill
  two different fields. Splitting them is what makes `path path!` + optional `name` expressible.
- **Delete `NamedItemDeserializer`, mark `name string!` everywhere.** Rejected: name and `!` are
  different jobs; deleting the name injector forces every named model to declare `name` and risks
  silent breakage (`model step`). Keeping it costs nothing — the pass renders it a no-op where `name` is
  declared, and it remains the honest runtime fallback where it isn't.
- **Schema-aware `de`.** Rejected: fights serde's type-driven model and adds schema coupling; the pass
  achieves the same by normalizing upstream, leaving `de` simpler.
- **Header designation / prefix sigil / `* ^ #` / angle brackets.** Rejected: header bloat and `by`
  overload; `*` reads as pointer/glob; `^`/`#` undiscoverable or comment-like; `<…>` doesn't lex. `!`
  pairs with `?` and is free of meaning (NML is required-by-default, so `!` can't mean "required").

## 15. Out of scope

- **Go-to-definition precision (`nml-lsp`).** Clicking a *type* can resolve to a same-named field
  because `find_definition` is name+priority-based, not token-role-based. The fix resolves by the
  clicked token's CST role. Self-contained; tracked separately.
- **Optional future:** making a given model (e.g. `step`) **top-level-declarable** so it can be defined
  standalone and referenced — a consumer-schema choice; the name mechanism already covers identity for
  block declarations at the language level.

## 16. Revision 1 — Positional Marker Change

**The positional marker changes `!` → `+`.**

- **Status:** **Implemented (2026-07-04)** — workspace green (726 tests), clippy-clean; `nudge` re-validates
  its `+`-migrated schemas (28 embedded-schema tests pass). The five-line delta (§16.3), the 6-marker
  migration (§16.5), and the pinned collision test (§16.6, `plus_is_the_positional_marker_and_does_not_disturb_roles_or_strings`)
  all landed; `SyntaxKind::Bang` is deleted. **Scope:** the marker *character* only. Every semantic in §6–§14 — the positional
  shorthand, `?`-composition, the one-per-model constraint (§8), the materialization pass (§10), and all of
  Part A (name-injection) — holds **verbatim** with `+` substituted for `!`. This is a sigil swap, not a
  redesign.

### 16.1 Why change a shipped, working marker

The original choice (§14) reasoned that `!` is "free of meaning" because **NML is required-by-default, so
`!` cannot mean required.** That argument is *logically* sound — but only *after* a reader has internalized
NML's required-by-default rule. The marker's whole job is **discoverability**, which is a *first-read*
property, and on first read `!` misfires for a large audience:

- **GraphQL owns `!` as "required / non-null"** (`String!`), and GraphQL is the schema language most NML
  authors have seen. `name string!` reads to them as "name is a *required* string," not "name is the
  positional field." A sigil the audience misreads on sight fails at the one thing it exists to do,
  regardless of whether the misreading is *logically* precluded by NML's semantics.
- `+` carries no schema-world collision. Its only common association is regex "one-or-more," which is (a) a
  weaker association for a config/schema audience than GraphQL's `!`, and (b) **absent from NML itself** —
  NML spells cardinality with `[]` and optionality with `?`, and never uses `+` for quantity.
- **Type-suffix family.** `+` reads as a matched pair with `?` — `T?` (optional), `T+` (positional) — the
  same visual pairing `?`/`!` had, without the "required" baggage. `?`/`+` are a cleaner sibling set.

This does not retract §14's logic; it accepts that a marker must survive the *uninitiated* read, and `+`
does while `!` does not.

### 16.2 Feasibility — the lexer collision check (performed against the CST lexer)

`+` is viable with no grammar ambiguity, verified in `crates/nml-core/src/cst/`:

- **`+` is not a general value character.** It appears only in `is_role_continue` (`lexer.rs`) — a
  *continuation* class for `@`-prefixed **role** tokens (e.g. `@read+write`), which `scan_role` consumes as
  a unit, reached solely via the `@` dispatch arm. A role's internal `+` never touches the main dispatch.
- **A bare `+` is unclaimed.** Outside a role or a quoted string, `+` has **no** main-dispatch arm today; it
  falls to the error path. So a single new arm claims it cleanly: `string+` lexes as `Ident("string")
  Plus`; `@a+b` still lexes as one `Role`; `"a+b"` (a string literal) is untouched (strings scan separately).
- **`!` is used *only* as this marker** — a dedicated single-char token, 3 non-test consumers (lexer,
  parser, `ast`), no boolean-not, and NML has **no value-expression grammar**. Removing it is clean: a bare
  `!` reverts to the pre-RFC-0005 error token.
- **Future value arithmetic is not blocked.** If NML ever adds `+` arithmetic, the marker (a type suffix,
  inside a `model` declaration) and the operator (a value expression) occupy **grammatically distinct
  positions**; the parser interprets one `Plus` token by context — exactly as `*` is multiply-vs-deref in C
  or `-` is negate-vs-subtract. No lexer ambiguity, no forced marker change.

### 16.3 Code changes (delta only — the mechanism is untouched)

The token is inspected in **exactly one place** — the `shorthand()` accessor — and every consumer reads the
resulting `bool`, so the swap is a five-line change, not a sweep:

| Site | From | To |
| --- | --- | --- |
| `cst/syntax.rs` | `Bang,  // !` | `Plus,  // +` |
| `cst/lexer.rs` (dispatch + token-list test) | `b'!' => single(Bang)` | `b'+' => single(Plus)` — drop the `!` arm |
| `cst/parser.rs:384` field-type suffix | `eat(SyntaxKind::Bang)` | `eat(SyntaxKind::Plus)` |
| `cst/ast.rs:418` `shorthand()` accessor | `token(_, SyntaxKind::Bang)` | `token(_, SyntaxKind::Plus)` |
| `nml-fmt/formatter.rs:271` renderer | `out.push('!')` | `out.push('+')` (canonical optional-positional `?+`) |

**Deliberately *not* touched — this is the encapsulation win, not an omission:**

- **`cst/extract.rs` / `cst/lower.rs`** read the marker through `fd.shorthand()` (the accessor above), never
  the token, so they see only the `bool` and need **no** change. `ast.rs:418` is the single token-inspection
  point — the reason the delta is five lines.
- **`nml-lsp`** has **no** marker-specific code at all (it renders whatever token kinds the lexer emits), so
  it needs **no code edit** — only a **rebuild/reinstall** (`just install`) to run the new lexer, exactly as
  the original `!` rollout did.
- **`is_role_continue`** keeps `+` (roles legitimately use it — untouched by the new dispatch arm); the
  `shorthand: bool` flag, the §10 pass, and the §8 check (its message never names the char) are unchanged.

Rename `SyntaxKind::Bang` → `Plus` rather than adding a second token — one token, char-named like its
siblings (`Question`, `Dash`, `Pipe`), no dead variant, no `#[allow(dead_code)]`.

### 16.4 `?+` — optional positional

`?!` (§7) becomes `?+`. The parser accepts the two suffixes in **canonical order only** — `?` before the
marker (`parser.rs` eats `Question` then the marker), exactly as it did for `?!`, so `?+` is the one
accepted spelling and `+?` does not parse. What is "order-free" is the *AST*: `optional` and `shorthand` are
independent booleans, so there is no ordering to normalize, and `fmt` re-emits `?+` from the flags. The
swap changes only the character the parser eats second (`Bang` → `Plus`); the ordering rule is untouched.
The regex-quantifier evocation of a `?`-then-`+` suffix is niche and confined to the type position; it does
not mislead a config author the way GraphQL's `!` = required does.

### 16.5 Migration (hard cut — no dual support, per "remove legacy when the replacement is equal-or-better")

Mechanical rule: in **model** declarations, `T!` → `T+` and `T?!` → `T?+`. The complete set is **6 markers
across 3 files** (verified by grep; every other `!` in the tree is inside a **quoted string** — workflow
prompts like `"Done!"` — and is untouched, since strings scan separately):

- `nudge/src/schema/project.model.nml:9` — `name string!` → `name string+`
- `nudge/src/schema/workflow.model.nml:87` — `name string!` → `name string+`
- `nml/tests/fixtures/valid/models/core.model.nml` — four markers:
  `path path!` (:18), `address string!` (:22), `name string!` (:33) → `…+`; `run string?!` (:39) → `run string?+`

`SyntaxKind::Bang` is **deleted**; there is no `!`-marker compatibility shim. A stray `!` in marker position
becomes an ordinary unexpected-token error. *(Optional, non-required UX nicety for a smoother transition: a
one-line lexer diagnostic — "`!` is no longer the positional marker; use `+`" — retired once the tree is
migrated. Omitted by default to avoid transitional cruft.)* RFC 0005's title, §6, and §14 references to `!`
are superseded by this section; their prose holds verbatim with `+`.

### 16.6 Verification

- Re-run the full RFC-0005 suite with the sigil swapped: the lexer token-list test, parser `?`/`+`/`?+`,
  `cst::extract`, `fmt` round-trip, and the validator↔`de` agreement test — mechanism assertions are
  otherwise unchanged.
- **Add a pinned collision test** (the whole basis for choosing `+`): a bare `+` marker (`string+`) lexes as
  `Ident Plus`, while `@role+more` still lexes as a single `Role` and `"a+b"` stays one `String` — so the
  new marker cannot disturb role tokens or string literals.
- No `#[allow(dead_code)]`, no dual-marker code path: `Bang` is gone, not deprecated.
