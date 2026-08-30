# RFC 0018 — Numeric Schema Facets

- **Status:** Implemented (2026-07-26; §1.1–§1.6 and the §2 normative
  fixtures all verified against the CLI — syntax at every placement,
  ANY-variant-admits union semantics, exact `multipleOf` including the
  full-window spread, both diagnostics with error-index sections,
  canonical `fmt` rendering, and faceted types in LSP hover)

## Summary

Let a schema constrain the *value range* of a `number` field, first-class
in the type syntax:

```nml
model server:
    port number(min = 1, max = 65535)
    weight number(min = 0, exclusiveMax = 1)
    priceStep number(multipleOf = 0.01)
```

Enforcement is **exact**. Bounds compare through `Number`'s numeric `Ord`
(RFC 0016), so a boundary can never lie the way an f64 comparison does,
and `multipleOf` is decided by exact decimal divisibility — no epsilon,
no rounding. Every mainstream validator that offers `multipleOf` decides
it in binary floating point and famously mis-answers steps like `0.1`
(JSON Schema implementations, CUE); nml answers it exactly for every
representable value, which RFC 0016 makes possible and this RFC makes
useful.

## 1. Design

### 1.1 Syntax

A parenthesized facet list may follow the `number` type name, and only
the `number` type name:

```
facet-list  := "(" facet ("," facet)* ")"
facet       := facet-key "=" number-literal
facet-key   := "min" | "max" | "exclusiveMin" | "exclusiveMax" | "multipleOf"
```

- Each key at most once per list (duplicate → definition error).
- Facet values are `number` literals (RFC 0016 grammar; no references,
  no expressions — a schema is a contract, not a program).
- The list attaches wherever the bare `number` name is legal: scalar
  fields, `[]number`, `set<number(min = 1)>`, union variants, modifier
  fields. (Arm positions parse but are enforcement-inert — arm targets
  are references or string literals, never numbers — and the
  definition rules still validate their declarations.) For collections
  the facets constrain **each element**.
- Whitespace-insensitive around and within the parentheses
  (`number (min = 1)` attaches — a `(` at type-START is the union
  branch, a different path, so nothing is ambiguous); the canonical rendering
  (nml fmt, `FieldTypeExpr::Display`) is `number(min = 1, max = 65535)`
  — single spaces around `=`, `", "` between facets, source key order
  preserved.

**Why type syntax and not `#directives`.** Directives are the *consumer*
extension point: RFC 0030/0032 give the schema-package manifest sole
ownership of the directive vocabulary (`[]directive` table), and
consumers boot-reject undeclared names. Facets are *language* semantics
— the language's own validator enforces them — so routing them through
the consumer registration channel would either fork the vocabulary table
with built-ins or make core validation package-declarable. Both wrong.
The type is the contract; the constraint is part of the type.

**Compatibility gate, for free.** Schema packages embed schema *source*.
A pre-facet reader's parser rejects `number(...)` loudly, so an old
binary can never load a faceted schema and silently under-validate — the
failure mode is a parse error, not a validation downgrade.

### 1.2 Semantics

For a candidate value `v` (a `Value::Number`) against facets `F`:

- `min = m`: reject iff `v < m`. `exclusiveMin = m`: reject iff
  `v <= m`.
- `max = m`: reject iff `v > m`. `exclusiveMax = m`: reject iff
  `v >= m`.
- `multipleOf = m`: reject iff `v` is not an exact decimal multiple of
  `m` (§1.3).

All comparisons are `Number::cmp` — numeric, exact, total. Facets are
checked only after the value has already passed the type check (a
non-number in a faceted field reports the existing type mismatch, never
a facet violation on garbage).

For a union-typed field, matching stays type-shaped — a faceted number
variant admits every number — and the matched variant's facets then
bind the value (guarded so pre-facet unions keep byte-identical
behavior). An `$ENV`-backed value (`Value::Secret`) bypasses facet
checks exactly as it bypasses every static schema check — resolution
happens after validation; this is the language's existing posture for
references, inherited, not widened.

Definition-side rules, emitted from `extract_schema` itself — the one
API every schema-consuming surface constructs through (both CLI verbs,
the LSP, the loader and therefore packages and downstream boots), so
they cannot be skipped by construction:

