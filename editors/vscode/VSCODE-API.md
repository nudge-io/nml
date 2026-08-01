# VS Code API floor

The NML VS Code extension targets **VS Code and Cursor** through the same
compatibility contract: `engines.vscode` in `package.json`.

## The rule

| Field | Meaning |
|-------|---------|
| `engines.vscode` | Minimum host version the extension supports at **runtime** |
| `@types/vscode` | Maximum VS Code API surface used at **compile time** |

**`@types/vscode` must exactly match the semver floor of `engines.vscode`** (and
must never be newer). With `engines.vscode: ^1.91.0` and `@types/vscode: 1.91.0`,
both declare the same API floor — there is no intentional lag.

`@vscode/vsce package` enforces this when building a VSIX. Our
`pnpm run check:toolchain` script enforces the same rule earlier in CI and local
dev (see `scripts/vscodeEnginePolicy.mjs` and `scripts/lockfilePolicy.mjs`).

`@types/vscode` is a devDependency only — it does not ship in the VSIX. Cursor
and VS Code both read `engines.vscode`; there is no separate Cursor engines
field.

## Current floor

- `engines.vscode`: `^1.91.0`
- `@types/vscode`: `1.91.0` (exact pin)

This floor matches `vscode-languageclient` v10, structured `LogOutputChannel`,
and the bundled WASM neutral server.

`nml.server.path` must be an **absolute** path outside any open workspace
folder. Relative paths are refused (cwd-dependent spawn is unpredictable).
Rejected overrides fall through to bundled WASM, then `~/.cargo/bin/nml-lsp`.

## Raising the API floor (intentional upgrade)

Do this in **one PR**, never via Dependabot alone:

1. Decide the new minimum host version (check release notes for APIs you need).
2. Bump `engines.vscode` (e.g. `^1.125.0`).
3. Bump `@types/vscode` to the **same** version (exact pin).
4. Re-run `pnpm install` from the repo root and commit `pnpm-lock.yaml`.
5. Run `pnpm run check:toolchain`, `pnpm run typecheck`, `pnpm run test:unit`, `pnpm test` (from `editors/vscode/`).
6. Run `pnpm run package` (or let CI do it).
7. Note the new minimum in the changelog and `INSTALL.md`.

Dependabot is configured to **ignore** `@types/vscode` so grouped pnpm bumps
cannot break this contract (see `.github/dependabot.yml`).

**Dependabot + `minimumReleaseAgeStrict`:** if a Dependabot PR fails install
because a dependency was published less than 24 hours ago, re-run the workflow
after the release ages in.

**`onlyBuiltDependencies`:** mirrors `allowBuilds` — only `esbuild`,
`@vscode/vsce-sign`, and `keytar` may run install scripts (see `pnpm-workspace.yaml`).

**`trustPolicy`:** intentionally not set to `no-downgrade` — it blocks
`mocha`'s `chokidar` transitive (provenance downgrade). See the comment in
`pnpm-workspace.yaml`.

## Automation

- `pnpm run check:toolchain` — manifest + root `pnpm-lock.yaml` + Node/pnpm toolchain validation
- Runs at the start of `verify` / `verify:ci`, in `vscode:prepublish`, on extension
  path changes in the pre-commit hook, and in `just lint-ext` / `just compile-ext`
- Unit tests: `pnpm run test:engine-policy`, `pnpm run test:lockfile-policy`, `pnpm run test:toolchain-policy`

**CI packaging note:** `vsce` 3.3.0 has no `--no-prepublish` flag, so CI runs
`vscode:prepublish` during verify (bundle), E2E (`pretest`), and `package`. A
future `package:ci` shortcut depends on upstream vsce support.
