# RFC 0024 — ParamRef Primitive

- **Status:** Proposed
- **Date:** 2026-08-29
- **Depends on:** RFC 0008 (diagnostics), RFC 0022 (type aliases), Tape RFC 0001
- **Crates:** nml-core (parser, model, schema extraction), nml-validate,
  nml-lsp, nml-fmt
- **Consumer:** Tape RFC 0001 (inferred flow parameters)

## Summary

A schema may declare a field typed as `ParamRef` or as a union containing
`ParamRef` (typically `string | ParamRef`). In instances, the **ParamRef arm**
is written with explicit assignment: an unquoted identifier on the RHS of
`=` — a reference to a runtime parameter slot, not a string literal.

```nml
model typeStep:
    input (string | ParamRef)+

flow demo:
    steps:
        - search as typeStep:
            input = memberId
```

Tape (RFC 0001) requires every `ParamRef` identifier to name a declared
`type` alias (RFC 0022) in the loaded schema package. The parameter contract
is **inferred** from vendor step usage — no separate `params` block.

## Motivation

**1. Ergonomics.** Stringly `inputParam = "memberId"` duplicates the name,
defeats rename-safety, and splits one concept across two fields (`input` vs
`inputParam`).

**2. Parser-native disambiguation.** `input = "123"` (literal string) vs
`input = memberId` (param ref) is resolved by union arm matching on the RHS —
no parallel vocabulary. Explicit `=` is **required** for ParamRef (Tape profile);
positional `input memberId` without `=` is rejected (**NML2059** / fmt fixit).

**3. DRY contract.** Param names in steps are the contract; types live in
`type` aliases (`tape.types.nml`). Authors write each name once.

**4. Security.** `ParamRef` fields are `#sealed` on Tape's `step` model;
tenant overlays cannot introduce or retarget them (RFC 0019 + Tape
`tenant_flow_guard`).

## 1. Design

### 1.1 Schema

`ParamRef` is a new primitive field type (alongside `string`, `number`,
`secret`, …):

```
field-type-expr ::= … | "ParamRef" | "(" field-type-expr ("|" field-type-expr)+ ")"
```

Rules:

- `ParamRef` is valid only in **instance** field positions — not as a top-level
  `type` alias RHS (reject `type foo ParamRef`).
- Unions with `ParamRef` **must** include `string` when literals are also
  allowed (Tape: `input (string | ParamRef)?`). Parser uses quoted strings or
  unquoted idents on the RHS of `=` to select the union arm.
- Embedders may restrict `ParamRef` to specific models (Tape: `typeStep.input`,
  `navigateStep.url`, …).

### 1.2 Instance syntax (normative — Tape profile)

| Form | Union arm | Example |
|------|-----------|---------|
| Quoted string after `=` | `string` | `input = "hello"` |
| Unquoted ident after `=` | `ParamRef` | `input = memberId` |
| Positional ident (no `=`) | — | **Invalid** for ParamRef fields |
| `$ENV.VAR` / secret literal forms | `secret` fields only | not on `ParamRef` unions |

ParamRef RHS lexing: unquoted ident, same token class as model references.
**Not** a string literal; **not** a role selector (`@role`).

**Authoring rule (Tape):** always `field = ident` for parameter slots
(`input = memberId`, `url = appBaseUrl`). `nml-fmt` normalizes any legacy
positional form to explicit `=`.

### 1.3 Validation split (NML vs consumer)

| Layer | Responsibility |
|-------|----------------|
| **nml-validate** | Shape: field allows `ParamRef`; ident is syntactically valid; union arm resolution |
| **Consumer (Tape)** | Semantics: ident names a `type` alias; collect inference set; bind-time facet/secret checks |

Unknown alias at Tape check → **TPE0017** (consumer). NML does not require
global alias existence for reference targets (same rule as RFC 0007 §4.1).

### 1.4 Representation

```rust
pub enum ScalarValue {
    String(String),
    ParamRef(String),  // ident text
    // …
}
```

Display / fmt: re-emit `field = ident` for `ParamRef` (no quotes on ident).

### 1.5 LSP

When the active field type is `ParamRef` or `string | ParamRef`:

- After `input = ` / `url = ` (partial RHS): complete **type alias names**
  declared in the schema package (Tape: from `tape.types.nml` + vendor
  extensions).
- Hover on RHS ident: go to `type` declaration (RFC 0022).

### 1.6 Diagnostics

- **NML2059** (existing band TBD allocation): `ParamRef` value where field
  type does not include `ParamRef`.
- Consumer codes (Tape TPE0017, TPE0012, TPE0015) cover alias resolution and
  bind failures.

## 2. Tape integration (normative for consumer)

See Tape RFC 0001 § Inferred parameter contract. Summary:

1. Vendor steps use `input = memberId` / `url = appBaseUrl`.
2. `infer_params()` walks the **vendor base** layer; collects `ParamRef` idents.
3. Each ident must match `type <ident> …` in the loaded types schema.
4. `bind_params()` expands aliases; `secret` → `SecretProvider`; faceted
   strings → `StringFacets::violations`.

## 3. Out of scope

- Cross-file param namespaces (params are flow-local names; values from
  `RunContext`).
- `ParamRef` in config files outside Tape flow steps (v1).
- Optional/default params (Tape v1: all inferred params required at bind).

## 4. Rollout

1. Add `FieldType::ParamRef` to schema extraction and AST.
2. Union arm parsing: unquoted ident on RHS of `=` → `ParamRef` when allowed.
3. fmt round-trip for `ParamRef`.
4. LSP completion hook (consumer supplies alias list).
5. Land with Tape `flow.model.nml` and RFC 0022.
