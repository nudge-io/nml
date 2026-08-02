//! WASI-compat filesystem helpers for the neutral server.

use std::path::Path;

/// Deliberately-leaked listing handles minted so far (the wasi branch of
/// [`ReadDir`]'s `Drop`). Native builds never increment it — the accessor
/// and its unit test keep the wiring compiled and observably zero on every
/// target, so the wasi-only arithmetic cannot rot uncompiled.
#[cfg(any(target_os = "wasi", test))]
static LEAKED_HANDLES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Budget of deliberately-leaked listing handles before [`read_dir`] starts
/// refusing (wasi only). What it bounds is MEMORY, not fd space: the
/// `wasm-wasi-core` host's fd table is a monotonic-counter JS `Map` — no
/// slot reuse, no ceiling — so the leak can never exhaust that host's fds,
/// but every forgotten listing pins one host map entry plus the guest `DIR`
/// heap allocation forever. 4096 caps that otherwise-unbounded tail, and on
/// WASI hosts that DO enforce fd ceilings it refuses before any plausible
/// cap is reached. Callers already degrade honestly at a refusal (claims
/// walk → `Truncated` → `Undetermined`; index walk skips the directory).
/// Retirement condition: same as the wrapper — delete once a fixed
/// `wasm-wasi-core` is the supported floor.
#[cfg(any(target_os = "wasi", test))]
const LEAKED_HANDLE_BUDGET: usize = 4096;

/// How many listing handles this process has leaked so far — 0 on native,
/// where `Drop` really disposes.
#[cfg(any(target_os = "wasi", test))]
fn leaked_handles() -> usize {
    LEAKED_HANDLES.load(std::sync::atomic::Ordering::Relaxed)
}

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
/// destructor never runs. The leak's cost is MEMORY, not fd space
/// (`wasm-wasi-core`'s Map-based fd table cannot exhaust — see
/// `LEAKED_HANDLE_BUDGET`): each forgotten listing pins one host map
/// entry plus the guest `DIR` heap allocation, counted in
/// `LEAKED_HANDLES` and capped by the budget — strictly better than the
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
    // Past the budget, every further listing would leak yet another
    // handle; refusing converts the unbounded-memory tail into the
    // bounded-walk degradation callers already handle.
    #[cfg(target_os = "wasi")]
    if leaked_handles() > LEAKED_HANDLE_BUDGET {
        return Err(std::io::Error::other(
            "wasi fd budget exhausted (leaked-handle cap)",
        ));
    }
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
            {
                // Each forget is one more entry in the leak ledger the
                // budget check in `read_dir` reads.
                LEAKED_HANDLES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::mem::forget(inner);
            }
            #[cfg(not(target_os = "wasi"))]
            drop(inner);
        }
    }
}

