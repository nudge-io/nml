# RFC 0022 — Type Aliases

- **Status:** Proposed
- **Date:** 2026-08-29
- **Depends on:** RFC 0018 (facet grammar), RFC 0021 (string facets), RFC 0009
  (diagnostic bands)
- **Crates:** nml-core (parser, model, schema extraction), nml-validate,
  nml-lsp, nml-cli
- **Docs in scope:** `spec/syntax.md`, `spec/types.md`, `spec/models.md`,
  `docs/language-guide.md`, `crates/nml-core/assets/error-index.md`

## Summary

Let schema authors name a type expression once and reuse it everywhere a
field type is written:

```nml
type memberId string(pattern = "^\\d{10}$", minLength = 10, maxLength = 10)
type appBaseUrl string(pattern = "^https://")
type apiToken secret
type routingKey (string | number)

model endpoint:
    memberId memberId
    apiKey apiToken
```

**Syntax rule:** `type` aliases attach the type expression with a **space**
(same as model fields: `port number(min = 1)`). **`=` is not used** — that
token is reserved for **values** in instances (`required = true`).

Instances use aliases as field types (`memberId memberId`) or as **ParamRef**
RHS in Tape flows (`input = memberId` — RFC 0024). Resolution is
**transparent** — an alias expands to its underlying `FieldType` before
validation; no wrapper type at runtime.

## Motivation

**1. Ergonomics.** RFC 0021 allows `string(pattern = …)` inline on every
field. Real configs repeat the same shapes (member IDs, HTTPS URLs, API
tokens). Aliases keep schemas DRY without a second validation language.

**2. Tape inferred params.** Flow steps reference params by name
(`input = memberId`). Each name must match a `type` alias; the contract is
inferred from step usage (Tape RFC 0001) — no parallel param declaration
language.

**3. One engine.** Alias resolution happens before facet enforcement and
union matching. `nml-validate`, `bind_params`, and the RFC 0047 resolved
lane all see the expanded type — no embedder-specific type registries.

## 1. Design

### 1.1 Syntax

A top-level declaration introduces an alias:

```
type-decl  := "type" ident field-type-expr
```

The `field-type-expr` is the same production used after a field name in a
`model` body — primitives (with facets), models, enums, unions, modifiers,
lists, sets. **No `=` between the alias name and the type expression.**

Examples:

```nml
type port number(min = 1, max = 65535)
type slug string(pattern = "^[a-z0-9-]+$", minLength = 3, maxLength = 64)
type credential secret
type label string
type routingKey (string | number)
type tags []string(pattern = "^[a-z]+$")
```

Rules:

- `type` is a new top-level keyword beside `model`, `enum`, `oneof`, `trait`,
  `role`.
- Aliases are **not** instantiable blocks — no `type foo:` instance syntax.
- Alias names share the model namespace charset (lowercase ident convention;
  same as model names).
- **No forward references** — the RHS may reference only primitives, models,
  enums, and aliases declared earlier in the same schema load unit (file
  order within a package schema source, then package `[]schema` list order).
  Forward reference → NML2070 or definition error on the `type` span.

**Alias vs model.** A `model` declares a composite shape instances can
author. A `type` declares a synonym for an existing type expression.
`type memberId string(…)` does not create a `memberId:` block keyword for
instances unless a separate `model memberId` also exists (discouraged — same
name for both is a definition error).

**Alias vs instance `type` field.** Top-level `type memberId string` names a
schema alias. This is distinct from unrelated uses of the word `type` as a
**property name** on other models (e.g. `kind = "foo"`).

### 1.2 Unions and complex RHS

**Yes — unions are supported.** The RHS is any legal field-type expression,
including parenthesized unions:

```nml
type routingKey (string | number)
type contact (string(pattern = "^.+@.+$") | secret)
```

Semantics are **identical** to writing the union inline on a field — the
alias is transparent. Union matching, facet enforcement per variant, and
RFC 0047 resolved-lane behavior are unchanged; only the spelling is named.

Guidance:

- **Prefer named unions** when the same `(A | B)` appears on multiple fields.
- **Prefer separate aliases** over wide unions when variants need different
  runtime handling (e.g. Tape `credential` vs `memberId` — two aliases, not
  one union param type).
- Facets on union members apply **per variant** after expansion, same as
  inline unions today.

### 1.3 Resolution

Resolution is **compile-time on the schema index**:

