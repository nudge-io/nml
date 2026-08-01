# Contributing to NML

Thanks for your interest in NML. This document covers how to build, test, and
land a change.

## Development setup

Rust (stable, ≥ the workspace `rust-version`), Node.js **22+**, pnpm **11** (via Corepack), and [`just`](https://github.com/casey/just):

```bash
corepack enable && pnpm install   # once per clone, from repo root
just test        # cargo test --workspace
just lint        # Rust fmt + clippy + doc (matches rust-ci)
just lint-ext    # extension typecheck (matches extension-build verify)
just fmt         # cargo fmt --all
just install     # build LSP + VS Code extension and install locally
just verify-ext  # extension gate: toolchain + typecheck + unit + bundle
```

Changes under `editors/vscode/` must keep **`just verify-ext`** green (use **`just verify-ext-full`** for E2E after `just build-lsp-wasm`). PRs that touch both Rust and the extension should run **`just lint`** and **`just lint-ext`** (or **`just verify-ext`** for extension changes).

The extension toolchain gate (`pnpm run check:toolchain`) validates VS Code API floor, manifest/lockfile alignment for `@types/vscode`, Node 22+, and the Corepack-pinned pnpm version. It runs in CI, before compile/typecheck, and in the pre-commit hook when extension or lockfile files change.

Enable the repo git hooks once per clone (Rust fmt/check + extension toolchain on relevant paths):

```bash
git config core.hooksPath hooks
```

## Landing a change

1. Every change must keep `just test` and `just lint` green. Extension changes must also keep `just verify-ext` green.
2. **User-facing changes ship with their documentation — in the same PR.**
   A change is user-facing if it adds/changes syntax, a public API, a CLI
   flag, an LSP capability, or a diagnostic. Update the relevant guide
   (`docs/`), reference (`spec/`), and CHANGELOG entry. If you believe no
   docs are needed, say why in the PR description — "internal only" is a
   valid answer, silence is not.
3. Docs snippets must pass the docs verification suite (`just docs-test`) —
   tagged ```` ```nml ```` blocks in the guides run through the real CLI in
   CI (see `scripts/docs_test.py` for the tag grammar), so keep examples
   runnable and self-contained.
4. Add a CHANGELOG line under `Unreleased` for anything a user would notice.

## Language changes (RFCs)

Syntax or semantics changes go through an RFC in `docs/rfcs/` (see the
[RFC index](docs/rfcs/README.md)):

- Copy the structure of an existing RFC; number sequentially.
- An RFC includes a **Documentation** section: which guides, spec sections,
  and reference pages the change touches. **An RFC is not "Implemented" until
  its documentation has landed** — implementation and docs are one
  deliverable.
- Keep the RFC's `Status` header accurate as the work progresses; the index
  table must match.

## Style

- Rust: `cargo fmt` + clippy clean; match the surrounding code's comment
  density and naming.
- Docs: second person, present tense; every example self-contained and
  runnable; terminology per the glossary (once published, `docs/`).

## License

By contributing, you agree that your contributions are dual-licensed under
MIT OR Apache-2.0, without any additional terms or conditions.
