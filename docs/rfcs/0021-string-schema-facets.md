# RFC 0021 — String Schema Facets

- **Status:** Proposed
- **Date:** 2026-08-29
- **Depends on:** RFC 0018 (facet grammar, NML2057/NML2058), RFC 0009
  (diagnostic bands), RFC 0047 (resolved lane — implemented in
  `nml-validate`)
- **Related:** RFC 0022 (type aliases — named reuse of faceted types)
- **Crates:** nml-core (parser, model, schema extraction), nml-validate,
  nml-lsp, nml-cli
- **Docs in scope:** `spec/syntax.md`, `spec/types.md`, `spec/models.md`,
  `docs/language-guide.md`, `crates/nml-core/assets/error-index.md`

## Summary

Let a schema constrain the *shape* of a `string` field, first-class in the
type syntax — the string counterpart to RFC 0018's numeric range facets:

```nml
type memberId string(pattern = "^\\d{10}$")   # RFC 0022 alias; inline also legal

model endpoint:
    baseUrl string(pattern = "^https://")
    slug string(pattern = "^[a-z0-9-]+$", minLength = 3, maxLength = 64)
    code string(minLength = 10, maxLength = 10)
```

Three facet keys, all string-specific (RFC 0018 deliberately avoided
reusing `min`/`max` for length — JSON Schema's `minimum` vs `minLength`
split):

| Key | Value type | Meaning |
|-----|------------|---------|
| `pattern` | string literal | Entire value must match this regex (singular — one pattern per list) |
| `minLength` | non-negative integer literal | Length ≥ bound (inclusive) |
| `maxLength` | non-negative integer literal | Length ≤ bound (inclusive) |

Enforcement applies to **authored literals** at `nml check` time and to
**resolved `$ENV` text** when the validator owns resolution
(`SchemaValidator::with_env_resolution` — RFC 0047). `secret`-typed fields
never enter the resolved facet lane (credentials must not be materialized
solely to validate).

## Motivation

**1. Typed params without a parallel mini-language.** Tape (and other
embedders) need member IDs, URLs, and labels validated at bind time.
Today the only string-level primitive is bare `string` or reference-only
`secret`. Facets let embedders declare shapes once (often via RFC 0022
aliases) without capability regex lists or embedder-specific validators.

**2. One facet family per domain.** RFC 0018 established that
`number(min = …)` constrains numeric magnitude and that length belongs
under different keys. This RFC closes the gap for `string` only.

**3. JSON Schema familiarity — with intentional strictness.** `pattern`,
`minLength`, and `maxLength` are portable names. NML uses camelCase
consistently (`exclusiveMin`, not `exclMin`). **`pattern` uses full-string
matching** (stricter than JSON Schema's default substring match — see
§1.2).

## 1. Design

### 1.1 Syntax

A parenthesized facet list may follow the `string` type name:

```
facet-list   := "(" facet ("," facet)* ")"
string-facet := string-facet-key "=" facet-value
string-facet-key := "pattern" | "minLength" | "maxLength"
facet-value  := string-literal           ; for `pattern`
              | non-negative-int-literal ; for `minLength` / `maxLength`
```

- Each key at most once per list (duplicate → NML2058).
- Facet values are **literals only** — no references, no expressions.
- The list attaches wherever the bare `string` name is legal: scalar
  fields, `[]string(…)`, `set<string(…)>`, union variants, modifier
  fields. For collections the facets constrain **each element**.
- Canonical rendering: `string(pattern = "^https://", minLength = 3)`.
- `pattern` is **singular** — alternation belongs inside the regex
  (`^(a|b)$`), not a second key.

**Why not `patterns` (plural)?** One key, one regex, one rule — no
classifier for authors.

**Why not `min` / `max` on `string`?** Magnitude bounds stay on
`number`/`duration`; length uses `minLength`/`maxLength`.

### 1.2 Semantics

For a candidate string value `v` against string facets `F`:

- **`pattern = p`:** reject iff the **entire** string does not match regex
  `p` (full-string / implicit anchoring). This is **stricter than JSON
  Schema**, where `pattern` matches any substring unless the author adds
  `^`/`$`. NML chooses full match for param-validation safety.
- **`minLength = n`:** reject iff `len(v) < n`.
- **`maxLength = n`:** reject iff `len(v) > n`.

**Length** is Unicode scalar count (`str::chars().count()`), not UTF-8 byte
length and not grapheme clusters. Document as JSON Schema–aligned intent with
the usual scalar-count implementation.

Facets run only after the value passes the type check. Union matching stays
type-shaped; matched variant facets bind the value (RFC 0018 path).

#### Literal lane

Authored `Value::String` literals → NML2057. Values may appear in diagnostics
(truncated at 64 characters with `…`).

#### Resolved lane (RFC 0047)

When `SchemaValidator::with_env_resolution` is configured, `$ENV` /
`Value::Secret` / fallback chains on **faceted non-`secret` fields** enter
`check_resolved_facets` (already shipped for `number`/`duration`). String
facets extend the same function:

- Resolve the deferred value once (whole-chain semantics unchanged).
- For `Value::Resolved(text)` on a `string` field with facets: validate
  `text.as_str()` against `StringFacets`.
