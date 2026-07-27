# NML development tasks

# Build the LSP binary in release mode
build-lsp:
    unset CARGO_TARGET_DIR && cargo build -p nml-lsp --release

# Build the LSP binary in debug mode
build-lsp-debug:
    unset CARGO_TARGET_DIR && cargo build -p nml-lsp

# Build the LSP as a WASM module — the neutral server the VS Code extension
# bundles (`vscode:prepublish` copies it into the VSIX). Without this the VSIX
# would ship a stale wasm, or `bundle:wasm` would fail on a fresh checkout.
build-lsp-wasm:
    rustup target add wasm32-wasip1
    unset CARGO_TARGET_DIR && cargo build -p nml-lsp --target wasm32-wasip1 --release

# Copy the built LSP binary to ~/.cargo/bin
install-bin: build-lsp
    cp target/release/nml-lsp ~/.cargo/bin/nml-lsp

# Compile the VS Code extension TypeScript
compile-ext:
    cd editors/vscode && npm install && npm run compile

# Package the extension as a VSIX (fresh WASM built first; old VSIXes cleared so
# exactly one remains for install-ext to pick up regardless of version).
package-ext: compile-ext build-lsp-wasm
    cd editors/vscode && rm -f *.vsix && npx vsce package --allow-missing-repository

# Install the VSIX into Cursor (globs the single freshly-built VSIX, so a
# version bump never breaks this).
install-ext: package-ext
    cursor --install-extension editors/vscode/*.vsix

# Full rebuild and reinstall: LSP binary + extension + install into Cursor
install: install-bin install-ext
    @echo "Done. Reload Cursor (Cmd+Shift+P → Developer: Reload Window)"

# Run all workspace tests (matches CI: --locked against the committed lock)
test:
    cargo test --workspace --locked

# Run only the LSP tests
test-lsp:
    cargo test -p nml-lsp

# Run clippy on the workspace (matches CI: --all-targets + --locked)
lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

# Format all Rust code
fmt:
    cargo fmt --all

# Check formatting without modifying
fmt-check:
    cargo fmt --all -- --check

# Verify the tagged ```nml blocks in the Markdown docs against the real CLI
# (see scripts/docs_test.py for the tag grammar). Matches the CI docs job.
docs-test:
    unset CARGO_TARGET_DIR && cargo build -p nml-cli --locked
    unset CARGO_TARGET_DIR && cargo build -p nml-cookbook --examples --tests --locked
    python3 scripts/docs_test.py

# The declared floor toolchain (read from Cargo.toml, the single source of
# truth) builds every target, the wasm server, and the doctests, against
# the committed lock — see docs/stability.md.
# Verify the MSRV contract locally (matches the CI msrv job)
msrv:
    #!/usr/bin/env bash
    set -euo pipefail
    version="$(grep -m1 '^rust-version' Cargo.toml | cut -d '"' -f 2)"
    [ -n "$version" ] || { echo "no rust-version in Cargo.toml" >&2; exit 1; }
    echo "MSRV from Cargo.toml: $version"
    rustup toolchain install "$version" --profile minimal --target wasm32-wasip1
    unset CARGO_TARGET_DIR
    RUSTUP_TOOLCHAIN="$version" cargo check --workspace --all-targets --locked
    RUSTUP_TOOLCHAIN="$version" cargo check -p nml-lsp --target wasm32-wasip1 --locked
    RUSTUP_TOOLCHAIN="$version" cargo test --doc --workspace --locked

# Fuzz one target (nightly; `cargo install cargo-fuzz` first), seeding it
# with the tracked landmarks in fuzz/seeds/<target>/.
#
# Two corpora, deliberately: libFuzzer writes new finds to the FIRST
# directory and treats the rest as read-only inputs. `fuzz/corpus/` is
# machine-generated and gitignored — it is 14k files and tens of MB, which
# is exactly why it must not be committed. `fuzz/seeds/` is hand-written,
# tiny, and tracked: each file is a grammar landmark (a separator spelling,
# a 34-digit coefficient, the smallest magnitude) that a fresh clone should
# explore in its first seconds instead of rediscovering by mutation.
# Passing both is what makes the seeds do anything, so it lives here rather
# than in someone's shell history.
#
# **If a run finds a crash**, cargo-fuzz writes the reproducer to
# `fuzz/artifacts/<target>/` — which is gitignored, so it disappears on the
# next clean and can never fail again. Copy it into `fuzz/seeds/<target>/`
# with a name that says what it broke. That is what turns a one-time find
# into a permanent regression: every future run replays it in its first
# seconds, on every machine.
fuzz target='number' time='60':
    #!/usr/bin/env bash
    set -euo pipefail
    # Both directories must exist before libFuzzer opens them: the corpus
    # is gitignored (absent on a fresh clone) and git cannot track an empty
    # seed directory, so a target with no landmarks yet would otherwise
    # fail to start rather than simply fuzz unseeded.
    mkdir -p fuzz/corpus/{{target}} fuzz/seeds/{{target}}
    cargo +nightly fuzz run {{target}} \
        fuzz/corpus/{{target}} fuzz/seeds/{{target}} \
        -- -max_total_time={{time}}

# Clean build artifacts
clean:
    cargo clean
