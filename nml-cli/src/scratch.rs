//! A test's scratch directory, removed when its guard drops — on a red
//! assertion too (a unit test has no `CARGO_TARGET_TMPDIR`; the guard
//! mirrors the integration suite's under the system temp dir, where
//! thousands of `nml-*` leftovers stood after rounds 80–84). ONE type
//! for the crate's unit tests; it derefs to its path.

use std::path::{Path, PathBuf};

pub(crate) struct Scratch(PathBuf);

impl Scratch {
    /// A fresh, empty directory named by `tag`, the pid and a
    /// process-wide nonce (a re-used pid, or a same-process re-entry,
    /// must not collide).
    pub(crate) fn new(tag: &str) -> Self {
        static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nml-{tag}-{}-{nonce}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            panic!("scratch {}: {e}", dir.display());
        }
        Self(dir)
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