- Facets attach only to the `number` primitive. `string(min = 1)` is a
  definition error (length facets are out of scope — this RFC is the
  numeric-range capstone of RFC 0016, and conflating count constraints
  with value constraints under one key set is how JSON Schema ended up
  with `minimum`/`minLength`/`minItems`).
- `min`/`exclusiveMin` are mutually exclusive; likewise the max pair.
- When both ends are present the range must be satisfiable:
  `min > max` (with strictness taken into account) is a definition
  error, not an unsatisfiable trap laid for config authors.
- `multipleOf` must be `> 0`.
- A declared field default must itself satisfy the facets — reported
  as a facet *violation* (NML2057) through the same shared enforcement
  pass config values use. Defaults are judged where the schema is
  **loaded** (`extract_schema` for the facet rules; `load_schema` for
  the type rules via `default_diagnostics` — including the editor,
  whose schema load pass calls `load_schema` itself), so every
  consumer gets the check through one code path and one declared
  default yields exactly one finding however many models inherit it.

### 1.3 Exact divisibility (`Number::is_multiple_of`)

New public predicate on the RFC 0016 core, the single numeric home:

```rust
pub fn is_multiple_of(&self, m: &Number) -> bool
```

`v` is a multiple of `m` (`m != 0`) iff `v / m` is an integer. With both
in normalized form (`c_v × 10^-s_v`, `c_m × 10^-s_m`, trailing zeros
stripped), let `d = s_m − s_v`:

- `v == 0` → **true** (zero is a multiple of everything).
- `d >= 0` → true iff `c_m | c_v × 10^d`, decided as
  `(|c_v| mod |c_m|) × (10^d mod |c_m|) ≡ 0 (mod |c_m|)` — modular
  exponentiation plus one modular multiply. Residues live below
  `10^34 < 2^113`, whose products overflow `u128`, so multiplication is
  a double-and-add `mulmod` (≤ 128 iterations, branch pattern
  data-independent in length). No bignum, no allocation, O(log d)
  overall with `d ≤ 12320` bounded by the RFC 0016 scale window.
  Cost note: at the finest grid (`multipleOf = 10^-6176`) one check is
  ~15–30 µs against ~140 ns for an unfaceted number — bounded and
  linear in element count, but a schema pairing that grid with a large
  config array makes editor revalidation multi-second. Prefer the
  coarsest grid that expresses the constraint.
- `d < 0` → true iff `(c_m × 10^{-d}) | c_v`. Since `|c_v| < 10^34`,
  any divisor that overflows the checked multiply — or any `-d ≥ 34` —
  already exceeds `|c_v|`, so the answer is false without computing.

Sign is irrelevant to divisibility (`-0.2` is a multiple of `0.1`);
the predicate works on absolute coefficients. This is a *predicate*,
not arithmetic: it returns a boolean, allocates nothing, rounds
nothing, and keeps the RFC 0016 "no arithmetic engine" posture intact.

Worked truths the tests pin: `0.3 / 0.1` **is** a multiple (the classic
f64 lie says otherwise: `0.3 / 0.1 = 2.9999…96` in binary64);
the full-window spread (10^6144 against 10^-6176, both written as
plain literals) decides correctly without overflow; `2.50` is a multiple of `0.05` in every cohort spelling
(divisibility is value-based, scale-independent).

### 1.4 Representation

- **AST** (`ast.rs`): `FieldTypeExpr::Named` gains an optional facet
  list — `Named { name: Identifier, facets: Vec<FacetExpr> }` with
  `FacetExpr { key, value: SpannedValue, span }`; empty vec = bare name.
  `Display` renders the canonical form. Serialization: facets appear in
  the AST dump as part of the type expression (externally tagged like
  every other node).
- **Model** (`model.rs`): `FieldType::Primitive(PrimitiveType)` becomes
  `Primitive { ty: PrimitiveType, facets: NumberFacets }`, where
  `NumberFacets { min: Option<FacetBound>, max: Option<FacetBound>,
  multiple_of: Option<FacetValue> }` and `FacetBound` carries
  `{ value: Number, exclusive: bool, span }`. `NumberFacets::NONE` is
  the empty constant; a struct variant (not a wrapper variant) so every
  existing `Primitive(...)` pattern breaks at compile time and each of
  the ~31 match sites is reviewed rather than silently bypassed — the
  same compiler-driven totality this codebase used for the RFC 0016
  enum replacement.
