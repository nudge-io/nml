# Installing the NML Extension in Cursor

Complete steps to build, package, and install the NML language extension (with LSP) into Cursor.

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

The extension defaults to `~/.cargo/bin/nml-lsp`. If you want a different location, set `nml.server.path` in Cursor settings after installation.

## Step 3: Compile the extension TypeScript

From the **repo root**:

```bash
corepack enable
pnpm install
just compile-ext
```

This compiles TypeScript into `out/` and bundles `dist/extension.js`. The VSIX ships the bundle — if you skip this step, the extension will use stale code.

## Step 4: Package the VSIX

```bash
just package-ext
```

Or from `editors/vscode/` after WASM is built: `pnpm run package`.

## Step 5: Install in Cursor

```bash
cursor --install-extension editors/vscode/*.vsix
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
- **"Failed to start language server"**: The binary path is wrong. Verify with `which nml-lsp` or set `nml.server.path` in Cursor settings.
- **Changes not taking effect**: Run `just compile-ext` (or `just package-ext`) before installing the VSIX. The VSIX bundles `dist/extension.js`, not the TypeScript source.
- **Binary didn't change after rebuild**: Cursor sets `CARGO_TARGET_DIR` to a sandbox temp folder. Run `unset CARGO_TARGET_DIR` before `cargo build`. Verify with `md5 target/release/nml-lsp` before and after.
- **Cmd+Click not working**: Reload Cursor after installing. The LSP must be running (check Output panel).
