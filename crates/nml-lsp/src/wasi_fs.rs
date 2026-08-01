//! WASI-compat filesystem helpers for the neutral server.

use std::path::Path;

/// Open a directory listing, ABORT-PROOF under `wasm-wasi-core`.
///
/// Rust std's `ReadDir` panics in `Drop` when `closedir` fails
/// ("unexpected error during closedir"), and VS Code's WASI host
/// (`ms-vscode.wasm-wasi-core`) returns `EBADF` from `fd_close` on
/// directory fds where every native platform (and wasmtime) succeeds.
/// The returned wrapper iterates LAZILY — callers keep their
/// early-return caps and never materialize a listing — and its own
/// `Drop` disposes of the inner iterator: dropped normally on native,
/// deliberately LEAKED (`mem::forget`) on wasi so the panicking
/// destructor never runs. The cost is one guest fd-table slot per
/// listing, bounded by listing frequency and strictly better than the
/// alternative (the whole server aborts mid-pull, taking every future
/// diagnostic with it — the failure mode that shipped as an
/// unexplained E2E timeout).
///
/// Unreadable entries are skipped (`flatten` semantics — what every
/// call site did before this wrapper existed).
///
/// Upstream: `docs/upstream/wasm-wasi-core-fd-close-ebadf.md` (draft
/// issue with the minimal repro). Delete this module — and its source
/// ratchet test — once a fixed `wasm-wasi-core` is the supported floor.
pub(crate) fn read_dir(dir: &Path) -> std::io::Result<ReadDir> {
    Ok(ReadDir(Some(std::fs::read_dir(dir)?)))
}

pub(crate) struct ReadDir(Option<std::fs::ReadDir>);

impl Iterator for ReadDir {
    type Item = std::fs::DirEntry;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.as_mut()?.find_map(|e| e.ok())
    }
}

impl Drop for ReadDir {
    fn drop(&mut self) {
        if let Some(inner) = self.0.take() {
            #[cfg(target_os = "wasi")]
            std::mem::forget(inner);
            #[cfg(not(target_os = "wasi"))]
            drop(inner);
        }
    }
}

#[cfg(test)]
mod tests {
    /// Source-level ratchet: EVERY directory listing in this crate goes
    /// through [`super::read_dir`]. A raw `std::fs::read_dir` compiles
    /// clean, passes every native test, and reintroduces the
    /// wasm-wasi-core abort (`unexpected error during closedir`) for
    /// every wasm user — the exact failure that shipped as an
    /// unexplained E2E timeout. The extension E2E only guards the call
    /// sites it happens to exercise; this guards them all, at unit-test
    /// speed, host-independently.
    #[test]
    fn all_dir_listings_go_through_the_wasi_safe_helper() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("crate src readable") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs")
                    || path.file_name().is_some_and(|n| n == "wasi_fs.rs")
                {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("source readable");
                for (i, line) in text.lines().enumerate() {
                    if line.contains("fs::read_dir(")
                        && !line.contains("wasi_fs::read_dir(")
                        && !line.trim_start().starts_with("//")
                    {
                        offenders.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "raw fs::read_dir outside wasi_fs.rs — use wasi_fs::read_dir \
             (panicking ReadDir Drop aborts the wasm server under wasm-wasi-core):\n{}",
            offenders.join("\n")
        );
    }
}
