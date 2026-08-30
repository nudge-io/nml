# RFC 0007 — Typed Arm Field Types: `(K -> V)`

- **Status:** **Implemented** (2026-07-07; draft v1 2026-07-06, v2 same-week after a six-finding self-review, built against v2 the same day). **Both layers green.** Layer 1 (the generic `BodyEntryKind::Arm` grammar) landed while implementing nudge RFC 0018's `|denial` routing. Layer 2 is built as specified: `FieldTypeExpr::Arms` + `FieldType::Arms`, `(K -> V)` required-paren parsing (arrow consumed only inside the paren branch, `=>` still gets the RFC 0006 guidance), `FieldTarget::Arms` with arms-aware union selection in `resolve_type_in_body` (body shape — arms / list items / neither — picks the variant), the §4.2 placement rule + §4.3 well-formedness in nml-validate (7 new pins incl. the §4.1 negative existence-check pin), nml-fmt arm-run arrow alignment (F4) + `(K -> V)` type rendering (round-trip + idempotence pinned), and LSP arm-target **completion** (`find_arm_target_types_at`, `V`-scoped, array-item aware) **and hover** (`find_declaration_hover`, extracted testable — named array items like `- ProUpsell:` hover as their item keyword with their **own leading-comment docs** (`ListItem::doc_comment`, with the preceding-sibling fallback for the first item, whose comment attaches to the enclosing `Body`); a top-level declaration outranks a same-named item in both the hover and the `doc_comment_for` lookup). Suites: nml workspace **741/0**; downstream nudge **3384/0** with the first consumer live — `denial (string | (role -> denial))?` on `mount` (config.model.nml) and `app` (project.model.nml), lowered to `DenialBinding` and stamped into class-aware `DenialRouting` per RFC 0018 §3.3 (R1b/R4b/R3d verified end-to-end from `app.nml` strict validation through class routing, incl. the §6 selector-in-`|allow` startup check). This RFC promotes both layers from "one denial-only production" (how nudge RFC 0018 §4.3/R5 framed it) to a documented, reusable language feature at the same bar as RFC 0004/0006.
- **Builds on:** [RFC 0002 — Shared Body-Aware Dispatch + `oneof` Defaults](./0002-visitor-unification-oneof-defaults-workflow-migration.md) (`oneof` is the named, discriminated cousin of these arms), [RFC 0003 — Schema-Driven Field Completion](./0003-schema-driven-field-completion.md) (the LSP surface a typed target plugs into), [RFC 0004 — Lossless CST](./0004-lossless-cst.md) (the CST the `Arm`/`Arms` nodes join), [RFC 0006 — The Arrow Token](./0006-thin-arrow.md) (the `->` these arms and this type both use).
- **Crates touched:** `nml-core` (`cst::syntax`, `cst::parser`, `cst::ast`, `cst::lower`, `ast`, `resolve`), `nml-fmt` (arm emission/alignment + `(K -> V)` type rendering), `nml-validate` (new `FieldType::Arms` + arm-placement rule + key/`else` well-formedness + inline-value validation), `nml-lsp` (completion/hover on arm targets). Downstream: `nudge` (`config.model.nml` denial typing; `DenialExperience` lowering) — the first consumer, nudge RFC 0018 §4.4.
- **Removes (legacy):** the framing in nudge RFC 0018 §4.3/R5 that the language delta is *"one production, `(Role | 'else') Arrow Ident`"* with *"zero grammar changes"*. Superseded: arms are a **general block body-entry**, and their schema home is a **typed field-type constructor**, not a `|denial`-only production. RFC 0018's R5 grammar-delta claim is now historical. Also removed from v1 of this RFC: the name `Map` (v2 finding F3 — see §2.1) and the claim that the validator authoritatively type-checks *reference* targets (v2 finding F1 — see §4.1).

---

## 1. Decision

Arms are a first-class NML construct in **two layers**.