- Diagnostics use the **provenance-redacting** form: name `$ENV.KEY` and the
  violated bound/pattern — **never** echo resolved secret text (mirror
  `validate_facets_resolved` for numbers).

`secret`-typed fields **do not** enter this lane — the existing guard
prevents reading `$ENV` solely to judge facets on credential fields.

#### `secret` primitive

No facets on `secret` (NML2058). Shape constraints on resolved secrets
belong in the embedder consumer (`bind_params`, vault policy), not static
schema check of file contents.

### 1.3 Regex engine

- **Engine:** `regex` crate (linear-time guarantees on supported syntax).
- **Compilation:** invalid `pattern` → NML2058 at schema load.
- **Size limit:** `pattern` source length ≤ **512 characters** at schema
  load (NML2058). Mitigates author-error ReDoS and keeps schemas reviewable.
- **Dialect:** no backreferences or look-around; document supported subset
  in `spec/models.md`.

### 1.4 Definition-side rules (NML2058)

- Facets attach only to `string`. `number(minLength = 1)` and
  `secret(pattern = "x")` are errors.
- `minLength` / `maxLength`: non-negative integers only.
- `minLength > maxLength` when both present → unsatisfiable range error.
- Field defaults must satisfy facets → NML2057 at schema load.
- Unknown keys on `string` → error listing `pattern`, `minLength`,
  `maxLength`.
- Update carrier message: facets attach to `` `number` ``, `` `duration` ``,
  and `` `string` ``.

### 1.5 Value-side rules (NML2057)

Reuse NML2057 for literal violations. Pattern: **does not match** the
schema's `pattern = …`. Length: **below** / **above** `minLength` /
`maxLength` (parallel to numeric min/max wording).

Resolved lane: `'field' from $ENV.KEY resolved to a value …` with no literal
echo (RFC 0047 style).

### 1.6 Representation

```rust
pub struct StringFacets {
    pub pattern: Option<PatternFacet>,  // source + compiled Regex + span
    pub min_length: Option<LengthFacet>,
    pub max_length: Option<LengthFacet>,
}

impl StringFacets {
    /// Literal lane — value may appear in messages (truncated).
    pub fn violations(&self, s: &str) -> Vec<String> { … }
    /// Resolved lane — descriptions only, no echo of `s`.
    pub fn violation_descriptions(&self, s: &str) -> Vec<String> { … }
}
```

- **`PrimitiveFacets::String(Box<StringFacets>)`** beside `Number` and
  `Duration`.
- **Do not** add string keys to `Facets<T>` / `FacetDomain` — parallel
  record, shared extraction pipeline, separate predicate.
- **`regex` dependency:** `nml-core` (so embedders like Tape call
  `StringFacets::violations` without pulling all of `nml-validate`).

### 1.7 Tooling

- **fmt / LSP hover:** show full faceted type.
- **Completion:** facet keys inside `string(`.
- **Packages:** no manifest format change.

## 2. Fixtures (normative)

(See prior acceptance tables for literal config — unchanged.)

**Resolved lane (normative, requires `with_env_resolution`):**

```nml
model svc:
    code string(pattern = "^\\d{10}$")
```

```nml
svc S:
    code = $ENV.MEMBER_CODE
```

| Resolved `$ENV.MEMBER_CODE` | Result |
|----------------------------|--------|
| `"1234567890"` | accept |
| `"abc"` | NML2057; message names `$ENV.MEMBER_CODE`, not value |

## 3. Consumers

### Tape (RFC 0001)

- `flowParam.type` names an RFC 0022 alias (`credential`, `memberId`, …).
- `bind_params()` expands alias → `StringFacets::violations` or `secret`
  handling — **same functions as `nml check`**.
- `literalInputPolicy = param-only` on capabilities governs step literals;
  not replaced by this RFC.

### Nudge / general embedders

HTTPS URLs, slugs, fixed-width codes — schema-native, no custom validators.

## 4. Documentation

Update `spec/*`, language guide, error-index (NML2057/2058 string examples),
RFC 0018 §3 forward pointer to RFC 0021.

## 5. Out of scope

- **`secret` facets** — reference-only type; see §1.2.
- **`patterns` plural** — use alternation in one regex.
- **`min` / `max` on `string`** — use `minLength` / `maxLength`.
- **`forbidPattern` / SSN builtins** — embedder policy (Tape
  `literalInputPolicy` + `tape check` literal scanner).
- **Collection cardinality** (`minItems`) — different RFC.
- **Unicode normalization (NFC)** — v1 compares raw scalars; document
  limitation; optional `normalize = nfc` facet deferred.
- **Type aliases** — RFC 0022 (required for ergonomic Tape params).

## 6. Rollout

1. `StringFacets` + `PrimitiveFacets::String` in `nml-core`
2. Schema extraction + regex compile + 512-char limit
3. Literal enforcement in `nml-validate`
4. **Extend `check_resolved_facets`** + `resolved_facet_tests` for strings
5. Docs + error-index → Implemented

Ship alongside or immediately before RFC 0022 when Tape typed params land.
