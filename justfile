# NML development tasks

# Build the LSP binary in release mode
build-lsp:
    unset CARGO_TARGET_DIR && cargo build -p nml-lsp --release

# Build the LSP binary in debug mode
build-lsp-debug:
    unset CARGO_TARGET_DIR && cargo build -p nml-lsp

# Copy the built LSP binary to ~/.cargo/bin
install-bin: build-lsp
    cp target/release/nml-lsp ~/.cargo/bin/nml-lsp

# Compile the VS Code extension TypeScript
compile-ext:
    cd editors/vscode && npm install && npm run compile

# Package the extension as a VSIX
package-ext: compile-ext
    cd editors/vscode && npx vsce package --allow-missing-repository

# Install the VSIX into Cursor
install-ext: package-ext
    cursor --install-extension editors/vscode/nml-lang-0.1.0.vsix

# Full rebuild and reinstall: LSP binary + extension + install into Cursor
install: install-bin install-ext
    @echo "Done. Reload Cursor (Cmd+Shift+P → Developer: Reload Window)"

# Run all workspace tests
test:
    cargo test --workspace

# Run only the LSP tests
test-lsp:
    cargo test -p nml-lsp

# Run clippy on the workspace
lint:
    cargo clippy --workspace -- -D warnings

# Format all Rust code
fmt:
    cargo fmt --all

# Check formatting without modifying
fmt-check:
    cargo fmt --all -- --check

# Clean build artifacts
clean:
    cargo clean
