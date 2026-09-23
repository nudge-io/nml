# NML development tasks
#
# ── THE LANDING GATE ────────────────────────────────────────────────────────
# Every check that can fail a pull request is a `gate-*` recipe BELOW, and a
# GitHub workflow may not run a gate command any other way: it calls
# `just gate-<name>`. `just gate-contract` (scripts/gate.py) enforces that —
# a `run:` line in .github/workflows/ that is neither recognised infrastructure
# nor a `just` recipe fails the build, and so does a `gate-*` recipe no
# workflow and no tier runs. So "it passed locally" and "it passed in CI" are
# the same sentence by construction, not by anyone keeping two lists in sync.
#
#     just gate                 # every gate this machine can run, in order
#     just gate fast            # seconds:  contract + fmt/clippy/rustdoc
#     just gate fast core       # minutes:  + tests, docs examples, extension
#     just gate-contract        # just the CI/local divergence ratchet
#     python3 scripts/gate.py table    # which gates run where
#
# `just gate` prints the tree it is gating and a digest of that tree's diff,
# wipes this workspace's cargo fingerprints first, and asserts that the
# workspace really recompiled and that no test binary ran twice in one log —
# the four ways a green gate has turned out to be about nothing here.
# ────────────────────────────────────────────────────────────────────────────

# Every gate this machine can run (see scripts/gate.py TIERS for the lists).
gate *tiers:
    python3 -u scripts/gate.py run {{tiers}}

# What this machine is missing, BEFORE a gate spends ten minutes finding out.
#
# `just gate` runs the tiers in order and reports a missing tool exactly as it
# reports a broken build — RED, a log path, and nothing that says "install
# this". A contributor on a fresh machine then discovers the toolchain one red
# gate at a time, and the tier that needs the most tools is the one that runs
# last. This recipe is the preflight: every requirement of every gate, what
# needs it, and the one command that supplies it. It runs no gate and changes
# nothing.
#
# Exit 1 only when the DEFAULT tiers (fast + core) cannot run: those are what
# a pull request must be green on. The rest is reported and not enforced.
doctor:
    #!/usr/bin/env bash
    # No `set -e`: the point is to report EVERY gap in one pass.
    set -uo pipefail
    fail=0
    row() { printf '%-4s  %-24s  %-33s  %s\n' "$1" "$2" "$3" "$4"; }
    have() { command -v "$1" >/dev/null 2>&1; }
    # $1 ok?  $2 what  $3 gates  $4 remedy  $5 required-for-default | "auto"
    # "auto" = the gate installs it itself, so absence is a longer first run,
    # never a failure: reported, but neither MISSING nor counted.
    check() {
        if [ "$1" = yes ]; then row "ok" "$2" "$3" ""
        elif [ "${5:-no}" = auto ]; then row "auto" "$2" "$3" "$4"
        else
            row "MISS" "$2" "$3" "$4"
            [ "${5:-no}" = yes ] && fail=1
        fi
        return 0
    }
    printf '%-4s  %-24s  %-33s  %s\n' "" "requirement" "needed by" "how to get it"
    printf '%-4s  %-24s  %-33s  %s\n' "----" "------------------------" "---------------------------------" "-------------"

    have just    && j=yes || j=no
    check "$j" "just" "every gate" "cargo install just" yes
    have cargo   && c=yes || c=no
    check "$c" "cargo / rustc" "every Rust gate" "https://rustup.rs" yes
    have rustup  && r=yes || r=no
    check "$r" "rustup" "gate-msrv, gate-wasm" "https://rustup.rs" no
    have python3 && py=yes || py=no
    check "$py" "python3" "the gate runner, gate-docs" "https://python.org" yes
    if [ "$py" = yes ] && python3 -c 'import yaml' >/dev/null 2>&1; then y=yes; else y=no; fi
    check "$y" "PyYAML" "gate-contract (the runner)" "python3 -m pip install pyyaml" yes
    have node    && nd=yes || nd=no
    check "$nd" "node >= 22" "gate-ext, gate-ext-e2e" "https://nodejs.org (22 or newer)" yes
    have pnpm    && pn=yes || pn=no
    check "$pn" "pnpm 11" "gate-ext, gate-ext-e2e" "corepack enable && pnpm install" yes
    have git     && g=yes || g=no
    check "$g" "git" "the gate runner, gate-api" "https://git-scm.com" yes

    msrv="$(grep -m1 '^rust-version' Cargo.toml 2>/dev/null | cut -d '"' -f 2)"
    if [ "$r" = yes ] && [ -n "${msrv:-}" ] && rustup toolchain list 2>/dev/null | grep -q "^${msrv}"; then m=yes; else m=no; fi
    check "$m" "toolchain ${msrv:-MSRV}" "gate-msrv" "rustup toolchain install ${msrv:-\$MSRV} --profile minimal --target wasm32-wasip1" no
    if [ "$r" = yes ] && rustup target list --installed 2>/dev/null | grep -q '^wasm32-wasip1$'; then w=yes; else w=no; fi
    check "$w" "wasm32-wasip1 target" "gate-wasm, gate-ext-e2e" "rustup target add wasm32-wasip1" no
    if [ "$r" = yes ] && rustup toolchain list 2>/dev/null | grep -q '^nightly'; then ng=yes; else ng=no; fi
    check "$ng" "nightly toolchain" "gate-fuzz, gate-minimal-versions, gate-api" "rustup toolchain install nightly" no
    have cargo-deny && cd_=yes || cd_=no
    check "$cd_" "cargo-deny" "gate-supply-chain" "cargo install cargo-deny" no
    have cargo-fuzz && cf=yes || cf=no
    check "$cf" "cargo-fuzz" "gate-fuzz" "cargo install cargo-fuzz" no
    have cargo-public-api && cp_=yes || cp_=no
    check "$cp_" "cargo-public-api 0.52" "gate-api" "gate-api installs it (first run is slow)" auto
    have cargo-semver-checks && cs=yes || cs=no
    check "$cs" "cargo-semver-checks 0.50" "gate-api" "gate-api installs it (first run is slow)" auto

    echo
    echo "Not checkable here, but needed anyway:"
    echo "  * network        gate-ext runs \`pnpm audit\`; gate-ext-e2e downloads a VS Code build on first run"
    echo "  * NML_DOWNSTREAM gate-downstream needs a checkout of the consuming workspace"
    echo
    echo "Roughly, on one machine:  gate fast ~20s, gate fast core ~3min, gate ~tens of minutes."
    if [ "$fail" -ne 0 ]; then
        echo
        echo "doctor: the default tiers (fast + core) cannot run on this machine yet." >&2
        exit 1
    fi
    echo
    echo "doctor: this machine can run \`just gate fast core\`."