#[cfg(test)]
mod tests {
    /// The leak ledger's native contract: `Drop` really disposes here, so a
    /// full open→iterate→drop cycle through the wrapper leaves the counter
    /// at zero — and the budget wiring stays compiled on every target. The
    /// wasi increment/refuse branch can only be pinned structurally (it
    /// needs the wasm-wasi-core host to run), so this is the whole
    /// native-side observable.
    #[cfg(not(target_os = "wasi"))]
    #[test]
    fn native_drop_disposes_and_leaks_nothing() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let listed = super::read_dir(&src).expect("src listable").count();
        assert!(listed > 0, "src has entries");
        assert_eq!(
            super::leaked_handles(),
            0,
            "only the wasi mem::forget branch may count"
        );
        // Compile-time: a zero budget would refuse the FIRST listing on
        // wasi — the wrapper would be worse than the bug it works around.
        const { assert!(super::LEAKED_HANDLE_BUDGET > 0) };
    }

    /// Everything but code: string/char literals and comments (line, and
    /// nested block) replaced with a space, so the matcher below sees
    /// tokens only. A `"a//b"` literal can no longer mask the rest of its
    /// line, and a `'"'` char literal cannot desynchronize the string
    /// scanner. Raw strings ride their hash fence.
    fn code_only(text: &str) -> String {
        let b: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                '/' if b.get(i + 1) == Some(&'/') => {
                    while i < b.len() && b[i] != '\n' {
                        i += 1;
                    }
                }
                '/' if b.get(i + 1) == Some(&'*') => {
                    let mut depth = 1usize;
                    i += 2;
                    while i < b.len() && depth > 0 {
                        if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                            depth += 1;
                            i += 2;
                        } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                            depth -= 1;
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    out.push(' ');
                }
                'r' if matches!(b.get(i + 1), Some('"') | Some('#')) => {
                    let mut j = i + 1;
                    let mut hashes = 0usize;
                    while b.get(j) == Some(&'#') {
                        hashes += 1;
                        j += 1;
                    }
                    if b.get(j) == Some(&'"') {
                        j += 1;
                        while j < b.len() {
                            if b[j] == '"' {
                                let mut k = j + 1;
                                let mut h = 0usize;
                                while h < hashes && b.get(k) == Some(&'#') {
                                    h += 1;
                                    k += 1;
                                }
                                if h == hashes {
                                    j = k;
                                    break;
                                }
                            }
                            j += 1;
                        }
                        i = j;
                        out.push(' ');
                    } else {
                        out.push('r');
                        i += 1;
                    }
                }
                '"' => {
                    i += 1;
                    while i < b.len() {
                        match b[i] {
                            '\\' => i += 2,
                            '"' => {
                                i += 1;
                                break;
                            }
                            _ => i += 1,
                        }
                    }
                    out.push(' ');
                }
                // `'"'` / `'\"'` would otherwise open a phantom string;
                // other ticks (lifetimes, plain char literals) are inert
                // to the matcher and pass through.
                '\'' if b.get(i + 1) == Some(&'"') && b.get(i + 2) == Some(&'\'') => {
                    out.push(' ');
                    i += 3;
                }
                '\'' if b.get(i + 1) == Some(&'\\')
                    && b.get(i + 2) == Some(&'"')
                    && b.get(i + 3) == Some(&'\'') =>
                {
                    out.push(' ');
                    i += 4;
                }
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        out
    }

    /// Source-level ratchet: EVERY directory listing in this crate goes
    /// through [`super::read_dir`]. A raw `std::fs::read_dir` compiles
    /// clean, passes every native test, and reintroduces the
    /// wasm-wasi-core abort (`unexpected error during closedir`) for
    /// every wasm user — the exact failure that shipped as an
    /// unexplained E2E timeout. The extension E2E only guards the call
    /// sites it happens to exercise; this guards them all, at unit-test
    /// speed, host-independently.
    ///
    /// The matcher is deliberately maximal: after scrubbing the legal
    /// wrapper spelling, ANY remaining `read_dir` token in code is a
    /// violation — the call form (`fs::read_dir(`), the method form
    /// (`.read_dir(`), UFCS (`Path::read_dir(`), imports and `as`
    /// aliases, and function-pointer bindings (`let f =
    /// std::fs::read_dir;`) all collapse into the same substring. A new
    /// legitimate helper must route through `wasi_fs` (or extend the
    /// sentinel here, in review).
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
                let collapsed: String = code_only(&text)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("");
                let scrubbed = collapsed.replace("wasi_fs::read_dir(", "\u{0}LEGAL\u{0}(");
                if scrubbed.contains("read_dir") {
                    for (i, line) in text.lines().enumerate() {
                        let code = code_only(line);
                        if code.contains("read_dir") && !code.contains("wasi_fs::read_dir") {
                            offenders.push(format!(
                                "{}:{}: {}",
                                path.display(),
                                i + 1,
                                line.trim()
                            ));
                        }
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

    /// The scrubber itself: each bypass class the ratchet must catch, and
    /// the masking shapes it must NOT be fooled by.
    #[test]
    fn ratchet_scrubber_catches_every_bypass_shape() {
        let catches = [
            "std::fs::read_dir(&d)",
            "Path::read_dir(&d)",
            "d.read_dir()",
            "use std::fs::read_dir;",
            "use std::fs::{read_dir as rd};",
            "let f = std::fs::read_dir; f(&d)",
            "let p = base.join(\"a//b\"); std::fs::read_dir(&p)",
            "let c = '\"'; std::fs::read_dir(&d)",
        ];
        for src in catches {
            let collapsed: String = code_only(src).split_whitespace().collect();
            let scrubbed = collapsed.replace("wasi_fs::read_dir(", "\u{0}LEGAL\u{0}(");
            assert!(scrubbed.contains("read_dir"), "must catch: {src}");
        }
        let passes = [
            "crate::wasi_fs::read_dir(&d)",
            "// std::fs::read_dir(&d) in a comment",
            "/* fs::read_dir in a block /* nested */ comment */",
            "let s = \"read_dir mentioned in a string\";",
            "let r = r#\"read_dir in a raw string\"#;",
        ];
        for src in passes {
            let collapsed: String = code_only(src).split_whitespace().collect();
            let scrubbed = collapsed.replace("wasi_fs::read_dir(", "\u{0}LEGAL\u{0}(");
            assert!(!scrubbed.contains("read_dir"), "must pass: {src}");
        }
    }
}
