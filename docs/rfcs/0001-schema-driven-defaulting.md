# RFC 0001 — Schema-Driven Defaulting

- **Status:** Implemented (the schema-defaulting pipeline `apply_defaults → resolve →
  from_block` / `from_body_defaulted`, `SchemaIndex`, and the `de.rs` resolve hook; RFC 0002
  builds on it and RFC 0003 consumes its `SchemaIndex`)
- **Crates touched:** `nml-core`, `nml-validate`, downstream consumers (`nudge`). (`nml-fmt` needs **no** change — its formatter already renders defaults from the AST's typed value; see §7.)
- **Supersedes:** the "Schema-driven defaulting (HIGH PRIORITY)" item in `nudge/TODO.md`
- **Related, intentionally separate:** `oneof` discriminator default (unblocked by this RFC; see §11)

## 1. Summary

Make the NML **schema** the single source of truth for field defaults. Today a
field default is declared twice — once in the NML model (`messageStream string =
"outbound"`), read only by the *validator*, and once as a Rust
`#[serde(default = …)]`, which is what actually runs. The schema default is never
injected at deserialization, so it is documentation only.

This RFC introduces a **schema-guided defaulting pass**: a pure AST→AST transform
that injects missing field defaults into a `Body` before serde sees it. The pass
is built on a new shared **`SchemaIndex` + `resolve_field` dispatch** primitive that
the validator also uses, so field→model dispatch has exactly one definition (with a
full schema-guided visitor unification gated as Phase 5's first step). `nml-core::de`
stays schema-agnostic (no schema types leak in); its only change is a small
`From<ResolveError>` error-conversion impl, not schema coupling.

## 2. Motivation

1. **Spec conformance.** [`spec/models.md`](../../spec/models.md) §"Field Presence
   Rules" states: *"`= value` — field has a default. Instances may omit it; the
   default is used."* The current implementation never uses the default at
   deserialize time, so it **violates its own specification**. This RFC brings the
   implementation into conformance.
2. **Eliminate the dual source of truth.** The NML default and the serde default
   can drift silently — the schema can claim one value while serde uses another —
   and every defaulted field must be written twice.
3. **Unlock dependent features.** A clean defaulting pass is the prerequisite for
   `oneof` discriminator defaults (inject the default tag before serde sees it)
   and shares its dispatch primitive with the deferred `oneof` LSP completion work.

## 3. Current state (as of this RFC)

Grounded in the code so the design is concrete:

- **The default is typed in the AST but flattened on extraction.**
  [`ast::FieldDefinition.default_value`](../../crates/nml-core/src/ast.rs) is
  `Option<SpannedValue>` (typed, with span). But
  [`model_extract::convert_field_def`](../../crates/nml-core/src/model_extract.rs)
  flattens it to `Option<String>` via `format_default`, discarding type and span.
  [`model::FieldDef.default_value`](../../crates/nml-core/src/model.rs) is therefore
  `Option<String>`.
- **The validator only checks default *presence*.**
  [`schema.rs`](../../crates/nml-validate/src/schema.rs) uses
  `field.default_value.is_none()` to decide whether a missing field is an error. It
  never reads the value.
- **`de` is schema-agnostic by design.**
  [`de::from_block`](../../crates/nml-core/src/de.rs) is handed a Rust target type,
  not a model name. [`de::from_body_resolved`](../../crates/nml-core/src/de.rs) runs
  `resolve_body → apply_shared_properties → from_block`.
- **Lookups are linear.**
  [`SchemaValidator::find_model`/`find_oneof`](../../crates/nml-validate/src/schema.rs)
  scan a `Vec` (`O(n)`).
- **Dispatch is entangled with diagnostics.** Block→model and nested-block→model
  resolution lives inside `validate_instance_against_model` (~150 lines)
  interleaved with diagnostic emission.
- **The schema is already in memory at runtime in consumers.** `nudge` embeds its
  `.nml` model files and builds strict `SchemaValidator`s
  (`nudge/src/embedded_schemas.rs`), validating each file once, then deserializing
  per block with `from_body_resolved`. The block keyword (→ root model) is in scope
  at every deserialize site.

## 4. Goals / Non-goals

**Goals**

- Schema is the single source of truth for default **values**.
- Spec-conformant: declared defaults are applied when a field is omitted.
- `nml-core::de` remains schema-agnostic (no schema types); its only edit is a
  `From<ResolveError>` impl for error ergonomics.
- One definition of schema dispatch (`resolve_field`), shared by validation and
  defaulting.
- Declared defaults are type-checked at schema load (constraint-checking deferred
  until constraints are modeled at all — §10).
- Bounded and terminating on recursive/untrusted schemas.
- Ergonomic call sites in consumers.

**Non-goals**

- `oneof` discriminator *default* injection (separate, unblocked — §11).
- Removing `#[serde(default)]` from `Option<T>` optionals (intentionally kept — §9).
- Compile-time codegen of serde defaults from schema (rejected — §12).

## 5. Pipeline ordering — resolution runs **last**

The canonical pipeline becomes:

```
apply_shared_properties  →  apply_defaults  →  resolve_body  →  from_block
        (nml-core)              (nml-core)        (nml-core)      (nml-core)
```
All four stages live in `nml-core`; `from_body_defaulted` (§9) composes them. The
only `nml-validate` involvement is exposing the shared `SchemaIndex` via `index()`.

**This ordering is required for correctness, not merely preferred.** A declared
default is an arbitrary `SpannedValue`, so it may itself be a `$ENV.X` secret or a
fallback chain (e.g. `apiKey secret = $ENV.DEFAULT_KEY`). If resolution ran *before*
injection, an injected secret-valued default would escape resolution and reach serde
as a raw `$ENV.X` string — a bug. Injecting first, resolving last, ensures injected
defaults are resolved on equal footing with author-written values. Default injection
and shared-merge are *structural* (presence-based); resolution is *value-based*;
running structure first and values last is the only order that resolves everything
exactly once.

Consequences:

- **Least secret exposure.** `apply_shared_properties` and `apply_defaults` only
  ever see `$ENV.X` *references*, never resolved secret material. Secrets are
  materialized only in the final `resolve_body` step immediately before serde.
  This honors the [`resolve.rs`](../../crates/nml-core/src/resolve.rs) contract
  ("callers must not log or serialize resolved bodies") by keeping the
  schema-aware crate out of the secret-handling path entirely.
- **Cleaner separation of concerns:** structure is settled before values are.

**Precedence (highest wins):** explicit author value → shared property (`.key`) →
model default. Achieved by ordering shared-merge before defaulting: defaulting only
fills names still absent after the merge.

### 5.1 This is a behavior change, not a no-op

Moving resolution from first (`from_body_resolved`'s current `resolve → shared`) to
last changes one observable semantic beyond injected defaults: **resolution becomes
lazy with respect to overridden/absent values.** Today an unresolvable secret
*anywhere* in the body — including in a `.key` shared property that an item later
overrides — fails the entire body, because resolution runs before the merge drops
the overridden property. Under resolve-last, an overridden or never-injected
shared/default value is removed before `resolve_body` runs, so it is never resolved
and cannot fail. This is strictly more correct (don't resolve what isn't used), but
it is a genuine behavior difference and gets its own dedicated test
(`overridden_shared_secret_is_not_resolved`), alongside re-pinning the existing
shared+secret tests (e.g. `resolve_shared_scalar_in_shared_property`).

To avoid a mixed-order window during migration (§15), the reorder is implemented
**only in the new `from_block_defaulted`/`from_body_defaulted` orchestration**;
`from_body_resolved` keeps its current order until it is removed. No single config is
ever processed by two different orders.

## 6. Shared primitives — `SchemaIndex` + `resolve_field` dispatch

Both validation and defaulting are schema-guided AST walks. They share these
primitives instead of duplicating dispatch. The mandated shared surface for *this*
RFC is the `SchemaIndex` (§6.1) and the `resolve_field` dispatch function (§6.2); the
full schema-guided visitor is gated to Phase 5 (§6.2).

### 6.1 `SchemaIndex` (nml-core)

An index over models/enums/oneofs, built once, giving `O(1)` lookup while
**preserving definition order and first-definition-wins semantics** (today's
behavior — duplicates are reported but the first definition is authoritative). It
therefore keeps an ordered `Vec` for iteration plus a name→position map for lookup,
rather than a bare `HashMap` (which would lose order and silently change which
duplicate wins and the order of order-sensitive diagnostics such as cycle and
duplicate reporting):

```rust
// nml-core
pub struct SchemaIndex {
    models: Vec<ModelDef>,
    model_pos: HashMap<String, usize>, // first occurrence wins; not overwritten on dup
    enums: Vec<EnumDef>,
    enum_pos: HashMap<String, usize>,
    oneofs: Vec<OneOfDef>,
    oneof_pos: HashMap<String, usize>,
}

impl SchemaIndex {
    pub fn build(models: Vec<ModelDef>, enums: Vec<EnumDef>, oneofs: Vec<OneOfDef>) -> Self;
    pub fn model(&self, name: &str) -> Option<&ModelDef>;     // O(1)
    pub fn oneof(&self, name: &str) -> Option<&OneOfDef>;     // O(1)
    pub fn enum_def(&self, name: &str) -> Option<&EnumDef>;   // O(1)
    pub fn models(&self) -> &[ModelDef];                      // ordered iteration
}
```

It lives in `nml-core` because it is pure schema data + lookup with no validation
policy; `ModelDef` already lives there. `SchemaValidator` owns a `SchemaIndex`
internally; its `O(n)` `find_model`/`find_oneof` scans are replaced by it.

### 6.2 Shared dispatch — mandated; full visitor — gated to Phase 5

The load-bearing thing that must not be duplicated is **field→target resolution**:
given a `FieldDef` (and the index), does this field resolve to a nested model, a
`oneof`, a free-form object, a union, or a leaf primitive? This is extracted as one
pure function and is the **mandated** shared primitive:

```rust
// nml-core
pub enum FieldTarget<'a> {
    Model(&'a ModelDef),
    OneOf(&'a OneOfDef),
    ListOf(Box<FieldTarget<'a>>),
    Object,            // free-form PrimitiveType::Object — no schema to recurse into
    Union,             // ambiguous without a discriminator — not recursed in v1
    Leaf,              // primitive scalar
}

impl SchemaIndex {
    /// The single name→target decision (model / oneof / leaf), shared by the
    /// validator and the defaulter.
    pub fn resolve_ref(&self, name: &str) -> FieldTarget<'_>;
    // FieldTarget borrows only from the index, never from the field, so the
    // field argument is not tied to the index's lifetime.
    pub fn resolve_field<'a>(&'a self, field: &FieldDef) -> FieldTarget<'a>;
}
```

The `name → target` decision lives in exactly one place, `resolve_ref`, and is
called by **both** crates: the defaulter via `resolve_field` (which composes
`resolve_ref` for `ModelRef` fields), and the validator directly for its nested-block
and list-item model-ref dispatch (`validate_ref_instance`). Neither re-derives the
model-or-oneof decision; each keeps its own walk (the validator's diagnostic-emitting,
the defaulter's injecting). What remains validator-specific is the **body-dependent**
union/list dispatch — it inspects the instance body (`has_list_items`) to pick a
variant, which a field-only `resolve_*` cannot express. That last piece is unified
only by the Phase 5 visitor below.

A fuller unification — a single schema-guided **visitor** that owns the walk (and
the depth guard) and yields `(child_body, target)` events, with validation and
defaulting as two visitor implementations — is the cleaner end-state. It is
deliberately *not* a prerequisite of *this* feature, but it is a **hard prerequisite
of Phase 5** (`oneof` discriminator default + union defaulting), and the sequencing
is load-bearing:

- **Why not up front.** Rewriting the validator's ~2,900-line traversal before the
  defaulting feature (and its tests) exist would front-load the riskiest change onto
  the most load-bearing code, with only the validator suite as a guard, and force the
  unifying abstraction to be designed against a single known consumer (validation, a
  read-only fold) plus a speculative second (defaulting, a tree transform). Mandating
  the shared `resolve_field` now captures the DRY-critical non-duplication
  immediately; the *walk* that remains duplicated in v1 is the simple subset
  (scalars, nested blocks, plain lists, present-discriminator oneofs).
- **Why it cannot be skipped.** The expensive-to-duplicate part of the walk is the
  union + list dispatch (`schema.rs` ~L381–427). v1 defaulting deliberately skips
  union-typed fields (§8), so that complexity is never duplicated *until* Phase 5
  introduces union/discriminator defaulting — which forces the complex shared walk
  anyway. Phase 5 is therefore the natural, non-skippable moment to unify: both
  consumers and both test suites exist, the read-fold vs. transform-map shapes are
  both known, and the refactor is guarded on both sides. **Phase 5 MUST NOT proceed
  on a duplicated walk;** the visitor unification is its first step.

This makes the unification a scheduled, gated obligation rather than an orphanable
"recommended" follow-on, so the interim duplication stays bounded to the cheap subset
and is retired exactly when it would otherwise become expensive.

## 7. Typed defaults

`FieldDef.default_value` changes from `Option<String>` to `Option<SpannedValue>`,
preserving the AST's typed value and span. Consequences:

- `model_extract::convert_field_def` carries the typed value; **`format_default` is
  deleted** (now dead — no `#[allow(dead_code)]`).
- `schema.rs`'s `is_none()` presence check is unchanged (`Option::is_none` is
  type-agnostic).
- **`nml-fmt` is unaffected.** Its formatter operates on the *AST*
  (`ast::FieldDefinition.default_value`, already an `Option<SpannedValue>`), not on
  `model::FieldDef`, and already renders the default via `format_value`. The only
  lossy `String` form was `model::FieldDef.default_value`, consumed solely by the
  validator's presence check and one extraction test — neither of which the
  formatter touches. (An earlier draft assumed the formatter needed updating; it
  does not.)

## 8. Defaulting algorithm

`apply_defaults(index, root, body) -> Body` — pure, owned output (consistent with
`resolve_body` which already clones). `root` resolves through the `SchemaIndex` to
**either a model or a `oneof`** — a top-level block can be a discriminated union
(e.g. nudge's `email` and `identityProvider` are top-level `oneof`s, deserialized as
blocks). A `oneof` root is handled exactly like a present-discriminator `oneof` field
below (resolve the discriminator, default the selected variant). If `root` resolves
to nothing, defaulting is a no-op (§9). Per-node behavior:

- **Scalar field with a default, absent** → inject `Property { name, value:
  default.clone() }`.
- **Present nested block (`ModelRef`)** → recurse with the referenced model.
- **Absent *required* nested block whose model is *fully defaultable*** (every
  required field is itself defaulted, transitively) → **materialize** it from
  defaults. Otherwise leave it absent (serde/validation reports the missing required
  fields). An **optional** nested model is never materialized — serde reads its
  absence as `None`, and synthesizing a default struct would wrongly turn `None`
  into `Some(default)`. Mirrors serde: a required `T` with a struct default is
  synthesized; an `Option<T>` is left `None`.
- **List field (`[]model`) and top-level array items (`ArrayBody`)** → recurse into
  each *present* `Named` item with the item model; never synthesize items (absent
  list ⇒ empty, handled by serde).
- **`oneof` field with a *present* discriminator** → resolve the variant and inject
  that variant's field defaults (mirrors `validate_instance_against_oneof`). An
  *absent* discriminator is out of scope here (§11).
- **Free-form `object`-typed field** (`FieldTarget::Object`) → pass through
  untouched. There is no model to default against, matching the validator, which
  `{}`-skips `PrimitiveType::Object`.
- **Union-typed field** (`FieldTarget::Union`) → pass through untouched in v1.
  Without a resolved discriminator the variant is ambiguous, and guessing one to
  inject its defaults would be wrong. Defaulting into unions is revisited only
  alongside the `oneof` discriminator-default work (§11).

### 8.1 Termination & bounds

Three independent guards bound the pass on hostile/recursive schemas (a depth
guard alone is **not** sufficient — it caps recursion depth but not width):

1. **Defaultability is precomputed once** as a least fixpoint over all models
   (`O(models² · fields)`), so the materialization decision is an `O(1)` set
   lookup. A naive per-field recursion would be exponential on a diamond-shaped
   schema. A required reference *cycle* never enters the set (it can never be
   satisfied without an authored value), so cycles are handled by construction.
2. **A materialization budget** (`MAX_MATERIALIZED_MODELS`) caps the *total*
   number of nested models synthesized from defaults in one pass. Without it, a
   diamond of required defaultable refs would emit exponentially many blocks
   within the depth limit; with it, a hostile schema degrades to a
   missing-required-field validation error rather than memory exhaustion.
3. **A depth guard** (`MAX_DEFAULT_DEPTH`, mirroring `MAX_VALIDATION_DEPTH`)
   bounds recursion depth into authored and materialized structure as
   defense-in-depth.

Together these are the DoS bound for untrusted schemas loaded by other `nml-core`
/ `nml-validate` consumers.

## 9. API surface & consumer ergonomics

**Deserialization stays entirely in `nml-core`.** The orchestration is a pair of
free functions in `nml-core`'s `defaults` module that take the shared `&SchemaIndex`,
so `nml-core` owns the whole deserialize path (`de` + defaulting) and `nml-validate`
stays validation-only — it grows no serde dependency and no deserialization surface,
only an `index()` accessor exposing the primitive it already owns:

```rust
// nml-core::defaults — the new orchestration
pub fn from_body_defaulted<T: for<'de> Deserialize<'de>>(
    index: &SchemaIndex, root: &str, body: &Body, resolver: &ValueResolver,
) -> Result<T, de::Error>;

/// Keyed by a block's keyword (its root model/oneof) so keyword and body can't be
/// mismatched.
pub fn from_block_defaulted<T: for<'de> Deserialize<'de>>(
    index: &SchemaIndex, block: &BlockDecl, resolver: &ValueResolver,
) -> Result<T, de::Error>;

// nml-validate
impl SchemaValidator { pub fn index(&self) -> &SchemaIndex; }
```

Consumer migration (nudge): each `from_body_resolved(&block.body, &resolver)` becomes
`nml_core::from_block_defaulted(VALIDATOR.index(), block, &resolver)` (or
`from_body_defaulted(index, "model", body, &resolver)` for nested blocks / array
items with a known model). No call site hand-rolls the pipeline.

This was chosen over a `SchemaValidator::deserialize_block` *method* (an earlier
sketch): putting deserialization on the validator would pull serde and a
deserialize responsibility into the validation crate. Free functions in `nml-core`
keep each crate single-purpose — `nml-core` = parse + deserialize + defaulting,
`nml-validate` = validation — both sharing one `SchemaIndex`. The `&SchemaIndex`
argument is marginally less terse than a method but keeps the dependency graph clean.

**Root resolution** uses `SchemaIndex::resolve_ref` to map the keyword/name to a
model *or* a `oneof` (top-level unions like `email`/`identityProvider`), dispatching
defaulting accordingly (§8). **No-schema is graceful, not an error:** if the name
resolves to neither, defaulting is a no-op and deserialization proceeds (shared +
`from_block`). This preserves deserialization for schema-less or not-yet-modeled
blocks and means defaulting can never *block* a load — it only ever adds values.
Callers pass names derived from the schema (a keyword or a field's model ref), not
free-form strings, so a no-op is the correct outcome for the rare unmodeled block
rather than a silent bug.

No trait is added to `nml-core`: an earlier sketch put a `BodyDefaulter` trait in
core implemented only by `nml-validate`, which is a needless dependency inversion.
Keeping orchestration as free functions in the crate that already owns `de` is
strictly cleaner.

## 10. Default type checking (constraint checking deferred)

With typed defaults available, schema load validates that each declared default
**matches its field's declared type** — e.g. reject `count number = "high"`. The
span carried on the typed default points the diagnostic at the offending literal.
This is a real correctness gain that the typed-default carry (§7) makes possible.

**It reuses the validator's existing `validate_value_against_type`** — the exact check
already applied to instance values ([`schema.rs`](../../crates/nml-validate/src/schema.rs))
— now also applied to the declared default. This is the DRY choice and, critically,
the *correct* one: that check already encodes the primitive coercion rules, so
string-backed types accept string literals (the spec's own `sessionDuration duration
= "24h"` default is a `Value::String` against a `duration` field, and must not be
rejected). A parallel hand-written check would risk diverging from how instance values
are validated; reusing the one check makes default-checking and value-checking
provably identical.

**Constraint-checking of defaults is explicitly deferred.** The spec documents field
constraints (`<min>`, `<max>`, `<integer>`, `<minLength>`, …), but as of this RFC
**constraints are not modeled anywhere** — the parser does not parse them, `FieldDef`
has no constraints field, and the validator does not enforce them. Constraint
*modeling and enforcement* is a separate, larger gap and its own RFC; only once it
exists can defaults be checked against constraints. Promising constraint-checking
here would rest on infrastructure that does not exist, so this RFC commits to
type-checking only and notes constraint-checking as future work contingent on that
RFC.

## 11. `oneof` discriminator default (separate, now unblocked)

Once `apply_defaults` can inject an absent discriminator property before serde sees
it, a `oneof` may declare a default arm (syntax TBD in its own RFC, e.g.
`oneof email by provider = "log":`). This is deliberately **not** in this RFC — it
is the feature the TODO notes was omitted "precisely because defaults aren't
injected today." It reuses this RFC's injection path, but it is **not** a trivial
follow-up: because it also introduces union defaulting (the body-dependent variant
walk), Phase 5 carries the visitor unification (§6.2) as its mandatory first step.
The injection *mechanism* is reused; the *walk* is unified, not reused as-is.

## 12. Legacy removal

- **`format_default`** — deleted (§7).
- **`O(n)` `find_model`/`find_oneof` scans** — replaced by `SchemaIndex` (§6.1).
- **`from_body_resolved` / `from_block`** — **kept; not legacy.** `nml-core` is a
  standalone config library usable without `nml-validate` (its `lib.rs` doctests
  deserialize via `from_block` with no schema in sight), so its schema-less
  deserialization API is a supported feature for external embedders, not duplicated
  capability. The new `from_*_defaulted` functions do *not* supersede it — they require a
  `SchemaValidator` a schema-less caller does not have. What migrates is only nudge's
  *internal* use of `from_body_resolved` on blocks that **do** have a schema; those
  call sites move to the defaulted entry points. `from_body_resolved` itself stays as
  `nml-core`'s documented no-schema path.
- **Serde value-defaults** — **kept, not removed** (refined during implementation).
  The original plan was to delete `#[serde(default = "fn")]`s the schema now injects.
  But a serde value-default also backstops the **plain-serde, no-schema** path
  (`from_block` / `from_body_resolved`) — nml-core's documented general-library API,
  used by the unmodeled `worker` block, the drift test, and any external embedder of
  these structs. The defaulting pass only supplies the value *when a schema is
  present*, so removing the serde default would **reduce capability** for the
  schema-less path (omitted fields would fail to deserialize). By the same
  "remove-only-if-equivalent-capability" rule that keeps `from_body_resolved` (§12
  above), the serde value-default is a legitimate no-schema fallback, not legacy.
  Single-source-of-truth is achieved a different way: the **drift test (§13) makes
  the dual declaration provably consistent** — the schema is authoritative for
  schema'd loads, and the serde default can never silently diverge from it. `Option<T>`
  optionals keep `#[serde(default)]` for the same reason as before (absence ⇒ `None`).

## 13. Drift safety net

Serde defaults are functions/`Default` impls and **cannot be introspected at
runtime**, so the test cannot reflect on the `#[serde(default)]` attributes directly.
Instead it asserts **behavioral equivalence**: for each defaulted struct, deserialize
an *empty* (or minimal-required) block two ways — once through the new
defaulting path (schema injects the values) and once through plain serde (its
`#[serde(default)]` supplies them) — and assert the two results are equal. Equality is
compared via `serde_json::to_value` rather than `PartialEq`, so the test needs only
`Serialize` (already derived on config structs) and does not force a `PartialEq`
derive onto every tested type. Any divergence between an NML-declared default and its
serde counterpart fails the test. This kills the drift problem immediately: it makes
the dual declaration (schema default + serde default, both retained per §12) provably
consistent, so the schema is authoritative for schema'd loads while the serde default
remains a safe no-schema fallback that can never silently diverge. A proc-macro that
*generates* serde defaults from the schema was considered and rejected (§14).

## 14. Alternatives considered

- **`BodyDefaulter` trait in `nml-core`** — rejected: dependency inversion, extra
  public surface, no benefit over orchestrating as free functions in `nml-core` (§9).
- **`SchemaValidator::deserialize_block` method** — rejected: pulls serde and a
  deserialize responsibility into the validation crate; free functions in `nml-core`
  keep each crate single-purpose (§9).
- **New `nml-default` crate** — rejected: it would duplicate the schema dispatch
  that lives in `nml-validate`/`nml-core`; the shared `SchemaIndex` + `resolve_field`
  (§6) is DRYer.
- **Defaulting inside `de.rs`** — rejected: couples the deserializer to schema
  types; the TODO explicitly forbids it; `de.rs` is handed a target type, not a
  model name.
- **Compile-time codegen of serde defaults from `.nml`** — rejected: heavy, brittle,
  worse build ergonomics; the drift test (§13) covers the problem cheaply.
- **Resolution first (current order)** — rejected: exposes resolved secrets to the
  schema-aware crate for no benefit (§5).

## 15. Phasing

- **Phase 0 — typed-default carry.** `model::FieldDef.default_value:
  Option<SpannedValue>`; carry the typed value in `convert_field_def`; delete
  `format_default` and its one extraction-test assertion. No change to deserialized
  values; `schema.rs` and `nml-fmt` need no edits (§7).
- **Phase 1 — `SchemaIndex` + shared dispatch.** Extract the order-preserving index
  (§6.1) and the `resolve_ref` / `resolve_field` dispatch (§6.2); switch the
  validator's lookups onto the index **and** its nested-block / list-item model-ref
  dispatch onto `resolve_ref` (`validate_ref_instance`), so the model-or-oneof
  decision has one definition shared with the defaulter. The body-dependent
  union/list dispatch stays validator-specific until the Phase 5 visitor. Low risk,
  behavior-preserving (guarded by the validator suite).
- **Phase 2 — defaulting pass + type checks.** `apply_defaults`;
  `from_body_defaulted`/`from_block_defaulted`; the resolve-last reorder implemented
  **in the new orchestration only** (§5.1), not by mutating `from_body_resolved`;
  default type-checking (§10); DoS bounds (§8.1). Full unit tests.
- **Phase 3 — consumer wiring + drift test.** Migrate nudge's deserialize sites to
  the new entry points; add the behavioral-equivalence drift test (§13).
- **Phase 4 — drift guard, serde defaults retained.** Implementation showed the
  serde value-defaults are the no-schema fallback, not removable legacy (§12). So
  Phase 4 is *not* a removal pass: it lands the behavioral-equivalence drift test
  (§13) that keeps the schema and serde defaults provably consistent, achieving
  single-source-of-truth without sacrificing the plain-serde capability.
- **Phase 5 (separate RFC) — `oneof` discriminator default + union defaulting.**
  Gated. Its **first, non-skippable step** is the visitor unification (§6.2):
  refactor the validator's traversal and the defaulter's walk onto a single
  schema-guided visitor, guarded by both the validator and defaulting suites. Only
  then does Phase 5 add discriminator/union defaulting on the unified walk. Phase 5
  MUST NOT proceed on a duplicated walk.

## 16. Testing strategy

- Scalar injection; precedence (explicit > shared > default).
- Nested recursion; nested materialization, both defaultable and not.
- List-item and top-level array-item recursion.
- Present-discriminator `oneof` variant defaulting — both as a field and as a
  **top-level block root** (e.g. an `email`/`identityProvider`-shaped union).
- Unmodeled root keyword → defaulting is a graceful no-op; deserialization still
  succeeds (§9).
- `object`- and union-typed fields pass through untouched (§8).
- Secret-valued default is resolved (resolve-last correctness, §5): a field whose
  default is `$ENV.X` reaches serde as the resolved value, never a raw reference.
- Termination: self-referential fully-defaultable model bounded at depth limit.
- `overridden_shared_secret_is_not_resolved` (§5.1) — the resolve-last behavior
  change; plus re-pinned existing shared+secret tests.
- Injected defaults never override shared/explicit values.
- Default type-checking rejects a type-mismatched default (e.g. `count number =
  "high"`) and accepts valid string-backed / `$ENV` defaults (reusing
  `validate_value_against_type`); exercised on nudge's real embedded schemas.
- `SchemaIndex` preserves first-definition-wins and ordered iteration (§6.1).
- Spec-conformance test using the spec's own `webProfile` / `sessionDuration =
  "24h"` example.
- Behavioral-equivalence drift test (§13) for every defaulted struct.
- Validator regression suite unchanged after the Phase 1 lookup switch (and after
  the Phase 5 visitor unification, when it lands).

## 17. Risks

- **High blast radius** across all NML consumers — mitigated by Phase 0 being
  value-neutral, Phases 1–2 additive, and the Phase 3 drift test proving equivalence
  before any serde default is removed.
- **Pipeline-ordering regressions** — pinned by precedence and shared+secret tests.
- **Over-eager materialization** — bounded by the fully-defaultable rule and depth
  guard, tested both ways.
- **Resolve-last semantic change** — overridden/absent unresolvable values are no
  longer resolved (§5.1); isolated to the new orchestration path, dedicated test,
  existing shared+secret tests re-pinned.
- **Validator refactor onto the visitor (Phase 5's gating first step)** — decoupled
  from this feature; lands only when Phase 5 needs the shared complex walk, guarded
  by both the validator and defaulting suites.