# ── gates ───────────────────────────────────────────────────────────────────

# The CI/local divergence ratchet, plus its own self-test (a ratchet that has
# never been seen RED is a comment).
gate-contract:
    python3 scripts/gate.py check
    python3 scripts/gate.py self-test

# rust-ci.yml `lint`: formatting, lints, and rustdoc as a build product.
gate-lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

# rust-ci.yml `test`: the default lane, against the committed lock.
gate-test:
    unset CARGO_TARGET_DIR && cargo test --workspace --locked

# rust-ci.yml `test`, the four `#[ignore]`d perf tiers. Release-timed with
# 30-200x headroom; run alone (`--test-threads=1`) because their bounds are
# wall-clock and a loaded host starves them.
gate-perf:
    unset CARGO_TARGET_DIR && cargo test -p nml-core --release --lib --locked -- --ignored perf_ --test-threads=1
    unset CARGO_TARGET_DIR && cargo test -p nml-validate --release --lib --locked -- --ignored perf_ --test-threads=1
    unset CARGO_TARGET_DIR && cargo test -p nml-validate --release --test beneath_race --locked -- --ignored perf_ --test-threads=1
    unset CARGO_TARGET_DIR && cargo test -p nml-cli --release --test cli_tests --locked -- --ignored perf_ --test-threads=1

# rust-ci.yml `supply-chain` (deny.toml): permissive-only licenses, RustSec
# advisories, crates.io-only sources. `cargo install cargo-deny` first.
gate-supply-chain:
    cargo deny check licenses advisories sources

# The MSRV contract (docs/stability.md), read from Cargo.toml so the manifest
# stays the single source of truth. Matches rust-ci.yml `msrv`.
gate-msrv:
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

# The wasm32-wasip1 arm, which is the DEFAULT editor backend and therefore
# shipped code: it must type-check, lint, and build in release — the artifact
# the VSIX carries. `--locked` because that artifact is released.
#
# `cargo clippy --target wasm32-wasip1` is here and NOT in any CI job today:
# the wasm arm is cfg-gated code (the synchronous pump, wasi_fs) that the
# native lanes never compile, so a lint regression in it reaches users
# through the bundled server without ever being seen.
gate-wasm:
    rustup target add wasm32-wasip1
    unset CARGO_TARGET_DIR && cargo check -p nml-lsp --target wasm32-wasip1 --locked
    unset CARGO_TARGET_DIR && cargo clippy -p nml-lsp --target wasm32-wasip1 --locked -- -D warnings
    unset CARGO_TARGET_DIR && cargo build -p nml-lsp --target wasm32-wasip1 --release --locked

