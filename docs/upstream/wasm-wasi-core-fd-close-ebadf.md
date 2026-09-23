# Draft upstream issue — ms-vscode/vscode-wasm

Status: DRAFT, not yet filed. File against https://github.com/microsoft/vscode-wasm
(the `wasm-wasi-core` extension). Our mitigation
(`crates/nml-lsp/src/wasi_fs.rs`) is correct regardless of the upstream fix;
delete it (and its source ratchet test) once a fixed host version is the
floor we support.

---

**Title:** `fd_close` on a MOUNT ROOT's directory fd returns EBADF from the
second close on, aborting Rust guests (std `ReadDir` panics in `Drop` on
`closedir` failure)

**Host:** `ms-vscode.wasm-wasi-core` 1.0.2, VS Code 1.131.0 (macOS arm64 and
`ubuntu-latest` CI — reproduced on both).
**Guest:** Rust `wasm32-wasip1` (rustc 1.97.1, wasi-libc via std).

## Symptom

`std::fs::read_dir` of a MOUNT ROOT — a mounted workspace folder itself, not
a directory under it — aborts the guest at iterator drop, from the SECOND
listing of that root on. Measured under the host: a subdirectory lists and
closes any number of times; the root's first close succeeds and every later
one fails. The host opens the root by the relative path `"."`, which its node
table hands back without taking a reference, while the close releases one.

The abort itself:

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
// wasm32-wasip1; run under wasm-wasi-core with /workspace a MOUNT ROOT.
fn main() {
    for round in 1..=2 {
        for entry in std::fs::read_dir("/workspace").unwrap() {
            let _ = entry.unwrap().path();
        }
        // ReadDir dropped here → wasi-libc closedir → fd_close.
        // Round 1 closes cleanly; round 2 gets EBADF → panic in Drop → abort.
        println!("listed, round {round}");
    }
    println!("ok"); // never reached when the bug fires
}
```

In our workload that is every workspace folder, listed by every discovery
pass: the server answers the first few requests and then dies. The repro
above must therefore list `/workspace` TWICE — a single listing of a mount
root survives, and a listing of any subdirectory survives indefinitely.

## Expected

`fd_close` on a directory fd obtained for `fd_readdir` succeeds (POSIX
`closedir` contract). Guests cannot defend at the failure point: the close
happens inside std's destructor, and Rust deliberately panics there on the
grounds that a failing close of a known-valid fd means corrupted process
state.

## Workaround we ship

We list through `rustix::fs::Dir` instead of `std::fs::ReadDir`
(`crates/nml-lsp/src/wasi_fs.rs`). Its `Drop` calls `closedir` and IGNORES
the result, and the descriptor is released for real — the host drops its
table entry even when its own close errors. Nothing is leaked and nothing
aborts.

An earlier version of this note described a different workaround — drain the
entries and `std::mem::forget` the `ReadDir` — which leaked one guest
fd-table slot per listing. It is gone, with the handle counter and budget
that tried to bound it: a listing memo makes the leak smaller but never
finite, and the host's fd table is not reused.

The wrapper only opens; the listing itself (its sort, its kinds, the refusal
of a whole listing on one unreadable entry) is the validation kernel's one
rule (`nml_validate::workspace::listing`), the same the native oracle runs —
the wrapper yields entries exactly as `std` does, errors included.