- Non-number primitives always carry `NumberFacets::NONE` (the loader
  rejects facets on them before the model exists).

### 1.5 Diagnostics

Two new codes in the schema band, following the RFC 0009 taxonomy:

- **NML2057 — facet violation** (config side): value outside a declared
  facet. Message names the field, the offending facet spelled as
  authored, and the value (raw-AST band, bounded — numbers display at
  most their written length): `'port' is 70000, above the schema's
  max = 65535`. The facet's authored spelling rides in the message
  itself — schema and config are usually different files, and a
  same-file-only related span would point nowhere; the message carries
  both halves of the contradiction instead. No machine fix: rounding
  is never proposed
  (RFC 0016 doctrine); clamping a value the author wrote is a semantic
  decision only they can make.
- **NML2058 — facet definition error** (schema side): facet on a
  non-number type, duplicate/conflicting keys, `min > max`, or
  `multipleOf <= 0`. Message states the rule; span points at the
  offending facet. (Two things are deliberately NOT here: a non-number
  facet *value* is a parse error — the grammar admits only number
  literals — and a default violating its own facets is **NML2057**, a
  value breaking a constraint, reported where values are.)

Both get error-index entries with worked examples and `nml explain`
coverage, and flow through every existing surface (CLI, LSP publish,
`check --strict`) with zero new plumbing — they are ordinary schema
diagnostics.

### 1.6 Tooling

- **fmt**: renders via `FieldTypeExpr`'s `Display` (source key order
  preserved); facet lists round-trip byte-identically once canonical.
- **LSP**: field hover shows the full faceted type for free via the
  model `FieldType`'s `Display` — canonical key order (min, max,
  multipleOf), which may reorder relative to the source; both
  renderers emit the same spacing/punctuation form.
- **Packages** (RFC 0030): no format change — source travels, and the
  parse gate covers downgrade (§1.1).

## 2. Fixtures (normative)

- `port number(min = 1, max = 65535)`: `0` rejected (`min`), `1`
  accepted (inclusive), `65535` accepted, `65536` rejected, `80.0`
  accepted (RFC 0016 value-based — it IS 80).
- `weight number(min = 0, exclusiveMax = 1)`: `0` accepted, `1`
  rejected, `0.9999999999999999999999999999999999` (34 digits) accepted.
- `step number(multipleOf = 0.1)`: `0.3` **accepted** (exactness pin),
  `0.25` rejected, `0` accepted, `-0.2` accepted.
- `multipleOf` at the finest grid (10^-6176, written as `0.` + 6175
  zeros + `1` — e-notation is not literal grammar): admits the whole
  domain, and the check completes without overflow against 10^6144.
- Definition errors: `string(min = 1)`, `number(min = 2, max = 1)`,
  `number(multipleOf = 0)`, `number(min = 1, exclusiveMin = 0)`,
  duplicate `min`, `retries number(min = 0) = -1` (default violates).

## 3. Out of scope

String/collection length facets (different keys) → **RFC 0021** (`pattern`,
`minLength`, `maxLength` on `string`); collection cardinality (`minItems`)
remains a different RFC if ever demanded; facet expressions or references
(schemas are contracts);
money facets (needs a currency-match rule — deferred until a consumer
asks); facet-aware completion snippets.

**Duration facets** (`duration(min = 30s)`) were deferred here and have
since LANDED — the deferral closed once RFC 0017's semantic equality
made the family well-defined. `duration` is now a legal facet carrier
beside `number`, generic over one `FacetDomain` (`Facets<T>`; the
number and duration records are the same code): `min`/`max`/
`exclusiveMin`/`exclusiveMax` compare by `total_nanos` (so
`min = 1000ms` and `min = 1s` are the same bound, and an
`exclusiveMin = 1000ms, max = 1s` range is unsatisfiable), and
`multipleOf` is UNIT-BLIND nanos divisibility — `1500ms` is a multiple
of `250ms` and of `500ms` but not of `1s`; alignment semantics beyond
divisibility ("every 5 minutes, on the clock") remain a scheduling
concept and stay out. Bounds are duration LITERALS (`min = 5s`); a
unitless bound on a `duration` field, a duration bound on a `number`
field, and `multipleOf = 0s` are NML2058 definition errors. Facets on
any other type remain rejected with the message naming the type.
