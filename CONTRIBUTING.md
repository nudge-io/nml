# Contributing to NML

Thanks for your interest in NML. This document covers how to build, test, and
land a change.

## Development setup

Rust (stable, ≥ the workspace `rust-version`), Node.js **22+**, pnpm **11**
(via Corepack), [`just`](https://github.com/casey/just), and **Python 3.9+
with PyYAML** — `scripts/gate.py` reads the workflows through it, and it is
the one third-party Python package the repository uses, so a stock
interpreter without it refuses the first gate:

```bash
python3 -c "import yaml" || python3 -m pip install --user pyyaml
corepack enable && pnpm install   # once per clone, from repo root
just doctor                       # what this machine is missing, and why
```

**Run `just doctor` first.** It lists every tool every gate needs, which
gate needs it, and the command that supplies it, and it exits non-zero when
the default tiers cannot run — so a missing toolchain is one line now
rather than one red gate at a time, ten minutes in.

What the default tiers (`fast` + `core`) require: Rust stable (≥ the
workspace `rust-version`), `cargo`, `git`, [`just`](https://github.com/casey/just),
`python3` with **PyYAML** (the gate runner reads the workflows), Node.js
**22+** and pnpm **11** via Corepack. What the rest of `just gate` adds: a
`rustup` toolchain at the MSRV and one nightly, the `wasm32-wasip1` target,
`cargo-deny`, `cargo-fuzz`, a network (the extension gate audits, and the
real-editor suite downloads a VS Code build on first run). `just gate api`
installs its own two pinned tools.

```bash
just gate fast        # ~20 s:   the CI/local contract + fmt, clippy, rustdoc
just gate fast core   # ~3 min:  + tests, docs examples, the extension
just gate             # tens of minutes: everything CI runs on one machine
just fmt              # cargo fmt --all
just install          # build LSP + VS Code extension and install locally
```

Those times are one measurement on one laptop with a warm `target/`; the
ratios are the durable part. `just gate` prints each gate's own elapsed
time, so your machine tells you its numbers on the first full run.

Three gates change your machine rather than only reading it, and say so:
`gate-msrv` installs the MSRV toolchain, `gate-wasm` adds the
`wasm32-wasip1` target, and `gate-api` installs its two pinned classifier
tools and the nightly toolchain `cargo public-api` needs. Everything else only reads.

## The landing gate

**Every check that can fail a pull request is a `just gate-*` recipe, and a
GitHub workflow may not run a gate any other way — it calls `just gate-<name>`.**
`just gate-contract` enforces that: a `run:` line or a third-party action in
`.github/workflows/` that is neither declared infrastructure nor a `just`
recipe fails the build, and so does a `gate-*` recipe nothing runs. So "green
locally" and "green in CI" are the same sentence by construction. The recipe is
the only place a gate command is written; adding one to CI without adding the
recipe is a build failure, by design.

```bash
just gate                       # every gate this machine can run, in order
just gate fast core             # a tier list (see scripts/gate.py TIERS)
just gate-test                  # one gate
python3 scripts/gate.py table   # which gates run in CI, which in `just gate`
```

`just gate` prints the tree it is gating and a digest of that tree's diff,
wipes this workspace's cargo fingerprints first, and then asserts that the
workspace really recompiled, that no test binary ran twice in one log, and
what each suite's totals were — the ways a green gate has turned out here to
be about nothing.

Gates that need a tool: `just gate-contract` and `just gate-docs` need
`python3` (and `gate-contract` needs PyYAML, above), `just gate-supply-chain`
needs `cargo install cargo-deny`, `just gate-fuzz` needs `cargo install
cargo-fuzz` and a nightly toolchain, `just gate-api` installs its two pinned
classifiers and nightly (for rustdoc JSON), and `just gate-ext` runs `pnpm audit` (so it needs the
network).
`just doctor` checks all of them at once.

Changes under `editors/vscode/` must keep **`just gate-ext`** green, and
**`just gate-ext-e2e`** if they touch the editor surface — that one drives a
real headless VS Code over BOTH backends, the bundled wasm server and a native
`nml-lsp`, and each launch asserts which backend it actually started.

`just gate-ext` runs the extension's own tiers in order: the toolchain policy,
the typecheck, the **unit** suite (plain mocha — a unit test may not import
`vscode`), then **`pnpm run test:real`**, which spawns REAL processes: a
provider that ignores SIGTERM, HUP and INT, one that double-forks a worker out
of its own process tree, and a stand-in editor that is SIGKILLed. It is the
only tier that can leave something running, so each case's teardown asserts
that nothing it started survived — a leaked `sleep` loop poisons every
measurement taken after it. Run it alone with
`pnpm --filter nml-lang run test:real`.

CI runs `just gate-ext` on **ubuntu, macOS and Windows**. The two launch
mechanisms share no code — a process group and a death pipe on POSIX, the
runtime's job object and `taskkill /T /F` on Windows — so a one-platform lane
tests half the design. Your machine runs the half it has: on POSIX the two
`Windows:` cases report pending, on Windows the four `POSIX:` ones do, each
with the reason printed on the line above it. Pending cases there are the
platform branch, never a disabled test.

The extension toolchain gate (`pnpm run check:toolchain`) validates VS Code API floor, manifest/lockfile alignment for `@types/vscode`, Node 22+, and the Corepack-pinned pnpm version. It runs in CI, before compile/typecheck, and in the pre-commit hook when extension or lockfile files change.

Enable the repo git hooks once per clone (Rust fmt/check + extension toolchain on relevant paths):

```bash
git config core.hooksPath hooks
```

## Landing a change

1. Every change must keep **`just gate fast core`** green, and **`just gate`**
   must be run before you ask for review. No gate is routinely red: a red
   gate is your change, or an advisory published since the last green run
   (`gate-ext` audits the dependency tree, so read its log before assuming).
2. **User-facing changes ship with their documentation — in the same PR.**
   A change is user-facing if it adds/changes syntax, a public API, a CLI
   flag, an LSP capability, or a diagnostic. Update the relevant guide
   (`docs/`), reference (`spec/`), and CHANGELOG entry. If you believe no
   docs are needed, say why in the PR description — "internal only" is a
   valid answer, silence is not.
3. Docs snippets must pass the docs verification suite (`just gate-docs`) —
   tagged ```` ```nml ```` blocks in the guides run through the real CLI in
   CI (see `scripts/docs_test.py` for the tag grammar), so keep examples
   runnable and self-contained.
4. Add a CHANGELOG line under `Unreleased` for anything a user would notice.
5. **Changing a `pub` item in a library crate?** The public API has a
   record, a stamp and a ledger, exactly as the `--json` wire does —
   `docs/api/<crate>.api.txt`, the `API_STAMP` in `nml-cli/src/out.rs`,
   and one CHANGELOG entry per stamp. `revision` + 1 for an ADDITION,
   `apiVersion` + 1 for a BREAK (an item removed or reshaped, a new field
   on a constructible struct). Move the stamp and write the entry FIRST,
   then regenerate. [`docs/stability.md`](docs/stability.md#the-public-rust-api-record)
   explains why: the crates are consumed by path and by pinned rev, so
   cargo's own version resolution guards nobody.
6. Goldens are reviewed, never regenerated blind: the diff of the golden
   IS the change — read it, and say in the PR why each line moved. Every
   generated record in the tree, and the one command that rewrites it:

   | Record | Rewrite with |
   |---|---|
   | `tests/fixtures/link-matrix/expected.txt` | `NML_UPDATE_GOLDEN=1 cargo test -p nml-cli --test link_matrix` |
   | CLI transcript goldens (`tests/integration/cli_tests.rs`) | `NML_UPDATE_GOLDEN=1 cargo test -p nml-cli` |
   | `crates/nml-core/src/layers/tests/compose.golden` | `NML_UPDATE_GOLDEN=1 cargo test -p nml-core --lib layers` |
   | `nml-cli/src/limits_table.rs` (the published bounds) | `NML_UPDATE_GOLDEN=1 cargo test -p nml-cli -- limits::tests::the_generated_table_is_fresh` |
   | `docs/json/nml-ndjson-v1.shape.txt` and the `nml limits` block in `docs/guides/validate-in-ci.md` | `NML_UPDATE_GOLDEN=1 python3 scripts/docs_test.py` |
   | `docs/api/*.api.txt` (the public-API record) | `NML_UPDATE_GOLDEN=1 python3 scripts/api_record.py` |

   `NML_UPDATE_GOLDEN=1` never *decides* anything: for the wire shape and
   the API record it rewrites the file at a stamp you already moved by
   hand, and refuses if the CHANGELOG does not name it.

   The record makes an addition VISIBLE; it cannot ask whether anyone
   needs it. Before bumping `revision` for a new public item, run
   `just api-reach` (add `--consumer ../platform` for the embedder's
   checkout): an item nothing outside its crate reaches is a candidate for
   `pub(crate)`, and one only tests reach belongs behind a `test-support`
   feature — `nml-lsp`'s public surface is two entry points and one result
   type for exactly this reason.

   It is a NAME census, so it asks the question rather than answering it:
   a hit can be a word in a comment, and a miss only says that no tree you
   pointed it at spells the name. The proof of a narrowing is the compiler,
   against every consumer — the embedder's checkout included. Run it
   without `--consumer` and an item only the embedder calls reads as
   unreached — which is how two helpers the embedder's own modules call
   were nearly made test-only here.

   The census asks the question; the COMPILER answers it. Removing a
   recorded item, or narrowing one to `pub(crate)`, is not landable on a
   census alone — run it, and then compile every consumer:
   `cargo check --workspace --all-targets` here, and
   `NML_DOWNSTREAM=<embedder checkout> just gate downstream` there. A
   census reads names; a consumer can call an item through a path no name
   rule spells (`use nml_core::diff;` then `diff::f(..)`), and a
   documented contract constant can be needed by a consumer that never
   names it at all. Both have happened.

## Language changes (RFCs)

Syntax or semantics changes go through an RFC in `docs/rfcs/` (kept
outside the tracked tree; its `README.md` is the index):

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
  runnable; terminology per `docs/glossary.md` — one
  word per concept across the CLI, the `--json` wire, the editor and the
  docs. A concept an operator reads about gets a row there, with the exact
  string the surface prints.

## License

By contributing, you agree that your contributions are dual-licensed under
MIT OR Apache-2.0, without any additional terms or conditions.