# rust-ci.yml `package`: publish-readiness, continuously. The tutorial apps
# and the cookbook are not publish surface.
gate-package:
    #!/usr/bin/env bash
    set -euo pipefail
    # `cargo package` refuses a dirty working tree, and a developer's tree is
    # always dirty — so as CI spells it this gate is one nobody can run
    # locally, which is the disease this file exists to cure. On a dirty tree
    # it runs with `--allow-dirty` and SAYS the result is about the working
    # tree, not about what would be published.
    extra=()
    if [ -n "$(git status --porcelain 2>/dev/null)" ]; then
        echo "gate-package: the working tree is dirty — packaging it with --allow-dirty."
        echo "              CI packages a CLEAN checkout; this run proves the manifests and"
        echo "              file sets, not the exact tarball a release would upload."
        extra=(--allow-dirty)
    fi
    unset CARGO_TARGET_DIR
    cargo package --workspace --exclude 'nml-tutorial-*' --exclude nml-cookbook --locked "${extra[@]}"

# rust-ci.yml `minimal-versions`: resolve every DIRECT dependency to its
# declared minimum and prove the workspace still compiles. Deliberately
# unlocked — a different resolution is the point — so it REWRITES Cargo.lock;
# the recipe restores it, which the CI job does not have to care about.
gate-minimal-versions:
    #!/usr/bin/env bash
    set -euo pipefail
    # Restore from a COPY, never `git checkout` — concurrent work in this
    # tree is uncommitted, and a gate must not be able to discard it.
    mkdir -p target
    cp Cargo.lock target/Cargo.lock.gate-backup
    trap 'cp target/Cargo.lock.gate-backup Cargo.lock' EXIT
    cargo +nightly update -Zdirect-minimal-versions
    unset CARGO_TARGET_DIR
    cargo check --workspace --all-targets

