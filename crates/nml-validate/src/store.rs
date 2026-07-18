//! The per-user schema-package store (RFC 0030), read side.
//!
//! Layout — content-addressed, immutable:
//! `<base>/schema-packages/<name>/<version>+<hash8>/` slots, plus a
//! `<base>/schema-packages/<name>/current` pointer file naming the active
//! slot and its full content hash. Slots are written exactly once (temp-dir +
//! rename); the pointer rename is the only mutation of live data. Publishers
//! (nudge's auto-sync) own the write side; this module is the consumer read
//! path plus the layout definition both sides share.

use std::path::PathBuf;

use crate::package::{PackageError, SchemaPackage};

/// Pointer file name inside a package's store directory.
const CURRENT_POINTER: &str = "current";

/// The store subdirectory under the user data dir.
const STORE_SUBDIR: &str = "schema-packages";

/// A handle on a store root. `Store::user()` is the production location;
/// `Store::at` exists for consumers with an explicit base (tests, future
/// overrides).
#[derive(Debug, Clone)]
pub struct Store {
    base: PathBuf,
}

/// The per-validation-pass freshness probe (RFC 0030): the pointer file's
/// *content* (~80 bytes — one syscall against page cache, the same budget as
/// a stat). Content is exact by construction — the full hash is in it — so
/// no mtime-granularity hazard exists. `None` = not installed.
pub type PointerContent = Option<String>;

/// A resolved `current` slot: the loaded package plus its store identity.
#[derive(Debug)]
pub struct CurrentSlot {
    pub package: SchemaPackage,
    /// The full hash recorded in the pointer (verified against the loaded
    /// package's recomputed hash).
    pub content_hash: String,
}

/// Store-read failures, each a nameable degraded state.
#[derive(Debug)]
pub enum StoreError {
    /// No pointer for this package — the publisher has never synced here.
    NotInstalled,
    /// The pointer or slot is unreadable/malformed, or the slot's recomputed
    /// hash does not match the pointer (corruption / torn write).
    Corrupt { detail: String },
    /// The slot's package failed to load.
    Package(PackageError),
    /// A publish could not write the slot or flip the pointer.
    Write { detail: String },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => write!(f, "package is not installed in the store"),
            Self::Corrupt { detail } => write!(f, "store entry is corrupt: {detail}"),
            Self::Package(e) => write!(f, "stored package failed to load: {e}"),
            Self::Write { detail } => write!(f, "store write failed: {detail}"),
        }
    }
}