1. Load all `type` declarations into `TypeAliasIndex`.
2. When a field declares `fieldName aliasName` or an instance value is
   checked against a field typed to hold an alias name, **expand** `aliasName`
   to its `FieldType` (transitively if aliases chain).
3. Enforce facets, unions, and primitives on the expanded type.

**Chaining:** `type a string(minLength = 1)` and `type b a` — `b` expands
to `string(minLength = 1)`. Maximum chain depth **16**; deeper → NML2071.

**Cycles:** `type a b` / `type b a` → NML2071 at schema load.

### 1.4 Use sites

| Site | Example | Behavior |
|------|---------|----------|
| Model field type | `id memberId` | Expand `memberId` before validating instance values |
| Union variant | `(slug \| memberId)` | Expand each variant |
| List element | `[]memberId` | Expand element type |
| `ParamRef` target (Tape) | `input = memberId` on `string \| ParamRef` field | Consumer resolves ident against alias index (TPE0017) |

### 1.5 Diagnostics

- **NML2070 — unknown type alias**: field or instance references `foo` as a
  type, but no `type foo …` exists. Message lists nearby alias names
  (did-you-mean).
- **NML2071 — type alias cycle or excessive depth**: `type a b` /
  `type b a`, or chain depth > 16.

Alias definition errors (invalid RHS, facet on wrong primitive) remain
**NML2058** on the `type` declaration span.

### 1.6 Representation

```rust
pub struct TypeAliasDef {
    pub name: String,
    pub expanded: FieldType,  // fully expanded, cycle-checked at load
    pub span: Span,
}
```

Display / fmt: prefer the alias name when the expanded type matches exactly.
Hover may show `memberId (string(pattern = …))` without `=`.

### 1.7 Tooling

- **LSP completion:** after `type = ` on alias-ref fields, complete declared
  alias names; on `type ` at schema top level, complete type-expression starters.
- **Go to definition:** on alias use → `type` declaration.
- **Packages (RFC 0030):** aliases live in schema source files listed in
  `[]schema`; hashed with the rest of the schema package.

## 2. Fixtures (normative)

Schema:

```nml
type memberId string(pattern = "^\\d{10}$")
type credential secret
type routingKey (string | number)

model account:
    id memberId
    token credential
    key routingKey
```

Config:

```nml
account Acme:
    id = "1234567890"
    token = $ENV.API_TOKEN
    key = "route-42"
    key = 42
```

- `id = "abc"` → NML2057 (pattern mismatch after alias expansion).
- `token = "literal"` → secret-literal diagnostic.
- `key = true` → type mismatch (union is string | number only).

Tape-style param (inferred — see Tape RFC 0001; NML RFC 0024 `ParamRef`):

```nml
type memberId string(pattern = "^\\d{10}$")

flow demo:
    steps:
        - search as typeStep:
            input = memberId
```

Definition errors:

- `type a b` / `type b a` (cycle → NML2071)
- `type x unknownModel` (unknown RHS)
- `model memberId:` and `type memberId string` in same file (name clash)

## 3. Consumers

### Tape (RFC 0001)

- `tape-schemas` ships `tape.types.nml` with shared aliases (`apiToken`,
  `memberId`, `appBaseUrl`, …).
- Flow steps use `ParamRef` on `input` / `url` (RFC 0024); param names must
  match `type` alias names. The contract is **inferred** from vendor step
  usage — no `flow.params` block.
- `infer_params()` + `bind_params()` expand aliases →
  `StringFacets::violations` or `secret` handling in `nml-core`.

### Nudge / general embedders

Slug types, port aliases, and shared credential types in schema packages
without wrapper models.

## 4. Documentation

When implemented, update `spec/*`, language guide, error-index (NML2070/2071),
RFC 0021 cross-link.

## 5. Out of scope

- **Parameterized generics** (`type listOf<T> []T`) — not v1.
- **Opaque branded types** — aliases are transparent synonyms.
- **Cross-package alias imports** — package-local; reuse via multiple schema
  files in one `[]schema` mount.
- **`type` declarations in config instance files** — schema only.

## 6. Rollout

1. Parse `type Name <field-type-expr>` in schema extraction.
2. Build `TypeAliasIndex` with cycle detection and expansion (including
   union RHS).
3. Resolve alias names at field-type positions. Tape resolves `ParamRef`
   idents against the alias index at `tape check` (TPE0017).
4. LSP + fmt + error index.
5. Land with RFC 0021 when possible.

No config-file migration — purely additive schema vocabulary.
