# Installing the NML Extension in Cursor

Complete steps to build, package, and install the NML language extension (with LSP) into Cursor.

## Prerequisites

- Rust toolchain (`cargo`)
- Node.js and npm
- `vsce` CLI: `npm install -g @vscode/vsce`
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

```bash
cd editors/vscode
npm install
npm run compile
```

This compiles `src/extension.ts` into `out/extension.js`. The compiled JS is what Cursor actually runs — if you skip this step, the extension will use stale code.

## Step 4: Package the VSIX

```bash
npx vsce package --allow-missing-repository
```

This creates `nml-lang-0.1.0.vsix` in the current directory.

## Step 5: Install in Cursor

```bash
cursor --install-extension nml-lang-0.1.0.vsix
```

## Step 6: Reload Cursor

Open the command palette (Cmd+Shift+P) and run **Developer: Reload Window**.

## Verification

1. Open any `.nml` file.
2. Open **Output** (View > Output) and select **"NML Language Server"** from the dropdown.
3. You should see `Starting NML LSP: /Users/<you>/.cargo/bin/nml-lsp`.
4. Try Cmd+Click on a name to test go-to-definition.

## Quick one-liner (after initial setup)

From the `nml` repo root:

```bash
unset CARGO_TARGET_DIR \
  && cargo build -p nml-lsp --release \
  && cp target/release/nml-lsp ~/.cargo/bin/nml-lsp \
  && cd editors/vscode \
  && npm run compile \
  && npx vsce package --allow-missing-repository \
  && cursor --install-extension nml-lang-0.1.0.vsix
```

Then reload Cursor.

## Troubleshooting

- **No "NML Language Server" in Output dropdown**: The extension didn't activate. Check Extensions view — is `NML Language Support` installed and enabled?
- **"Failed to start language server"**: The binary path is wrong. Verify with `which nml-lsp` or set `nml.server.path` in Cursor settings.
- **Changes not taking effect**: Make sure you ran `npm run compile` before `vsce package`. The VSIX bundles `out/extension.js`, not the TypeScript source.
- **Binary didn't change after rebuild**: Cursor sets `CARGO_TARGET_DIR` to a sandbox temp folder. The build succeeds but the binary goes to the wrong place. Run `unset CARGO_TARGET_DIR` before `cargo build`. Verify with `md5 target/release/nml-lsp` before and after to confirm the binary actually changed.
- **Cmd+Click not working**: Reload Cursor after installing. The LSP must be running (check Output panel).
