# Draft upstream issue — ms-vscode/vscode-wasm

Status: DRAFT, not yet filed. File against https://github.com/microsoft/vscode-wasm
(the `wasm-wasi-core` extension). Our mitigation
(`crates/nml-lsp/src/wasi_fs.rs`) is correct regardless of the upstream fix;
delete it (and its source ratchet test) once a fixed host version is the
floor we support.

---

**Title:** `fd_close` on a directory fd returns EBADF, aborting Rust guests
(std `ReadDir` panics in `Drop` on `closedir` failure)

**Host:** `ms-vscode.wasm-wasi-core` 1.0.2, VS Code 1.131.0 (macOS arm64 and
`ubuntu-latest` CI — reproduced on both).
**Guest:** Rust `wasm32-wasip1` (rustc 1.97.1, wasi-libc via std).

## Symptom

Any `std::fs::read_dir` of a mounted workspace directory can abort the whole
guest at iterator drop:

```
thread 'main' (1) panicked at library/std/src/sys/fs/unix.rs:1031:9:
unexpected error during closedir: Os { code: 8, kind: Uncategorized, message: "Bad file descriptor" }
```

Rust's `ReadDir` treats a failing `closedir` as a guest-state invariant
violation and panics in `Drop`; with `panic=abort` (the wasip1 default) the
process dies. In an LSP server this presents as: the server answers a few
requests, then goes permanently silent — every downstream symptom
(diagnostic timeouts) points away from the actual cause.

The same guest binary run under wasmtime (`wasmtime run --dir host::/guest`)
performs identical `read_dir` sequences with no error: `fd_close` on the
directory fd succeeds there, so this looks host-specific, not wasi-libc.

## Repro (minimal guest)

```rust
// wasm32-wasip1; run under wasm-wasi-core with any mounted, non-empty dir.
fn main() {
    for entry in std::fs::read_dir("/workspace").unwrap() {
        let _ = entry.unwrap().path();
    }
    // ReadDir dropped here → wasi-libc closedir → fd_close → EBADF → abort.
    println!("ok"); // never reached when the bug fires
}
```

In our real workload the abort fires on the second-and-later listings of the
same mounted tree within one process (first listings at startup survive),
which suggests fd-table state in the host's readdir/close lifecycle rather
than a constant failure — but a single listing as above already reproduces
in our environment.

## Expected

`fd_close` on a directory fd obtained for `fd_readdir` succeeds (POSIX
`closedir` contract). Guests cannot defend at the failure point: the close
happens inside std's destructor, and Rust deliberately panics there on the
grounds that a failing close of a known-valid fd means corrupted process
state.

## Workaround we ship

Drain entries, then `std::mem::forget` the `ReadDir` on wasi builds so the
destructor never runs — leaking one guest fd-table slot per listing.
Bounded and survivable, but the wrong place for the fix to live.
