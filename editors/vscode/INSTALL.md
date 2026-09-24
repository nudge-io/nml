# Building and installing the extension from source (VS Code and Cursor)

Steps to build, package and install the NML extension from this checkout. A packaged VSIX already carries the language server as WebAssembly, so Steps 1-2 — a *native* server — are optional.

**Host compatibility:** the extension requires VS Code API **1.91+** (`engines.vscode` in `package.json`). Current Cursor releases satisfy this — the same VSIX installs in both VS Code and Cursor.

## Prerequisites

- Rust toolchain (`cargo`)
- Node.js **22+** and pnpm **11** (`corepack enable` from the repo root)
- Cursor IDE

## Step 1: Build the LSP binary

**Important:** Cursor's integrated terminal sets `CARGO_TARGET_DIR` to a sandbox temp directory. This means `cargo build` silently writes the binary to a temp folder while `target/release/` retains the old binary. You **must** unset it before building:

```bash
cd nml
unset CARGO_TARGET_DIR
cargo build -p nml-lsp --release
```

## Step 2: Install the binary to PATH

```bash
cp target/release/nml-lsp ~/.cargo/bin/nml-lsp
```

The extension resolves its server in three steps: `nml.server.path` if you set it, then the WASM backend
bundled in the VSIX, then `~/.cargo/bin/nml-lsp`. So a packaged VSIX already carries a server — Steps 1-2 are
for running a *native* one, which you select by setting `nml.server.path` (an absolute path; a leading `~/`
expands) in Cursor settings after installation.

## Step 3: Compile the extension TypeScript

From the **repo root**:

```bash
corepack enable
pnpm install
just compile-ext
```

This type-checks and compiles TypeScript into `out/` (`tsc -b`). The bundle the VSIX ships,
`dist/extension.js`, is written by the packaging step below (`vscode:prepublish`), not here.

## Step 4: Package the VSIX

```bash
just package-ext
```

Or from `editors/vscode/` after WASM is built: `pnpm run package`.

## Step 5: Install the VSIX

```bash
code --install-extension editors/vscode/*.vsix     # VS Code
cursor --install-extension editors/vscode/*.vsix   # Cursor
```

## Step 6: Reload Cursor

Open the command palette (Cmd+Shift+P) and run **Developer: Reload Window**.

## Verification

1. Open any `.nml` file.
2. Open **Output** (View > Output) and select **"NML Language Server"** from the dropdown.
3. You should see the neutral server starting (WASM or native path).
4. Try Cmd+Click on a name to test go-to-definition.

## Quick one-liner (after initial setup)

From the `nml` repo root:

```bash
just install
```

Then reload Cursor.

## Troubleshooting

- **No "NML Language Server" in Output dropdown**: The extension didn't activate. Check Extensions view — is `NML Language Support` installed and enabled?
- **"failed to start the NML language server"**: the toast names the remedy for the server it tried. Bundled WebAssembly server: check that the `ms-vscode.wasm-wasi-core` extension is installed and enabled. `nml.server.path`: check the path (`which nml-lsp`), or clear the setting to use the bundled server. **NML: Show Language Server Log** has the cause.
- **Changes not taking effect**: Run `just compile-ext` (or `just package-ext`) before installing the VSIX. The VSIX bundles `dist/extension.js`, not the TypeScript source.
- **Binary didn't change after rebuild**: Cursor sets `CARGO_TARGET_DIR` to a sandbox temp folder. Run `unset CARGO_TARGET_DIR` before `cargo build`. Verify with `md5 target/release/nml-lsp` before and after.
- **Cmd+Click not working**: Reload Cursor after installing. The LSP must be running (check Output panel).