# rust-ci.yml `fuzz-smoke`: EVERY target in fuzz/fuzz_targets/, enumerated
# from the directory — a hand-kept list silently omitted two targets for a
# whole certification arc. Bounded; deep fuzzing is `just fuzz <target>`.
gate-fuzz:
    #!/usr/bin/env bash
    set -euo pipefail
    cd fuzz
    for file in fuzz_targets/*.rs; do
        target="$(basename "$file" .rs)"
        mkdir -p "corpus/$target" "seeds/$target"
        cargo +nightly fuzz run "$target" "corpus/$target" "seeds/$target" ../tests/fixtures/valid \
            -- -runs=20000 -max_total_time=60 -dict=nml.dict
    done

# ci.yml `docs`: the tagged ```nml blocks in the Markdown docs run through the
# real CLI, so guide snippets cannot rot (scripts/docs_test.py).
gate-docs:
    unset CARGO_TARGET_DIR && cargo build -p nml-cli --locked
    unset CARGO_TARGET_DIR && cargo build -p nml-cookbook --examples --tests --locked
    python3 scripts/docs_test.py

# The extension's dependencies, exactly as CI installs them.
gate-ext-deps:
    corepack enable || true
    pnpm install --frozen-lockfile

# extension-build.yml `verify:ci`: toolchain policy, typecheck, unit tests,
# JS bundle, and `pnpm audit --audit-level=high` (needs the network).
gate-ext: gate-ext-deps
    pnpm --filter nml-lang run verify:ci

# extension-build.yml's real-editor suite, over BOTH backends.
#
# The wasm half needs the release wasm bundled; the native half needs a
# release `nml-lsp` on disk and NML_TEST_NATIVE=1, which makes the native
# lane REQUIRED rather than skipped-if-absent (.vscode-test.mjs throws if the
# binary is missing). Each launch gets its own short-path, empty user-data
# directory, and asserts which backend it actually started.
gate-ext-e2e: gate-ext-deps gate-wasm
    unset CARGO_TARGET_DIR && cargo build -p nml-lsp --release --locked
    cd editors/vscode && NML_TEST_NATIVE=1 pnpm test

# The VSIX, as extension-build.yml packages it.
gate-ext-package: gate-ext-deps
    pnpm --filter nml-lang run package

# The DOWNSTREAM compile: this workspace's crates are a library, and the
# consumer that finds out first when a type changes shape is `platform`
# (nudge). An `Arm`-by-type change once produced a THIRD
# downstream break nothing announced — because nothing in this repository
# ever compiled the consumer.
#
# Opt-in by pointing NML_DOWNSTREAM at a checkout, and LOUD when it is not
# set: a gate that skips itself when its input is missing is the vacuity
# this file exists to stop. It is in its own tier (`just gate downstream`)
# rather than the default list, because most machines do not have the
# consumer checked out and a gate must not be routinely red.
gate-downstream:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "${NML_DOWNSTREAM:-}" ]; then
        echo "error: set NML_DOWNSTREAM to a checkout of the consuming workspace" >&2
        echo "  e.g. NML_DOWNSTREAM=../nudge/platform just gate downstream" >&2
        echo "  (not skipped when unset on purpose: a gate that skips itself is not a gate)" >&2
        exit 1
    fi
    [ -f "$NML_DOWNSTREAM/Cargo.toml" ] || {
        echo "error: $NML_DOWNSTREAM has no Cargo.toml" >&2; exit 1; }
    # `--config paths=[…]` redirects the consumer's `nml-*` dependencies at
    # THIS tree without editing its manifests: the check is "does the
    # consumer still compile against what is in front of me", and it must not
    # leave the consumer's checkout modified.
    paths="[\"$PWD/crates/nml-core\",\"$PWD/crates/nml-validate\",\"$PWD/crates/nml-lsp\",\"$PWD/crates/nml-fmt\"]"
    echo "downstream: $NML_DOWNSTREAM against $PWD"
    cd "$NML_DOWNSTREAM"
    cargo check --workspace --all-targets --config "paths=$paths"

# ── developer loop (not gates: nothing here is a landing requirement) ───────
#
# Gates mostly only READ this machine; the three that do not say so on their
# own recipe (`gate-msrv` installs a toolchain, `gate-wasm` adds a target,
# `gate-api` installs two pinned tools). What separates this section is not
# that it writes — it is that no pull request is held to it.
# `just doctor` reports what every gate needs without running any of them.

# Build the LSP binary in release mode
build-lsp:
    unset CARGO_TARGET_DIR && cargo build -p nml-lsp --release

# Build the LSP binary in debug mode
build-lsp-debug:
    unset CARGO_TARGET_DIR && cargo build -p nml-lsp

# Copy the built LSP binary to ~/.cargo/bin
install-bin: build-lsp
    cp target/release/nml-lsp ~/.cargo/bin/nml-lsp

install-ext-deps:
    corepack enable || true
    pnpm install

compile-ext: install-ext-deps
    pnpm --filter nml-lang run check:toolchain
    pnpm --filter nml-lang run compile

package-ext: compile-ext gate-wasm
    cd editors/vscode && rm -f *.vsix && pnpm run package

# Install the VSIX into Cursor (globs the single freshly-built VSIX, so a
# version bump never breaks this).
install-ext: package-ext
    cursor --install-extension editors/vscode/*.vsix

# Full rebuild and reinstall: LSP binary + extension + install into Cursor
install: install-bin install-ext
    @echo "Done. Reload Cursor (Cmd+Shift+P → Developer: Reload Window)"

# Run only the LSP tests
test-lsp:
    cargo test -p nml-lsp

# Extension typecheck alone (the inner loop; `just gate-ext` is the gate).
lint-ext: install-ext-deps
    pnpm --filter nml-lang run check:toolchain
    pnpm --filter nml-lang run typecheck

# Format all Rust code
fmt:
    cargo fmt --all

# Check formatting without modifying (the pre-commit hook's step).
fmt-check:
    cargo fmt --all -- --check

# rust-ci.yml `api`. The PUBLIC API contract — the library half of the
# `--json` wire's. Regenerates nothing: it CHECKS the committed records,
# then asks the classifier whether what moved is breaking. Both tools are
# pinned WITH the toolchain (rustdoc's JSON rendering moves with the
# compiler; bump all three in one reviewed change) and installed here, so
# CI and a local run install the same versions. `cargo public-api` builds
# rustdoc JSON with a nightly toolchain (not the active one — the pin file
# still governs fmt/clippy/check); this recipe installs a DATED nightly with
# rustdoc (rolling `nightly` sometimes ships without rustdoc for a day).
# `NML_UPDATE_GOLDEN=1 just
# gate-api` rewrites the records AFTER the stamp and the CHANGELOG entry
# moved — never before. The baseline is `NML_API_BASELINE`, else
# `origin/main` where that ref exists, else `HEAD` (a clone with no
# remote still gets a verdict about its own uncommitted work).
gate-api:
    #!/usr/bin/env bash
    set -euo pipefail
    # cargo-public-api 0.52 floor (crates.io README compatibility matrix).
    # Bump with the tool + records in one reviewed change — not rolling `nightly`,
    # which can lack rustdoc on any given day (rustup-components-history).
    rustdoc_nightly="${NML_RUSTDOC_NIGHTLY:-nightly-2025-08-02}"
    echo "gate-api: installing ${rustdoc_nightly} (default profile — rustdoc JSON) for cargo public-api"
    # Do not use `--profile minimal --component rustdoc`: on many dated nightlies
    # rustdoc is not a separate downloadable component for that channel (CI fails
    # with "component rustdoc … is unavailable"), while the default profile still
    # ships rustdoc as part of its component set.
    rustup toolchain install "$rustdoc_nightly"
    rustup run "$rustdoc_nightly" rustdoc --version
    # Build the plugins with the workspace pin (rust-toolchain.toml). cargo-semver-checks
    # 0.50 needs rustc >= 1.93; the rustdoc nightly is 1.90 and is only for JSON at run
    # time (cargo-public-api's matrix names which nightly *outputs* it reads, not which
    # compiler must build the `cargo install` artifacts). Installing under the nightly
    # fails CI with "Failed to install cargo-semver-checks" after public-api succeeds.
    cargo install --locked cargo-public-api@0.52.0 cargo-semver-checks@0.50.0
    export RUSTUP_TOOLCHAIN="$rustdoc_nightly"
    python3 scripts/api_record.py
    base="${NML_API_BASELINE:-}"
    if [ -z "$base" ]; then
        if git rev-parse --verify --quiet origin/main >/dev/null; then base=origin/main; else base=HEAD; fi
    fi
    echo "gate-api: classifying against $base"
    # The record cannot see a new PRIVATE field on a public struct
    # (measured on `ast::Arm`): the classifier can, and names the lint.
    cargo semver-checks check-release --workspace --baseline-rev "$base"

# Fuzz ONE target for longer than the gate does (nightly; `cargo install
# cargo-fuzz` first), seeding it with the tracked landmarks in
# fuzz/seeds/<target>/. `just gate-fuzz` is the bounded sweep over ALL of
# them; this is the deep dive on one.
#
# Two corpora, deliberately: libFuzzer writes new finds to the FIRST
# directory and treats the rest as read-only inputs. `fuzz/corpus/` is
# machine-generated and gitignored — it is 14k files and tens of MB, which
# is exactly why it must not be committed. `fuzz/seeds/` is hand-written,
# tiny, and tracked: each file is a grammar landmark (a separator spelling,
# a 34-digit coefficient, the smallest magnitude) that a fresh clone should
# explore in its first seconds instead of rediscovering by mutation.
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
    mkdir -p fuzz/corpus/{{target}} fuzz/seeds/{{target}}
    cargo +nightly fuzz run {{target}} \
        fuzz/corpus/{{target}} fuzz/seeds/{{target}} \
        -- -max_total_time={{time}} -dict=fuzz/nml.dict

# The RFC 0025 timing cross-check: hyperfine over `nml check` on the five
# generated composition stacks (tests/fixtures/layers/perf/). NOT a gate —
# absolutes drift across machines, so the precise form is a SAME-SESSION
# two-binary comparison: run it once before and once after an engine change
# and compare. The portable gate is `just gate-perf`.
perf-layers:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v hyperfine >/dev/null || {
        echo "error: hyperfine is not installed (brew install hyperfine)" >&2
        echo "the portable gate: just gate-perf" >&2
        exit 1
    }
    cargo build --release -p nml-cli
    for f in tests/fixtures/layers/perf/*.nml; do
        hyperfine --warmup 3 --min-runs 20 "target/release/nml check $f"
    done

# The RFC 0025 composition golden (Phase 5, as amended by RFC 0019 item 0)
# runs on every `cargo test`: every layer-battery composition and
# every layer fixture, composed through `compose_file`, is checked against
# crates/nml-core/src/layers/tests/compose.golden. An intended change is a
# golden update, reviewed in the diff:
#   NML_UPDATE_GOLDEN=1 cargo test -p nml-core --lib layers
# Two commits compare with `NML_COMPOSE_DUMP=<dir>` at each, then diff -r.

# Clean build artifacts
clean:
    cargo clean
