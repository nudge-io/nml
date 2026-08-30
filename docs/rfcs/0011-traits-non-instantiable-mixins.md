# RFC 0011 — Traits: non-instantiable mixins

- **Status:** Implemented
- Date: 2026-07-23

## Summary

`trait Name:` declares a reusable bundle of fields that models (and other
traits) compose with `is` — and that can never be instantiated as a block,
referenced as a field type, or targeted by a `oneof` arm. Traits close the
abstract/concrete gap: today every mixin is a full `model`, so every shared
field bundle is also a legal block keyword users can author against, and
completion offers it. The distinction is standard in typed configuration
(Pkl `abstract class`; CUE definitions vs concrete values).

## Motivation

The tutorial work (2026-07-23) found `trait` was documented and parsed but
**silently ignored by schema extraction**: trait fields were invisible to
validation (unknown-property errors under `--strict`), their defaults never
applied, and `is`-of-a-trait resolved to nothing — with no diagnostic
anywhere in the chain. The teaching fallback — `model` used as a mixin —
works but leaks: the mixin becomes a real block type in strict mode and in
editor completion, and intent ("this is a capability, not a thing") is
unrecoverable from the schema.

Two failure classes motivate the enforcement half of this RFC:

1. The fiction survived because **an unresolved `is` target is silent**.
   That is fixed here independently of traits: every `is` target must now
   resolve, with a did-you-mean.
2. A schema author cannot say "not a block type". With schema packages
   binding user files `strict = true` (RFC 0030), a mixin-model lets users
   author meaningless-but-valid blocks against vocabulary the author never
   intended to expose.

## Design

### Representation: a kind, not a new type

A trait is structurally a model — fields, defaults, directives, docs,
`extends`. `ModelDef` gains a kind rather than a parallel type:

```rust
pub enum ModelKind { Model, Trait }   // on ModelDef as `kind`
```

Traits live in `ExtractedSchema::models` and the shared name namespace.
Everything that is *supposed* to treat traits like models — inheritance
resolution (`resolve_model_inheritance`), cycle detection, duplicate/
reserved-name checks, post-merge positional arity (NML2011), field
extraction incl. modifiers/directives/doc-comments — works unchanged and
stays DRY. Everything that must *not* treat a trait as a model gates on
`kind`. `SchemaValidator::new` / `SchemaIndex::build` signatures are
unchanged; downstream consumers (defaults, diff, serde) see only resolved
concrete models and need no changes.

### Composition rules

- `model X is A, B:` and `trait T is A, B:` — `is` composes **models and
  traits**, in either direction. Fields merge ancestor-first; own fields
  override (unchanged semantics).
- Every `is` target must resolve to a model or trait. Unknown → error with
  a did-you-mean over models ∪ traits. Enum/oneof target → error.
- A trait may not be: a block keyword, an array-declaration element
  keyword, a field type (including through `[]`, `set<>`, unions, `|`
  modifier types, and `(K -> V)` arm keys/targets), or a `oneof` arm
  target.

### Diagnostics (stable codes, error-index sections, all CI-verified)

| Code | Meaning |
|---|---|
| NML2020 | `is` target does not resolve to a model or trait (did-you-mean) |
| NML2021 | `is` target is an enum or `oneof` |
| NML2022 | a trait is referenced as a field type |
| NML2023 | a `oneof` arm targets a trait |
| NML2024 | a trait is instantiated as a block / array keyword |

NML2020–2023 are schema-load errors from a new core finder
(`nml_core::schema::find_composition_errors`, plus a `find_oneof_errors`
extension for 2023), run **before** inheritance resolution so each error
reports once, at the declaring definition. NML2024 is instance validation
and errors **even in lenient mode**: a trait keyword is never "some other
tool's block" — the schema declared the name, so using it as a block is
definitely a mistake (same reasoning as NML2003's lenient error). The
previously uncoded, suggestion-free "unknown parent model" path in the
validator collapses into NML2020.

### Editor surface

- Strict-mode unknown-keyword did-you-mean candidates exclude traits
  (this reaches the editor through the shared validator); `trait` joins
  the LSP's language-keyword completions. Block-keyword completions are
  otherwise derived from project config and workspace scans, which never
  contain trait names unless the user already wrote the (squiggled)
  error.
- All new diagnostics flow through the existing single Diagnostic→LSP
  converter — squiggles, codes, and hints arrive with no LSP-specific
  code.

### One finding, one owner (review hardening)

The validator keeps an in-file twin of the `is`-target checks for editor
use, with two guarantees added in review: a self-contained file's own
declarations always resolve (no false NML2020 against a foreign schema
set), and callers that route definitions through the loader pipeline
(`SchemaValidator::composition_checked_at_load`) silence the twin so the
CLI never double-reports. Definition-anchored loader findings carry their
declaring source (stamped at load), so they render `file:line:col`.

## Alternatives considered

- **Separate `TraitDef` type / separate `ExtractedSchema.traits` vec** —
  makes illegal states unrepresentable in the type system, but duplicates
  every field-handling path (extraction, merge, hover, arity checks) and
  breaks the public `SchemaValidator::new(models, enums, oneofs)` shape.
  The kind flag keeps one field pipeline and puts the enforcement at the
  few sites where the distinction is real.
- **`abstract` marker on `model`** (Pkl-style) — avoids a keyword, but
  `trait` is already in the wild in NML docs/specs and reads better at the
  declaration site. No grammar work either way (the parser already accepts
  any block keyword).
- **Do nothing / retire the word** — rejected by product decision: the
  strict-mode vocabulary leak is real, and the docs already promise
  traits.

## Compatibility

Pre-publish, pre-1.0: no released surface changes. `ModelDef` gains a
field (all construction sites are in-repo; nudge only reads). Schemas that
declared `trait` blocks previously got *silent nothing*; they now get real
semantics — strictly more checking, which can surface new (correct)
errors in files that leaned on the fiction. In-repo fixtures and docs are
updated in the same change (`spec/examples`, `tests/fixtures`, tutorial
chapters 4–9, language guide).

## Deferred

- **Required-but-undefaulted trait members** ("interface" traits: a
  composing model must supply `x`) — useful, larger semantic change,
  nothing needs it yet.
- Trait-aware hover/go-to-definition affordances beyond what the shared
  index already provides.