impl Store {
    pub fn at(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    /// The per-user store: `NML_SCHEMA_STORE_DIR` when set (an operational
    /// override for sandboxed CI, read-only homes, hermetic caches — and the
    /// process-e2e seam; the `CARGO_HOME` pattern), else
    /// `<data_dir>/nml/schema-packages/…` (XDG on Linux,
    /// `~/Library/Application Support` on macOS). Resolution lives HERE, in
    /// the crate both the publisher and the editor share: if only one side
    /// honored the override they would silently read different stores —
    /// the drift disease itself. `None` when the platform reports no data
    /// dir — consumers treat that as not-installed, never an error.
    pub fn user() -> Option<Self> {
        if let Some(dir) = std::env::var_os("NML_SCHEMA_STORE_DIR") {
            let path = PathBuf::from(dir);
            // Absolute only: a relative override would resolve against each
            // process's own CWD, splitting publisher and editor onto
            // different stores — the exact drift this shared resolver
            // exists to prevent. Relative values are ignored (fall through
            // to the platform default).
            if path.is_absolute() {
                return Some(Self::at(path));
            }
        }
        dirs::data_dir().map(|d| Self::at(d.join("nml")))
    }

    /// All store paths derive from here; a non-identifier name (a hostile
    /// pin like `../../x` or an absolute path, which `Path::join` would let
    /// *replace* the base) never touches the filesystem — callers see
    /// not-installed instead (RFC 0030 Security: charset enforced on every
    /// resolution path, not just at sync).
    fn package_dir(&self, name: &str) -> Option<PathBuf> {
        crate::package::valid_package_name(name).then(|| self.base.join(STORE_SUBDIR).join(name))
    }

    fn pointer_path(&self, name: &str) -> Option<PathBuf> {
        Some(self.package_dir(name)?.join(CURRENT_POINTER))
    }

    /// Read the pointer content — the freshness guard AND the load input,
    /// one read per pass (no guard-vs-load divergence to reason about).
    /// `None` = not installed (including invalid names, which never touch
    /// the filesystem).
    pub fn pointer_content(&self, name: &str) -> PointerContent {
        let path = self.pointer_path(name)?;
        std::fs::read_to_string(path).ok()
    }

    /// Resolve and load the `current` slot for `name`, verifying the loaded
    /// package's recomputed content hash against the pointer's recorded one —
    /// hash verification happens here, on load, never per keystroke.
    pub fn read_current(&self, name: &str) -> Result<CurrentSlot, StoreError> {
        match self.pointer_content(name) {
            Some(pointer) => self.load_current(name, &pointer),
            None => Err(StoreError::NotInstalled),
        }
    }

    /// Load the slot a pointer names (callers hold the pointer content they
    /// already read as the freshness guard).
    pub fn load_current(&self, name: &str, pointer: &str) -> Result<CurrentSlot, StoreError> {
        let mut lines = pointer.lines();
        let (slot, hash) = match (lines.next(), lines.next()) {
            (Some(slot), Some(hash)) if !slot.is_empty() && hash.starts_with("blake3:") => {
                (slot.to_string(), hash.to_string())
            }
            _ => {
                return Err(StoreError::Corrupt {
                    detail: "pointer must hold a slot name and a blake3 hash".to_string(),
                })
            }
        };
        // Slot names are single path components; a pointer must not be able
        // to walk the filesystem.
        if slot.contains('/') || slot.contains('\\') || slot.contains("..") {
            return Err(StoreError::Corrupt {
                detail: "pointer slot name is not a plain directory name".to_string(),
            });
        }
        let slot_dir = self.package_dir(name).expect("validated above").join(&slot);
        let package = SchemaPackage::from_dir(&slot_dir).map_err(StoreError::Package)?;
        let actual = package.content_hash();
        if actual != hash {
            return Err(StoreError::Corrupt {
                detail: format!("slot hash {actual} does not match pointer {hash}"),
            });
        }
        if package.manifest.name != name {
            return Err(StoreError::Corrupt {
                detail: format!(
                    "slot manifest names package '{}', pointer is for '{name}'",
                    package.manifest.name
                ),
            });
        }
        Ok(CurrentSlot {
            package,
            content_hash: hash,
        })
    }

    /// Package names present in the store (a directory with a `current`
    /// pointer). Feeds auto-association's "known package" set (RFC 0030) —
    /// the store-inclusive reading is what makes zero-config work.
    pub fn list_names(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.base.join(STORE_SUBDIR)) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join(CURRENT_POINTER).is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| crate::package::valid_package_name(n))
            .collect();
        names.sort();
        names
    }

    /// Slot directory name for a package identity: `<version>+<hash8>`.
    /// Shared layout definition — the publisher's write side must agree.
    pub fn slot_name(version: &str, content_hash: &str) -> String {
        format!("{version}+{}", hash8(content_hash))
    }

    /// Inventory the store for human display (RFC 0030 `schema list`).
    /// Derived from pointer files and directory names only — no package
    /// loads, no hash recomputation — so listing stays cheap and a corrupt
    /// slot cannot fail it. A corrupt/unreadable pointer yields a row with
    /// `"?"` identity fields rather than being dropped: an operator listing
    /// the store to debug it must SEE the broken entry, not miss it.
    pub fn list(&self) -> Vec<PackageListing> {
        self.list_names()
            .into_iter()
            .map(|name| {
                let (version, short) = self
                    .pointer_content(&name)
                    .and_then(|p| pointer_identity(&p))
                    .unwrap_or_else(|| ("?".to_string(), "?".to_string()));
                // Non-dot directories only: `.staging-*` / `.pointer-*` temp
                // artifacts are publish mechanics, not slots.
                let slot_count = self
                    .package_dir(&name)
                    .and_then(|dir| std::fs::read_dir(dir).ok())
                    .map(|entries| {
                        entries
                            .filter_map(|e| e.ok())
                            .filter(|e| e.path().is_dir())
                            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                            .count()
                    })
                    .unwrap_or(0);
                // Pointer mtime = when `current` last FLIPPED. An Unchanged
                // publish returns before touching the pointer, so this is the
                // last content-changing publish — not the last sync attempt.
                let published = self
                    .pointer_path(&name)
                    .and_then(|p| std::fs::metadata(p).ok())
                    .and_then(|m| m.modified().ok());
                PackageListing {
                    name,
                    version,
                    hash8: short,
                    slot_count,
                    published,
                }
            })
            .collect()
    }
}

