# RFC 0006 — The Arrow Token: `->` Replaces `=>`

- **Status:** **Implemented** (2026-07-04, same day). Lexer `Arrow` + guidance-rejected `FatArrow`; `oneof_arm` on `->` with one-pass recovery; formatter emits/aligns `->`; all embedded fixtures + `docs/language-guide.md` + downstream nudge schemas (10 arrows, 2 files) migrated. Suites: nml workspace green (nml-core 366/0 incl. the new lexer/parser pins), nudge 3254/0. One implementation note: the fixture migration was done with exact-substring replaces after a regex attempt matched Rust match arms — arrow migrations in Rust-adjacent text must never use `=>`-pattern regexes.
- **Builds on:** [RFC 0002 — Shared Body-Aware Dispatch + `oneof` Defaults](./0002-visitor-unification-oneof-defaults-workflow-migration.md) (the `oneof` arm syntax this changes), [RFC 0004 — Lossless CST](./0004-lossless-cst.md) (the lexer/parser this touches).
- **Crates touched:** `nml-core` (`cst::lexer`, `cst::syntax`, `cst::parser`), `nml-fmt` (arm emission + alignment comment). Editors: none required (the tmLanguage has no arrow rule today; adding one for `->` is optional polish). Downstream: `nudge` schema files migrate mechanically (10 occurrences in 2 files).
- **Removes (legacy):** `=>` as accepted syntax — hard cutover, no dual-accept release. `FatArrow` remains a *lexed* token solely so the parser can reject it with a targeted, self-fixing error ("`=>` was replaced by `->`"); it is never accepted by any production.

---

## 1. Decision

NML's arm arrow is **`->`**. Today that is one production — `oneof` arms:

```nml
oneof emailProvider by kind:
    "log"      -> emailLog
    "postmark" -> emailPostmark
```

— and every future arm-shaped construct (the first arrival is nudge RFC 0018's `|denial` routing arms) uses the same token. One arrow, everywhere, forever.

## 2. Why `->` over `=>`

1. **The `=` collision.** NML is assignment-dense: `key = value` is the most common line in every file. `=>` visually embeds the assignment glyph, so the language's two most distinct constructs — binding and routing — share their dominant visual element. `->` keeps them in disjoint glyph families. This is the argument that is about NML itself rather than taste.
2. **No comparison doppelgänger.** `=>` is one transposition from `>=`, which NML's operator audience reads daily in spreadsheets. `->` has no evil twin in this language (no `<-`, no pointer syntax).
3. **Audience precedent.** Arm syntax is split across languages (Rust `=>`; Kotlin `when` and modern Java `switch` use `->`, including `else ->`). NML's readers are operators, closer to Kotlin/Java/config culture than Rust culture — the same audience argument that chose `else` over `_` in nudge RFC 0018 R3c. The implementers' Rust muscle memory is real but is the wrong thing to optimize a config language for.
4. **Semantics.** Both NML arm uses are *routing* (discriminator → model; selector → experience). `->` reads as flow; `=>` reads as binding. Flow is the truer meaning.
5. **The window.** The entire real-world surface is 10 occurrences in two in-house schema files, one production, one formatter emit site. This decision is nearly free today and impossible after stability.

## 3. Changes

| Site | Change |
|---|---|
| `cst/syntax.rs` | add `Arrow` (`->`); keep `FatArrow` documented as *reject-with-guidance only* |
| `cst/lexer.rs` | `-` followed by `>` lexes `Arrow` (2 bytes); bare `-` stays `Dash` (list items, negative numbers — unaffected: `->` requires the immediate `>`); `=` `>` continues to lex `FatArrow` |
| `cst/parser.rs` `oneof_arm` | expect `Arrow`; on `FatArrow`, emit the targeted error **"'=>' was replaced by '->' (RFC 0006)"** and recover by consuming it — the file keeps parsing so all such errors surface in one pass |
| `nml-fmt` | emit `" -> "`; alignment logic unchanged (`->` and `=>` are both 2 columns) |
| test fixtures | mechanical `=>` → `->` in embedded NML sources |
| downstream `.nml` | `nudge/src/schema/server.model.nml` (8), `nudge/src/schema/controlplane.model.nml` (2) |

## 4. Non-changes

- **No dual-accept period.** All known NML files are in-house; a compatibility mode would be legacy support for zero users. The targeted parse error *is* the migration tool: it names the fix, the fix is one character, and `nml-fmt` never emits the old form.
- **`Dash` semantics untouched.** `-5` (negative numbers) and `- item` (list entries) lex exactly as before; `Arrow` only forms on the two-byte sequence `->`.
- **The CST stays lossless.** `Arrow` is an ordinary fixed token; round-trip guarantees are unchanged.

## 5. Tests

- Lexer: `->` lexes `Arrow`; `- >` (spaced) lexes `Dash`, `Ident`-adjacent `>` error as before; `-5`, `- item` unchanged; `=>` still lexes `FatArrow`.
- Parser: `oneof` arms parse with `->`; a `=>` arm produces exactly the guidance error and parsing continues (recovery pin).
- Formatter: arms emit and align with `->`; idempotence on migrated files.
- Downstream: nudge suite green after schema migration (path dependency — no version dance).