**Layer 1 — grammar (landed).** A routing arm `(@selector | else) -> Target` is a generic block body-entry (`BodyEntryKind::Arm`), parseable in *any* block body. The grammar is permissive about *where* arms appear; the schema decides *which* blocks accept them and *what* their keys and targets must be. This is the same "generic grammar, schema restricts" split NML already uses for list items and modifiers.

**Layer 2 — type system (this RFC).** A field may be typed as a **typed arm set**, written `(K -> V)` — a new `FieldTypeExpr::Arms { key, target }`. Both sides are ordinary type expressions, so the validator checks each arm's key against `K`, drives LSP completion/hover on targets from `V`, and fully validates inline-block targets against `V` (reference targets stay consumer-resolved — §4.1). The first consumer, nudge RFC 0018's denial routing, is then just a field typing:

```nml
model mount:
    # a bare experience name (Forbidden-only shorthand, RFC 0018 R4b) OR a role→experience arm set
    denial (string | (role -> denial))?
```

```nml
mount:
    path = "/dashboard"
    denial:
        @plan/Pro -> ProUpsell        # key checked against `role`; target completed from `denial`
        else      -> Generic          # `else`: always legal, at most one, last
```

One arrow (RFC 0006), one arm concept, now with types on both sides.

### 2.1 Why "Arms", not "Map"

v1 named the constructor `Map`. Renamed (F3): a hashmap mental model is **false** here — these are **ordered, first-match arms with an `else` catch-all**, and matching is consumer-defined (role subsumption, class-aware eligibility, RFC 0018 §3.3), not equality lookup. `match`/`when` semantics, not `HashMap` semantics. And "arm" is already the language's word — `oneof` arms, `BodyEntryKind::Arm`, the house arm idiom — so `FieldTypeExpr::Arms` is vocabulary-DRY. The *surface syntax* keeps the readable `(K -> V)` arrow form; only the AST/validator vocabulary says arms.

## 2. Why a typed arm set, not an untyped "arms" primitive

The naive design types the field as an opaque `arms` primitive — "role-or-`else` → some ident, target unchecked." Rejected:

1. **No target typing.** An untyped target is an inert token: no completion, no hover, no inline validation, no documented intent. `V` makes the target a *typed* surface (§4.1 for exactly what is and isn't checked).
2. **Not general.** An opaque primitive cannot express `string -> handlerModel`, `string -> (httpRoute | wsRoute)`, or any routing beyond denial. `FieldTypeExpr::Arms` reuses the *entire* existing type grammar on both sides, so union values, refs, and inline blocks compose for free.
3. **DRY with `oneof`.** `oneof X by f: "v" -> Model` is the *named, discriminated-by-a-field* form of the same `key -> target` arm; `(K -> V)` is the *anonymous, inline* form. One arm concept, two surfaces, one token (RFC 0006) — the typed arm set is the missing anonymous half, not a parallel invention.

## 3. Grammar and the required-parentheses rule

An arm-set type **must be parenthesized**: `(K -> V)`, never bare `K -> V`. This is not a new constraint bolted on — it is what the existing grammar already implies, and it resolves two ambiguities a bare form would introduce.

1. **`?` binds to the field, never to a type.** The optional marker is a *field-def suffix*, consumed **after** `type_expr()` returns (`cst/parser.rs`, the field-def arm: `type_expr()` then `eat(Question)`). There is no "optional inner type." So `(role -> denial)?` can only mean *optional field whose type is the arm set* — the reading one might fear, "arms to an optional value," is simply not expressible.
2. **`->` (like `|`) is consumed only inside the paren branch.** `type_expr()` eats an arrow (in Layer 2) or a pipe **only** after an `LParen`. A bare `role -> denial` at field-type position therefore parses `role`, returns, and the field-def logic then meets `->` where it expects `?`/`+`/`=` — a **parse error**, not an ambiguous parse. The un-parenthesized form isn't ambiguous; it's invalid.

Consequences, all free:

- **Consistency with unions.** `(a | b)` already requires parens for exactly the same reason. The whole rule is one line: *a scalar type is bare; any compound type expression (union or arm set) is parenthesized; suffixes (`?`, `+`, `= default`) live outside the type and bind to the field.*
- **`|` vs `->` precedence never arises.** Because everything compound is parenthesized, `(string | (role -> denial))` and `(role -> (a | b))` are always explicit. There is no precedence to define or misread.
- **Optional parens would be the *expensive* choice.** Making them optional means teaching the bare branch of `type_expr()` to consume `->`, which is precisely what *reintroduces* both ambiguities. Required parens is simultaneously the cleaner and the lower-effort answer.

## 4. The type: key and target domains

`FieldTypeExpr::Arms { key: Box<FieldTypeExpr>, target: Box<FieldTypeExpr> }`, rendered `(K -> V)`.

- **Key domain `K`** is a type expression naming the selector kind: `role` (selector tokens like `@plan/Pro`), `string` (literal keys, `oneof`-style), or an `enum` type. The **`else` catch-all is always a legal key** regardless of `K`, and is type-exempt — it is the universal fall-through, not a value of `K`.
- **Target domain `V`** is any type expression: a model reference (or inline block), or a union of models `(a | b)`.
- **Ordering is significant; matching is the consumer's.** Arms are first-match. NML validates *shape*; the *routing* semantics (which selector a denied principal matches) belong to the consumer — for denial, nudge RFC 0018 §3.3's class-aware algorithm.

### 4.1 What `V` checks — and what it deliberately does not (F1)

v1 overclaimed that "the validator rejects any target that does not resolve to a `denial`." That is **impossible to do soundly** for reference targets, and the correction is a feature, not a retreat:

- **Reference targets** (`@plan/Pro -> ProUpsell`, an identifier naming a declaration elsewhere) get **no existence/type checking from nml-validate**. Two structural reasons: (a) consumer resolution is **cross-scope** — nudge resolves an app-mount arm's target against the app's registry *with deployment-parent fallback*, which a single-file validator cannot see; an in-file check would fire **false errors on legitimate cross-file references**. (b) It matches nml's universal convention: every named reference (`wasm`, `workflow`, `web`) is consumer-resolved; nml never checks existence. This is also why Layer 1 put `BodyEntryKind::Arm` in `resolve.rs`'s literal pass-through group. For reference targets, `V` is load-bearing as: **LSP completion candidates** (names of declared `V`-typed items in scope), **hover documentation**, and **declared intent** the consumer's dual-enforcement layer implements authoritatively.
- **Inline-block targets** (`"k" -> ` followed by an indented body — an additive-future form, §6) **are fully validated** against `V`'s model via the existing `validate_ref_instance` machinery, exactly like any typed nested block. Inline values are in-file by construction, so the check is sound.

Same dual-enforcement shape as everything else in nml: **schema = shape + editor intelligence; consumer = load-time truth.**

### 4.2 Arm placement is schema-gated (F2)

Layer 1's permissive grammar needs its schema half: **an `Arm` body-entry is legal only inside a block whose schema-declared field type is an arm set** (`Arms`, possibly inside a union). An arm anywhere else — e.g. `@x -> y` inside a `pipeline:` block — is a **validator error**, not silently ignored. Without this rule, a misplaced arm parses, validates, and does nothing: a latent trap. This completes the "generic grammar, schema restricts" promise; the grammar stays permissive (good recovery, one parser), the schema closes the gap.

### 4.3 Well-formedness rules (F6)

- **`else` cardinality and position:** at most **one** `else` arm per block, and it must be **last** (first-match ordering makes a non-last `else` dead code; dead arms are authored bugs). Validator error otherwise.
- **Duplicate keys:** exact-duplicate keys in one block are a validator error. **Semantic overlap** (`@authenticated` vs `@plan/Pro` both matching one principal) is the consumer's domain — nml cannot know subsumption; RFC 0018's eligibility algorithm owns it.
- **Exhaustiveness:** for `(enum -> V)` with no `else`, full variant coverage *could* be required (as `oneof … by … as enum` does today). **Deferred** — an `else`-less enum-keyed arm set has no consumer yet; the clause is stated so the deferral is a decision, not an omission.
- **Type-shape rules (schema-definition time):** the type grammar deliberately parses compositions that have **no instance form**; the schema layer rejects them rather than accepting a field whose instances would silently go unvalidated (the fresh-eyes probe found all three passing clean). ① `(K -> V)` under `[]` — directly or through a union, at any depth — is an error: arms are body entries, not list items, so an array of arm sets can never be written. ② `(K -> V)` inside another arm set's key or target is an error: an arm's target is a bare reference identifier, so nesting can never be written. ③ A union with **more than one** arm-set variant is an error: the variant is selected by body *shape*, and an arms-shaped body always selects the first — the second would be silently unreachable, order-dependent semantics by accident. ④ `(K -> V)` anywhere in a **modifier's declared type** (`|gate (role -> denial)?`, directly or through a union) is an error: a modifier's instance value is an inline value or a list block, so an arm body can never be written under one. ⑤ **Shorthand on an arm-set type is IMPLEMENTED** (`f (K -> V)+`, bare or union-wrapped). A shorthand field is filled by a bare *scalar* list item (RFC 0005), and its scalar fill is the **canonical embedding `s ⇒ [else -> s]`** — a one-arm block whose `else` target mirrors the scalar's form (a bare name → an `else -> Name` reference, a quoted scalar → an `else -> "s"` literal), synthesized by `nml-core`'s identity/positional pass (`inject_arm`) so consumers read a uniform arm block whether the author wrote the sugar or the block. This was reserved for one round (the "why the wall?" dialogue): the types were never the obstacle — `s : V ⇒ [else -> s]` is well-typed — but an arm's **eligibility** (which outcome classes consult it) is *consumer* semantics the types don't carry; the natural "scalar = `else` arm" reading is exactly what nudge RFC 0018 R4b rejected for a *class-stratified* router. The resolution is that eligibility is the consumer's, not nml's: nml materializes the structural `else` arm; a flat router (workflow dispatch, redirect routing) reads `else` as "everyone", while a class-stratified consumer that wants Forbidden-only semantics uses the union form `(string | (K -> V))` and binds the scalar branch itself (as denial does). Both `(K -> V)+` and `(string | (K -> V))+` are accepted; the fill is lenient (an explicit arm block in the item body wins). Verified by `identity::scalar_fills_arm_set_shorthand_as_an_else_arm`.

### 4.4 The scalar union member is not sugar (F5)

nudge's `denial (string | (role -> denial))?` keeps `string` as a **distinct** member, *not* a shorthand that desugars to `else -> X`. RFC 0018 **R4b**: the scalar binds **`Forbidden` only** (anonymous visitors keep the login default with the experience as destination context), while an `else` arm is **anon-eligible** — collapsing them would paywall strangers, violating 0018's founding principle. Both the scalar and a reference target are consumer-resolved denial names (consistent under §4.1); the scalar's typing as `string` follows the same named-reference convention as `wasm`/`workflow`.

## 5. Changes

| Site | Change | Status |
|---|---|---|
| `cst/syntax.rs` | `Arm` node (added before `Error` to preserve contiguous discriminants) | **done** |
| `cst/parser.rs` `entry()` | dispatch `Role`/`else` to `arm()` **only** when `nth(1)` is `Arrow`\|`FatArrow` (keeps `else` a property name; lets stray `@…` recover; keeps `=>` guidance) | **done** |
| `cst/parser.rs` `arm()` | `selector -> Ident`, completing the `Arm` even on a missing target (recovery) | **done** |
| `cst/parser.rs` `type_expr()` | inside the paren branch, if `->` follows the first type, parse the target type → `Arms` (else the existing `\|` union loop) | **new** |
| `cst/ast.rs` | `Arm` accessors (`selector()` before-arrow, `target()` after-arrow); `TypeExpr` arms accessors | Arm **done**; Arms **new** |
| `cst/lower.rs` | `ast::Entry::Arm` → `BodyEntryKind::Arm`; arms `TypeExpr` → `FieldTypeExpr::Arms` | Arm **done**; Arms **new** |
| `ast.rs` | `Arm`/`ArmSelector{Role,Else}` + `BodyEntryKind::Arm`; `FieldTypeExpr::Arms{key,target}` + `Display` as `(K -> V)` | Arm **done**; Arms **new** |
| `resolve.rs` | `BodyEntryKind::Arm` in the literal pass-through group (reference targets are consumer-resolved — §4.1) | **done** |
| `nml-fmt` | render `Arms` type as `(K -> V)`; **align a homogeneous run of consecutive arm entries on the arrow** (F4 — matching `oneof` arm alignment; v1's single-space emit assumed heterogeneous blocks, but an arm-typed field block is purely arms) | arm emit **done, alignment new**; type render **new** |
| `nml-validate` | `FieldType::Arms`; **placement rule** (§4.2); key conforms to `K`; `else` cardinality/position (§4.3); exact-duplicate keys error; **inline-block targets** validated against `V`; **reference targets not existence-checked** (§4.1) | **new** |
| `nml-lsp` | completion + hover on an arm target scoped to `V` | **new** |
| downstream `nudge` | `denial (string \| (role -> denial))?` on `mount`/`[]app` (the target type is the existing `oneof denial` — variants `denialCard`/`denialPage`/`denialRedirect`); scalar = Forbidden-only (§4.4, NOT an `else` desugar); union → `Vec<Arm>` lowering; class-aware routing per RFC 0018 §3.3 | **Part 2** |

## 6. Scope boundaries (accommodated by the type, not built here)

The type is fully general; the grammar currently implements the subset denial needs. Two capabilities are **additive-future** — the type already describes them, and adding them later touches no file this RFC changes incompatibly:

1. **String-keyed inline arms — IMPLEMENTED** (`"k" -> v` in a plain block). `entry()` dispatches a `String` token followed by `->` to `arm()`; `ArmSelector::Literal(String)` carries the decoded key. String keys parse in `oneof`'s top-level discriminated form; a `(string -> V)` field brings them inline.
2. **Inline-object arm targets — IMPLEMENTED** (`@role/admin -> adminLanding:` + indented body, RFC 0007 §6.2). The arm RHS accepts a reference (`-> Name`), a string literal (`-> "path"`), or a named inline instance (`-> Name:` + body). Inline bodies are fully validated against `V`'s model via `validate_inline_body` / `materialize_arm_inline`, exactly like list-item inline instances. String/path literal targets remain for flat routers (item 3 below).
3. **String/path arm targets — IMPLEMENTED** (`@plan/Pro -> "workflows/pro.workflow.nml"`, `else -> "https://example.com/promo"`). Motivated by the natural consumers after denial — per-role **workflow dispatch** and **redirect routing** — where the target is a path/URL literal, not a declared name. `Arm.target` is a two-form `ArmTarget { Reference(Identifier), Literal { value, span } }`: the parser accepts an `Ident` (reference) or a `String` (literal) after the arrow; the formatter renders a literal quoted (like a `oneof` string key); and the validator (`validate_arm_target`) checks form against `V` — a **literal** requires a scalar-capable `V` (primitive/enum/union-with-one; a literal where a model/`oneof` instance is expected is a category error), a **reference** is shape-legal for any `V` and stays consumer-resolved (§4.1, never existence-checked). Verified end to end: `cst::lower` (both forms), `nml-fmt::test_format_arm_literal_targets_roundtrip`, `nml-validate::arm_literal_targets_require_a_scalar_target_type`. Denial rejects a literal loudly (its targets are `[]denial` names). **[SEC] consumer obligation:** a literal target is an untrusted config string — any consumer that touches disk or network with it (the workflow router's paths, a redirect's URLs) must run it through the same load-time gates as every other such config value (path: `is_safe_relative_path`/`open_within`-anchored reads; URL: `validate_link_url`-class checks). nml validates *type*, never *safety* — exactly the dual-enforcement split every other field follows.

### 6.1 Forward note: typed references (out of scope, worth an RFC)

F1 and F5 both expose the same missing primitive: nml has **no typed reference** — every named reference (`wasm`, `workflow`, `web`, `denial`-scalar, arm targets) is a bare `string`/`Ident` resolved by the consumer. A first-class *typed-reference* concept ("a name that must resolve, in the consumer's scope, to a declared X") would give completion, hover, and declared intent to every reference site uniformly, and would make the scalar member and arm targets fully symmetric. That is a broad, own-RFC-sized change touching every reference in the language; this RFC's `V`-for-completion design (§4.1) is deliberately shaped to be its first citizen rather than a competing mechanism.

## 7. Non-changes

- **`oneof` is untouched — and deliberately so.** The kinship is *syntactic* (both use `key -> target` arms, RFC 0006's one arrow), but the semantics differ at type level: `oneof` is a **sum type** — a tagged union whose discriminator is a *field of the instance*, selecting which model the flat instance body must satisfy. An arm set is a **routing table** — an ordered ruleset over *external* keys (roles, principals) mapping to targets; the instance *is* the table. Unifying the syntax (done) is correct; unifying the semantics would conflate a type former with a value form.
- **The CST stays lossless.** `Arm` and the arms `TypeExpr` are ordinary fixed-shape nodes; round-trip guarantees are unchanged.
- **`else` stays a contextual keyword** (`at_kw`), usable as an ordinary identifier everywhere outside arm-selector position.
- **Arms are serde-invisible — hand-parsed by design.** Like `|allow`/`|deny`/`|grant` modifiers and field definitions, an `Arm` has no generic serde target (there is no canonical deserialize for an *ordered routing table*), so `collect_body_map_entries` drops arm entries and the consumer reads `BodyEntryKind::Arm` directly with its field marked `#[serde(skip)]` — nudge's `DenialBinding::parse_from_body` is the reference. A shorthand-filled arm block (§4.3 ⑤) deserializes identically to an authored one: the enclosing block is serde-visible, the arms inside are hand-read past serde. A future workflow-dispatch consumer follows this same pattern rather than expecting serde to populate an arms field.
- **No dual-accept, no migration.** Arms and arm-set types are new syntax; there is nothing legacy to migrate, and `nml-fmt` only ever emits the canonical form.

## 8. Tests

- **Parser:** `(role -> denial)` parses as `Arms`; `(string | (role -> X))` as a union of a scalar and an arm set; `(role -> (a | b))` as arms to a union; **bare** `role -> X` at field-type position is a parse error (parens required); `?` after `)` attaches to the field, not the target.
- **Grammar (landed pins):** arms parse in a plain block; `else` outside arm position stays an identifier; `=>` in arm position yields RFC 0006's guidance error; a missing target still completes the `Arm` (recovery).
- **Formatter:** the `Arms` type renders `(K -> V)`; arm bodies round-trip and are idempotent; **a homogeneous arm run aligns arrows on the widest selector** (F4), matching the `oneof` alignment pin.
- **Validate:** an arm in a non-arm-typed block errors (§4.2 placement); a key that does not conform to `K` errors; `else` is always accepted but errors when duplicated or non-last (§4.3); exact-duplicate keys error; an inline-block target failing `V` errors; a **reference** target is *not* existence-checked (negative pin — cross-scope refs must not false-positive).
- **LSP:** completion at an arm target offers `V`-typed names; hover shows `V`.
- **Downstream:** nudge's `denial` field validates both the scalar and the block forms; the scalar form stays Forbidden-only (R4b pin, in nudge's suite); the nudge suite is green over the path dependency (no version dance).