/// One row of [`Store::list`]. Identity fields are `"?"` when the pointer
/// is corrupt — visible degradation, never silent omission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageListing {
    pub name: String,
    pub version: String,
    /// Short content hash (see [`hash8`]) of the `current` slot.
    pub hash8: String,
    /// Slot directories on disk: the pointed slot plus retained history.
    pub slot_count: usize,
    /// Pointer-file mtime — the last publish. `None` when unreadable.
    pub published: Option<std::time::SystemTime>,
}

/// Parse `(version, hash8)` out of a two-line pointer without touching the
/// slot: line 1 is `<version>+<hash8>` (the hash is the suffix after the
/// LAST `+`, since versions may themselves carry `+build` metadata), line 2
/// the full `blake3:` hash. The short form comes from [`hash8`] on line 2 —
/// the canonical truncation owner — not the slot suffix, so a listing row
/// always shows the hash the pointer actually pins. `None` on any shape
/// violation (the caller renders the visible `"?"` row).
fn pointer_identity(pointer: &str) -> Option<(String, String)> {
    let mut lines = pointer.lines();
    let slot = lines.next()?;
    let hash = lines.next()?;
    if !hash.starts_with("blake3:") {
        return None;
    }
    let (version, slot_hash) = slot.rsplit_once('+')?;
    if version.is_empty() || slot_hash.is_empty() {
        return None;
    }
    Some((version.to_string(), hash8(hash)))
}

/// Result of a publish: whether anything changed.
#[derive(Debug, PartialEq, Eq)]
pub enum PublishOutcome {
    /// `current` already pointed at this exact content.
    Unchanged,
    /// A slot was written (if absent) and `current` now points at it.
    Published { slot: String },
}

/// Process-unique temp-name nonce: pid + atomic counter — two threads
/// publishing the same package stage into distinct trees ("safe by
/// construction" must hold within a process, not just across processes).
fn temp_nonce() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

