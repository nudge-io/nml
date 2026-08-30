# RFC 0014 — Role-Conjunction Operator (`&`)

- **Status:** Implemented
**Scope:** nml-core (lexer, parser, value lowering), vscode grammar
**Consumers:** nudge (RFC 0019 §4.5 selector conjunctions)

## Summary

A single `&` between role tokens forms a **role-conjunction expression** in
value position:

```nml
|allow = [@role/admin & @role/editor, @plan/Pro]
|allow:
    - @member/acme & @role/billing
gate = @role/a & @role/b & @scope/x
```

The language carries the expression **opaquely** — nml does not know what
`@role/admin` means, and it does not interpret `&` beyond syntax. Consumers
assign the semantics (for nudge: set intersection — the identity must match
every atom). This is the same vocabulary-opacity boundary as directives
(RFC 0032) and schema vocabularies (RFC 0030).

## Why `&` (not `&&`, not `and`)

Selectors denote **sets**, not boolean runtime expressions. The languages
whose operators work on sets/types converged on single `&` for intersection
(TypeScript `A & B`, CUE unification). Doubled `&&` is C-family machinery for
disambiguating from bitwise `&` — a reason that does not exist here — and
would promise an expression language (`||`, `!`, parens) that is deliberately
out of scope: the list is already OR, subtraction is the consumer's `|deny`.
A word operator (`and`) would be nml's first bare-word operator and needs
context-sensitive keyword rules. `&` also reads as the prose ampersand to
non-programmer operators.

## Grammar

- **Lexer:** `&` is a one-character `Amp` token. It is *not* in the role-token
  continuation set, so an unquoted `&` always terminates the preceding token —
  `@role/a&@role/b` lexes identically to `@role/a & @role/b`. Unquoted `&`
  between roles is therefore unambiguously the operator; only quoted strings
  can carry a literal `&` (consumers own that residue rule).
- **Parser:** `Role (Amp Role)*` at value position (scalar values, inline
  array elements, block-list items — one shared `role_conjunction_tail`).
  Arm-selector position deliberately does **not** take the tail (an arm names
  one selector; consumers reject conjunctions there with their own guidance).
  Errors are targeted: dangling `&` → "expected a selector after '&'";
  `&&` → "'&' is the conjunction operator; '&&' is not needed" (the RFC 0006
  `=>` pattern).
- **CST:** atoms stay separate tokens inside one node — lossless.

## Lowering: the cross-repo contract

A conjunction lowers to **one** `Value::Role` (list items: one
`ListItemKind::Role`) carrying the canonical **`" & "`-joined** text —
single-spaced regardless of source spacing. That exact string is the contract
with consumers: nudge's selector parser splits on top-level `" & "`, its
`Display` emits it, and its effective-policy output prints it, so every
rendered policy is valid config syntax. (Bare role tokens can never contain
a quote or space, so this contract is unaffected by nudge's quoted-value
selector form — RFC 0055 D11, authored via NML *string* values like
`"@user/\"fred & wilma@example.com\""` — whose spans nudge's splitter treats
as literal.) Pinned by
`de::tests::role_conjunction_lowers_to_canonical_joined_text`; nudge pins the
inverse round-trip. The formatter re-renders from the lowered value, so
canonical spacing in `nml fmt` output is automatic — no formatter changes.

## Compatibility

Purely additive: `&` between roles was previously an `ErrorToken` parse
error, so no existing document changes meaning. The vscode grammar gains
`keyword.operator.conjunction.nml` (reinstall the extension, as with
RFCs 0005/0006).
