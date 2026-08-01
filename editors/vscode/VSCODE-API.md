# VS Code API floor

The NML VS Code extension targets **VS Code and Cursor** through the same
compatibility contract: `engines.vscode` in `package.json`.

## The rule

| Field | Meaning |
|-------|---------|
| `engines.vscode` | Minimum host version the extension supports at **runtime** |
| `@types/vscode` | Maximum VS Code API surface used at **compile time** |

**`@types/vscode` must never be newer than `engines.vscode`.**

`@vscode/vsce package` enforces this when building a VSIX. Our
`npm run check:engines` script enforces the same rule earlier in CI and local
dev (see `scripts/vscodeEnginePolicy.mjs`).

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
4. Re-run `npm install` and commit `package-lock.json`.
5. Run `npm run check:engines`, `npm run typecheck`, `npm run test:unit`, `npm test`.
6. Run `npx @vscode/vsce package` (or let CI do it).
7. Note the new minimum in the changelog and `INSTALL.md`.

Dependabot is configured to **ignore** `@types/vscode` so grouped npm bumps
cannot break this contract (see `.github/dependabot.yml`).

## Automation

- `npm run check:engines` — manifest + lockfile validation
- Hooked into `typecheck`, `vscode:prepublish`, and CI before typecheck
- Unit tests: `npm run test:engine-policy`