impl Store {
    /// Publish a package (RFC 0030 write side): write its content-addressed
    /// slot if absent (temp-dir + rename — atomic for creation; slots are
    /// immutable and never rewritten), atomically flip `current`, then GC.
    /// Idempotent — the steady state is one pointer read and a hash compare.
    /// Concurrent publishers are safe by construction: same-content racers
    /// rename to the same slot (first wins, second's rename fails onto an
    /// identical directory and is ignored), and the pointer flip is atomic.
    pub fn publish(&self, package: &SchemaPackage) -> Result<PublishOutcome, StoreError> {
        let name = &package.manifest.name;
        let hash = package.content_hash();
        let slot = Self::slot_name(&package.manifest.version, &hash);
        let pointer_value = format!("{slot}\n{hash}\n");
        if self.pointer_content(name).as_deref() == Some(pointer_value.as_str()) {
            return Ok(PublishOutcome::Unchanged);
        }
        let write_err = |detail: String| StoreError::Write { detail };
        let package_dir = self
            .package_dir(name)
            .ok_or_else(|| write_err(format!("'{name}' is not a valid package name")))?;
        let slot_dir = package_dir.join(&slot);
        if !slot_dir.is_dir() {
            let staging = package_dir.join(format!(".staging-{}", temp_nonce()));
            let _ = std::fs::remove_dir_all(&staging);
            std::fs::create_dir_all(&staging).map_err(|e| write_err(e.to_string()))?;
            std::fs::write(
                staging.join(format!("{name}.package.nml")),
                &package.manifest_text,
            )
            .map_err(|e| write_err(e.to_string()))?;
            for (logical, text) in &package.sources {
                let file = package
                    .manifest
                    .schemas
                    .iter()
                    .find(|e| e.name == *logical)
                    .map(|e| e.file.clone())
                    .ok_or_else(|| write_err(format!("no []schema entry for '{logical}'")))?;
                // Defense in depth: the write side enforces plain file names
                // exactly as every read side does — a manifest must never be
                // able to write outside its own slot.
                crate::package::check_plain_file_name(&file).map_err(write_err)?;
                std::fs::write(staging.join(file), text).map_err(|e| write_err(e.to_string()))?;
            }
            if let Err(e) = std::fs::rename(&staging, &slot_dir) {
                // A same-content racer won the rename: identical slot, fine.
                let _ = std::fs::remove_dir_all(&staging);
                if !slot_dir.is_dir() {
                    return Err(write_err(e.to_string()));
                }
            }
        }
        // Atomic pointer flip: write-then-rename within the same directory.
        let tmp = package_dir.join(format!(".pointer-{}", temp_nonce()));
        std::fs::write(&tmp, &pointer_value).map_err(|e| write_err(e.to_string()))?;
        std::fs::rename(&tmp, package_dir.join(CURRENT_POINTER))
            .map_err(|e| write_err(e.to_string()))?;
        self.gc(name, &slot);
        Ok(PublishOutcome::Published { slot })
    }

