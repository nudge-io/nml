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

Every process-backed server (an override, the native default, a project's
`<tool> lsp`) is spawned inside a `LaunchSandbox` (`pathSecurity.ts`), minted
in one place and stamped by the one `processServer` constructor: an EMPTY
private working directory under the extension's global storage, remade on each
activation, plus an environment overlay that REMOVES the loader- and
interpreter-injection variables. `vscode-languageclient` would otherwise
default the working directory to the first workspace folder, where the fixed
`lsp` argument is a script path for any interpreter the declared tool name
resolves to; on Windows it would also be a directory the default DLL search
order reads. The environment must be removed rather than omitted:
`getEnvironment` in `vscode-languageclient/lib/node/main.js` copies all of
`process.env` and overlays `options.env`, and Node's `spawn` drops only keys
whose value is `undefined`.

A project-declared `<tool> lsp` additionally carries a `ServerIdentityContract`
(`serverAcquisition.ts`): once the client is running, `initializeResult
.serverInfo.name` must be `nml-lsp`. A DIFFERENT name is a definite negative —
the process is stopped, the stored approval withdrawn, and the operator told.
A MISSING name, or no answer within `INITIALIZE_BUDGET_MS`
(`serverSession.ts`), stands the provider down for the session with the
approval untouched: neither is evidence against what the operator approved.
The neutral server is never held to this — an operator who set
`nml.server.path` chose that binary themselves. The trust algebra and
every word the operator reads are in `providerTrust.ts`, which imports no
`vscode` API so both are unit-tested.

## Raising the API floor (intentional upgrade)

Do this in **one PR**, never via Dependabot alone:

1. Decide the new minimum host version (check release notes for APIs you need).
2. Bump `engines.vscode` (e.g. `^1.125.0`).
3. Bump `@types/vscode` to the **same** version (exact pin).
4. Re-run `pnpm install` from the repo root and commit `pnpm-lock.yaml`.
5. Run `just gate-ext` and `just gate-ext-e2e` (the latter drives a real VS Code over both backends).
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

**CI packaging note:** `vsce` (pinned 3.9.2) has no `--no-prepublish` flag, so CI runs
`vscode:prepublish` during verify (bundle), E2E (`pretest`), and `package`. A
future `package:ci` shortcut depends on upstream vsce support.
