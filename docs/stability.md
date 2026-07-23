# Stability & Compatibility Policy

NML is **pre-1.0**. This page states exactly what that means, so you can
judge adoption risk on facts rather than version-number vibes.

## Versioning

- **0.x releases follow pre-1.0 semver:** breaking changes (language syntax,
  Rust API, CLI flags) may land in a **minor** release (`0.1 → 0.2`), never in
  a patch release. Every breaking change is called out in the
  [CHANGELOG](../CHANGELOG.md).
- **From 1.0**, the language grammar and the documented Rust API follow full
  semantic versioning.

## Breaking syntax changes ship with fixers

When the language changes shape, the tooling carries you across — this is a
commitment, demonstrated by the two renames already shipped:

- The old form is **rejected with a pointer**, not silently misparsed: writing
  `=>` in an arm today produces "`=>` was replaced by `->`" at the exact span.
- Where the rewrite is mechanical, `nml fmt` and the language server's
  quick-fixes apply it for you.
- Syntax changes go through an [RFC](rfcs/README.md) with a required
  Documentation section — a change is not "Implemented" until its docs and
  migration story have landed.

## Minimum supported Rust version (MSRV)

- The workspace declares `rust-version` in [Cargo.toml](../Cargo.toml); that
  is the toolchain we build and test with. It may be relaxed downward after a
  `cargo-msrv` audit, and it is never raised in a patch release.

## What is and isn't a stable interface

| Surface | Stability |
|---|---|
| Language syntax & semantics | Versioned; breaking only in 0.x minors, with fixers |
| Documented Rust API (`nml-core`, `nml-validate`, `nml-fmt`, `nml-lsp`) | Versioned; pre-1.0 changes noted in CHANGELOG |
| CLI subcommands, flags, and **exit codes** | Versioned |
| Schema package format | Gated by an explicit `formatVersion`; readers reject versions they don't support |
| Structured diagnostics (`Suggestion` spans/replacements, LSP wire shape) | Versioned |
| **Error codes** (`NML0000`) | Stable from the first published release: never renumbered, never reused; a retired code leaves a tombstone in the error index. Band grouping is allocation convenience, not API. |
| Diagnostic **message text** | Not a stable interface — do not parse it; use the structured fields (including `code`) |

## Support window

Fixes land on `main` and ship in the next release; there are no long-term
support branches before 1.0. Security reports: see [SECURITY](../SECURITY.md).