    /// Keep the pointed slot plus the 4 most recent others by mtime; skip
    /// slots younger than an hour (closes the create→flip race against
    /// concurrent publishers). Best-effort — GC failures never fail a
    /// publish.
    fn gc(&self, name: &str, pointed: &str) {
        let Some(dir) = self.package_dir(name) else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let now = std::time::SystemTime::now();
        let mut slots: Vec<(std::time::SystemTime, PathBuf, String)> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| {
                let file_name = e.file_name().into_string().ok()?;
                if file_name == pointed {
                    return None;
                }
                let modified = e.metadata().ok()?.modified().ok()?;
                // Orphaned temp dirs from crashed publishes (`.staging-*`)
                // age out like slots do — but they never count against the
                // keep-N budget, so mark them.
                Some((modified, e.path(), file_name))
            })
            .collect();
        slots.sort_by_key(|s| std::cmp::Reverse(s.0));
        let mut kept = 0;
        for (modified, path, file_name) in slots {
            let temp = file_name.starts_with('.');
            if !temp {
                kept += 1;
                if kept <= 4 {
                    continue;
                }
            }
            let aged = now
                .duration_since(modified)
                .map(|d| d.as_secs() >= 3600)
                .unwrap_or(false);
            if aged {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
}

/// The 8-hex-char short form of a content hash — the single owner of the
/// truncation used by slot names and human-facing identity strings.
pub fn hash8(content_hash: &str) -> String {
    content_hash
        .strip_prefix("blake3:")
        .unwrap_or(content_hash)
        .chars()
        .take(8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const MANIFEST: &str = "\
package demo:
    version = \"0.1.0\"
    formatVersion = 1

[]schema schemas:
    - core:
        file = \"core.model.nml\"

[]validator validators:
    - core:
        files:
            - \"demo.nml\"
        schemas:
            - core
        strict = true
";
    const CORE: &str = "model core:\n    name string+\n";

    /// Write a valid store entry by hand — the deliberate raw-layout fixture:
    /// it pins the on-disk format independently of `Store::publish`, and the
    /// tamper tests need byte-level control `publish` rightly refuses.
    fn write_store(base: &Path) -> String {
        let package = SchemaPackage::from_parts(MANIFEST, |_| Ok(CORE.to_string())).unwrap();
        let hash = package.content_hash();
        let slot = Store::slot_name("0.1.0", &hash);
        let slot_dir = base.join("schema-packages/demo").join(&slot);
        std::fs::create_dir_all(&slot_dir).unwrap();
        std::fs::write(slot_dir.join("demo.package.nml"), MANIFEST).unwrap();
        std::fs::write(slot_dir.join("core.model.nml"), CORE).unwrap();
        std::fs::write(
            base.join("schema-packages/demo/current"),
            format!("{slot}\n{hash}\n"),
        )
        .unwrap();
        hash
    }

    fn temp_base(tag: &str) -> PathBuf {
        // pid + process-wide counter: pid alone collides when a re-used pid
        // (or a same-process re-entry) hits the same tag.
        static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "nml-store-test-{tag}-{}-{nonce}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_current_roundtrip_and_stat_transitions() {
        let base = temp_base("roundtrip");
        let store = Store::at(&base);
        assert!(store.pointer_content("demo").is_none());
        assert!(matches!(
            store.read_current("demo"),
            Err(StoreError::NotInstalled)
        ));
        let hash = write_store(&base);
        assert!(
            store.pointer_content("demo").is_some(),
            "absent→present transition visible in the content probe"
        );
        let slot = store.read_current("demo").expect("reads back");
        assert_eq!(slot.content_hash, hash);
        assert_eq!(slot.package.manifest.name, "demo");
        assert!(slot.package.binding_for("demo.nml").is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Publish → read_current roundtrip: idempotence, pointer flip on
    /// content change, immutable prior slots.
    #[test]
    fn publish_roundtrip_and_idempotence() {
        let base = temp_base("publish");
        let store = Store::at(&base);
        let package = SchemaPackage::from_parts(MANIFEST, |_| Ok(CORE.to_string())).unwrap();
        let first = store.publish(&package).unwrap();
        assert!(matches!(first, PublishOutcome::Published { .. }));
        assert_eq!(store.publish(&package).unwrap(), PublishOutcome::Unchanged);
        let slot = store.read_current("demo").unwrap();
        assert_eq!(slot.content_hash, package.content_hash());
        // A changed package flips the pointer to a new slot; the old slot
        // remains on disk (immutable, GC'd only when aged).
        let edited =
            SchemaPackage::from_parts(MANIFEST, |_| Ok(format!("{CORE}// edit\n"))).unwrap();
        assert!(matches!(
            store.publish(&edited).unwrap(),
            PublishOutcome::Published { .. }
        ));
        assert_eq!(
            store.read_current("demo").unwrap().content_hash,
            edited.content_hash()
        );
        let old_slot = base
            .join("schema-packages/demo")
            .join(Store::slot_name("0.1.0", &package.content_hash()));
        assert!(old_slot.is_dir(), "prior slot is immutable, not replaced");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Same-package publishers racing within one process stage into
    /// distinct temp trees and converge on one valid store state.
    #[test]
    fn concurrent_same_package_publish_is_safe() {
        let base = temp_base("concurrent");
        let store = Store::at(&base);
        let package = std::sync::Arc::new(
            SchemaPackage::from_parts(MANIFEST, |_| Ok(CORE.to_string())).unwrap(),
        );
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let store = store.clone();
                let package = package.clone();
                scope.spawn(move || {
                    store.publish(&package).expect("publish never corrupts");
                });
            }
        });
        assert_eq!(
            store.read_current("demo").unwrap().content_hash,
            package.content_hash()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Best-effort contract's failure mode, deterministically on every
    /// platform and privilege level: a store base UNDER A REGULAR FILE fails
    /// with ENOTDIR — no chmod fixtures, no root/Windows caveats. The error
    /// must be `Write` (the publish path), not a validation error.
    #[test]
    fn publish_under_a_file_is_a_write_error() {
        let base = temp_base("enotdir");
        let file = base.join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        let store = Store::at(file.join("store"));
        let package = SchemaPackage::from_parts(MANIFEST, |_| Ok(CORE.to_string())).unwrap();
        match store.publish(&package) {
            Err(StoreError::Write { .. }) => {}
            other => panic!("expected Write error, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// GC keeps the pointed slot + 4 newest; only aged unpointed slots go.
    #[test]
    fn gc_prunes_only_aged_unpointed_slots() {
        let base = temp_base("gc");
        let store = Store::at(&base);
        let package = SchemaPackage::from_parts(MANIFEST, |_| Ok(CORE.to_string())).unwrap();
        store.publish(&package).unwrap();
        let dir = base.join("schema-packages/demo");
        // Six fake aged slots + one fresh one.
        for i in 0..6 {
            let slot = dir.join(format!("0.0.{i}+aaaaaaa{i}"));
            std::fs::create_dir_all(&slot).unwrap();
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
            let f = std::fs::File::open(&slot).unwrap();
            f.set_modified(old).unwrap();
        }
        std::fs::create_dir_all(dir.join("0.0.9+fresh000")).unwrap();
        // Re-publishing (unchanged) skips GC; force one via a content change.
        let edited = SchemaPackage::from_parts(MANIFEST, |_| Ok(format!("{CORE}// gc\n"))).unwrap();
        store.publish(&edited).unwrap();
        let remaining: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert!(
            remaining.iter().any(|s| s == "0.0.9+fresh000"),
            "grace-aged slot survives: {remaining:?}"
        );
        // pointed + 4 newest + fresh survive; the oldest aged fakes are gone.
        assert!(remaining.len() <= 7, "{remaining:?}");
        assert!(
            !remaining.iter().any(|s| s == "0.0.0+aaaaaaa0"),
            "oldest aged slot pruned: {remaining:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `list` over a healthy published package and a corrupt-pointer one:
    /// the healthy row carries real identity, the corrupt row survives with
    /// visible `"?"` fields (never dropped from the inventory).
    #[test]
    fn list_shows_healthy_and_corrupt_entries() {
        let base = temp_base("list");
        let store = Store::at(&base);
        assert!(store.list().is_empty(), "empty store lists nothing");

        let hash = crate::test_support::publish_demo(&store);
        // Second package with a corrupt (one-line, no blake3) pointer.
        let broken_dir = base.join("schema-packages/broken");
        std::fs::create_dir_all(broken_dir.join("0.5.0+deadbeef")).unwrap();
        std::fs::write(broken_dir.join(CURRENT_POINTER), "garbage\n").unwrap();

        let listings = store.list();
        assert_eq!(listings.len(), 2, "{listings:?}");
        // list_names sorts, so 'broken' precedes 'demo'.
        let broken = &listings[0];
        assert_eq!(broken.name, "broken");
        assert_eq!(
            broken.version, "?",
            "corrupt pointer is visible, not hidden"
        );
        assert_eq!(broken.hash8, "?");
        assert_eq!(broken.slot_count, 1, "slot dirs still counted");
        let demo = &listings[1];
        assert_eq!(demo.name, "demo");
        assert_eq!(demo.version, "0.1.0");
        assert_eq!(demo.hash8, hash8(&hash));
        assert_eq!(demo.slot_count, 1);
        assert!(
            demo.published.is_some(),
            "pointer mtime is readable on a just-published package"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn corrupt_states_are_named() {
        let base = temp_base("corrupt");
        let store = Store::at(&base);
        let hash = write_store(&base);
        // Tampered source ⇒ hash mismatch.
        std::fs::write(
            base.join("schema-packages/demo")
                .join(Store::slot_name("0.1.0", &hash))
                .join("core.model.nml"),
            "model core:\n    name string+\n    evil string?\n",
        )
        .unwrap();
        assert!(matches!(
            store.read_current("demo"),
            Err(StoreError::Corrupt { .. })
        ));
        // Traversal-shaped pointer ⇒ rejected before any read.
        std::fs::write(
            base.join("schema-packages/demo/current"),
            "../../etc\nblake3:abc\n",
        )
        .unwrap();
        assert!(matches!(
            store.read_current("demo"),
            Err(StoreError::Corrupt { .. })
        ));
        let _ = std::fs::remove_dir_all(&base);
    }
}
