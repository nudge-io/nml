//! Directory listings for the wasm editor: the ONE lister, and the
//! stamp-keyed memo the resolver hands to the kernel's oracle.
//!
//! # Why this module exists
//!
//! Rust std's `ReadDir` panics in `Drop` when `closedir` fails
//! ("unexpected error during closedir"), and VS Code's WASI host
//! (`ms-vscode.wasm-wasi-core`) returns `EBADF` from `fd_close` for the
//! descriptor a listing of a MOUNT ROOT holds: the host opens the root by
//! the relative path `"."`, which its node table hands back without
//! taking a reference, while the close releases one — so the first close
//! of a mount root succeeds and every later one fails. Measured under the
//! host: a subdirectory lists and closes any number of times; the mount
//! root — which is every workspace folder, listed by every discovery —
//! aborts the guest on the SECOND listing. In an LSP server that presents
//! as: the server answers a few requests, then goes permanently silent.
//!
//! [`read_dir`] therefore lists through `rustix::fs::Dir`, whose `Drop`
//! calls `closedir` and IGNORES its result, and the descriptor is closed
//! for real: the host drops its table entry even when its own close
//! errors. Nothing is leaked and nothing aborts.
//!
//! Upstream: `docs/upstream/wasm-wasi-core-fd-close-ebadf.md`. Delete the
//! source ratchet below — not the memo — once a fixed host is the floor.
//!
//! # Why the memo exists
//!
//! Every filesystem call on that host is an asynchronous round trip to
//! the extension host: a listing of a small directory costs ~0.29 ms and
//! a `stat` ~0.11 ms, where the same calls cost microseconds natively. A
//! discovery lists every directory under the root (255 in this
//! repository, 192 in a mid-size service repository) and the editor
//! rediscovers whenever a discovery INPUT changes — which is every
//! keystroke in an open manifest or schema source. [`Listings`] answers a
//! listing from the previous one while the directory's stamp (its size
//! and mtime, one `stat`) is unchanged, and freezes its answers for the
//! duration of ONE operation ([`Listings::snapshot`]) so a single walk
//! can never see a torn tree.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nml_validate::fs::{DirEntryLike, EntryKind};

/// One directory entry as the kernel's listing rule consumes it. The kind
/// is resolved when the entry is read — from `readdir`'s own type byte
/// where the host gives one, else by `lstat` — so an entry whose kind
/// cannot be read is an `Err` in the listing, which the kernel's rule
/// (`nml_validate::fs::listing`) refuses the whole listing on,
/// exactly as the native oracle does.
pub(crate) struct Entry(OsString, EntryKind);

impl DirEntryLike for Entry {
    fn name(&self) -> OsString {
        self.0.clone()
    }

    fn kind(&self) -> std::io::Result<EntryKind> {
        Ok(self.1)
    }
}

/// A directory's stamp: the two facts a `stat` gives that move whenever an
/// entry is created or removed (measured under `wasm-wasi-core`: both move
/// on a create AND on a delete). Nothing else is read — never the
/// content, never a second listing.
///
/// The second component is platform-native time from that one `stat`:
/// `last_write_time` on Windows, nanoseconds since the Unix epoch
/// elsewhere. When the time cannot be read, the memo does not cache.
///
/// On Windows the first component is usually `len()` from that same `stat`,
/// but directory `len()` is often zero on NTFS — and some Windows hosts do
/// not bump `last_write_time` when a child is created or removed — so when
/// `len()` is zero there the first component is the child count from one
/// `read_dir` pass (still one syscall family; never file content).
type Stamp = (u64, u64);

/// A directory's entries as they were read: names with their kinds, in
/// `readdir` order. Shared, never mutated — one allocation per directory
/// version, however many walks replay it.
type Held = Arc<Vec<(OsString, EntryKind)>>;

fn stamp_of(dir: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(dir).ok()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let t = meta.last_write_time();
        if t == 0 {
            return None;
        }
        let size = if meta.len() == 0 {
            std::fs::read_dir(dir).map(|rd| rd.count() as u64).ok()?
        } else {
            meta.len()
        };
        Some((size, t))
    }
    #[cfg(not(windows))]
    {
        let t = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        Some((meta.len(), t as u64))
    }
}

/// The resolver's listing memo: one entry per directory, held while the
/// directory's [`Stamp`] is unchanged.
///
/// What staleness this tolerates, exactly: a directory whose entries
/// changed without moving either its size or its mtime — the same
/// millisecond, the same reported size. Such a change cannot be a `.nml`
/// create or delete, because those arrive separately as watched-file
/// events and [`Self::clear`] drops the whole memo on one; and a universe
/// is a statement about `.nml` names and directory names only. The
/// alternative is not "fresher": a cached universe today is served
/// without looking at any directory at all, so a memo that re-stats every
/// directory on every pull is strictly MORE current than the cache it
/// feeds.
#[derive(Default)]
pub(crate) struct Listings {
    inner: Mutex<HashMap<PathBuf, (Stamp, Held)>>,
}

impl Listings {
    /// Forget everything: the blunt invalidation a watched-file create or
    /// delete takes, alongside the universe cache it invalidates.
    pub(crate) fn clear(&self) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// How many directories the memo is holding.
    ///
    /// The only way a test can see that [`Self::clear`] did anything: a
    /// held listing whose stamp still matches answers exactly what a
    /// fresh listing answers, so an assertion on the ANSWER passes
    /// whether or not the memo forgot. Measured — with `clear`'s body
    /// deleted, this module's tests stayed green.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// The lister for ONE operation. Within it a directory is listed or
    /// stamped at most once, so every answer the kernel's walk gets comes
    /// from one instant — a walk cannot see a file both present and
    /// absent. Across operations the memo re-stats (one `stat`) and
    /// re-lists only what moved.
    pub(crate) fn snapshot(
        &self,
    ) -> impl Fn(&Path) -> std::io::Result<Vec<std::io::Result<Entry>>> {
        let frozen: std::cell::RefCell<HashMap<PathBuf, Held>> =
            std::cell::RefCell::new(HashMap::new());
        move |dir: &Path| {
            if let Some(hit) = frozen.borrow().get(dir) {
                return Ok(replay(hit));
            }
            let entries = self.fresh(dir)?;
            frozen
                .borrow_mut()
                .insert(dir.to_path_buf(), Arc::clone(&entries));
            Ok(replay(&entries))
        }
    }

    /// The memo's own answer: the held listing while the stamp matches,
    /// else a real listing.
    fn fresh(&self, dir: &Path) -> std::io::Result<Held> {
        let stamp = stamp_of(dir);
        if let Some(stamp) = stamp {
            let held = self
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(dir)
                .filter(|(held, _)| *held == stamp)
                .map(|(_, entries)| Arc::clone(entries));
            if let Some(entries) = held {
                return Ok(entries);
            }
        }
        // A listing that refused an entry is never held: the refusal is
        // the listing's, and the kernel must meet it again next time.
        let entries = Arc::new(read_dir(dir)?);
        if let Some(stamp) = stamp {
            self.inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(dir.to_path_buf(), (stamp, Arc::clone(&entries)));
        }
        Ok(entries)
    }
}

/// The held listing as the kernel's rule consumes it. Every entry is
/// `Ok`: a listing that could not read an entry's kind was refused whole
/// (and never held), which is the same verdict the native oracle reaches
/// through a per-entry error.
fn replay(entries: &Held) -> Vec<std::io::Result<Entry>> {
    entries
        .iter()
        .map(|(name, kind)| Ok(Entry(name.clone(), *kind)))
        .collect()
}

/// List `dir` — THE directory listing of this crate (see the module doc:
/// std's own aborts the wasm server on a mount root). Entries come back in
/// `readdir` order with their `lstat` kinds; the kernel's listing rule
/// sorts them and refuses the whole listing on one it cannot read.
///
/// `Err` is the OPEN's failure. An entry whose kind cannot be read is
/// reported by failing the whole listing, which is what the kernel's rule
/// does with a per-entry error and what the native oracle does.
pub(crate) fn read_dir(dir: &Path) -> std::io::Result<Vec<(OsString, EntryKind)>> {
    lister::read_dir(dir)
}

/// The listing itself. `rustix::fs::Dir` wherever `rustix::fs` exists
/// (unix and wasi — the wasm editor's target, and the two platforms whose
/// test lanes exercise this module); `std` on the rest, which never run
/// this module in production: it is the wasm editor's lister, compiled
/// under `test` everywhere only so its wiring cannot rot uncompiled.
#[cfg(any(unix, target_os = "wasi"))]
mod lister {
    use std::ffi::OsString;
    use std::path::Path;

    use nml_validate::fs::EntryKind;
    use rustix::fs::{Dir, FileType, Mode, OFlags};

    #[cfg(unix)]
    fn os_string(bytes: &[u8]) -> OsString {
        use std::os::unix::ffi::OsStringExt as _;
        OsString::from_vec(bytes.to_vec())
    }

    #[cfg(target_os = "wasi")]
    fn os_string(bytes: &[u8]) -> OsString {
        use std::os::wasi::ffi::OsStringExt as _;
        OsString::from_vec(bytes.to_vec())
    }

    /// `readdir`'s own type byte, where the filesystem gives one.
    fn kind_of(ft: FileType) -> Option<EntryKind> {
        match ft {
            FileType::RegularFile => Some(EntryKind::File),
            FileType::Directory => Some(EntryKind::Dir),
            FileType::Symlink => Some(EntryKind::Symlink),
            FileType::Unknown => None,
            _ => Some(EntryKind::Other),
        }
    }

    /// The `lstat` fallback for a filesystem (or host) that reports no
    /// type byte — exactly what `std::fs::DirEntry::file_type` does.
    fn lstat_kind(dir: &Path, name: &OsString) -> std::io::Result<EntryKind> {
        let ft = std::fs::symlink_metadata(dir.join(name))?.file_type();
        Ok(if ft.is_symlink() {
            EntryKind::Symlink
        } else if ft.is_dir() {
            EntryKind::Dir
        } else if ft.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        })
    }

    pub(super) fn read_dir(dir: &Path) -> std::io::Result<Vec<(OsString, EntryKind)>> {
        let fd = rustix::fs::open(
            dir,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
        let mut stream = Dir::new(fd)?;
        let mut out = Vec::new();
        while let Some(entry) = stream.read() {
            let entry = entry?;
            let name = os_string(entry.file_name().to_bytes());
            // `.` and `..` are `readdir` mechanics, not entries: `std`'s
            // iterator drops them and every caller's listing is the
            // kernel's, which must see exactly what the native oracle
            // sees.
            if name == "." || name == ".." {
                continue;
            }
            let kind = match kind_of(entry.file_type()) {
                Some(kind) => kind,
                None => lstat_kind(dir, &name)?,
            };
            out.push((name, kind));
        }
        Ok(out)
    }
}

#[cfg(not(any(unix, target_os = "wasi")))]
mod lister {
    use std::ffi::OsString;
    use std::path::Path;

    use nml_validate::fs::EntryKind;

    pub(super) fn read_dir(dir: &Path) -> std::io::Result<Vec<(OsString, EntryKind)>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            out.push((
                entry.file_name(),
                if ft.is_symlink() {
                    EntryKind::Symlink
                } else if ft.is_dir() {
                    EntryKind::Dir
                } else if ft.is_file() {
                    EntryKind::File
                } else {
                    EntryKind::Other
                },
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use nml_validate::test_support::scan::blank_comments_and_strings;

    /// After a create or delete, some hosts report the directory's stamp
    /// a tick later than the child write returns. The memo keys on one
    /// `stat`, so wait briefly rather than flake on that ordering.
    fn wait_until_stamp_moves(dir: &Path, before: super::Stamp) {
        if super::stamp_of(dir) != Some(before) {
            return;
        }
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(5));
            if super::stamp_of(dir) != Some(before) {
                return;
            }
        }
        panic!("directory stamp did not move after a membership change (still {before:?})");
    }

    /// The lister agrees with the native oracle, entry for entry, over a
    /// tree that has one of every kind — the property the wasm editor's
    /// universe rests on (an entry this drops is a manifest that vanishes
    /// from discovery and a universe that reads as OPEN).
    #[test]
    fn the_lister_lists_exactly_what_the_native_oracle_lists() {
        let base = std::env::temp_dir().join(format!("nml-wasi-lister-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a-dir")).expect("scratch");
        std::fs::write(base.join("b-file"), b"x").expect("file");
        #[cfg(unix)]
        std::os::unix::fs::symlink("b-file", base.join("c-link")).expect("link");
        let mut mine = super::read_dir(&base).expect("lister");
        mine.sort();
        let native = nml_validate::fs::listing(std::fs::read_dir(&base)).expect("native");
        assert_eq!(mine, native, "the lister and the native oracle must agree");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The memo: a hit while the stamp holds, a refresh when it moves, and
    /// one instant per operation.
    #[test]
    fn the_memo_refreshes_on_a_stamp_move_and_freezes_within_one_operation() {
        let base = std::env::temp_dir().join(format!("nml-wasi-memo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        std::fs::write(base.join("one.nml"), b"x").expect("file");
        let memo = super::Listings::default();

        let op = memo.snapshot();
        assert_eq!(op(&base).expect("listed").len(), 1);
        let stamp_after_one = super::stamp_of(&base).expect("directory stamp");
        // Within ONE operation the answer is frozen: a file that appears
        // mid-walk cannot make the walk see two different trees.
        std::fs::write(base.join("two.nml"), b"y").expect("file");
        wait_until_stamp_moves(&base, stamp_after_one);
        assert_eq!(
            op(&base).expect("listed").len(),
            1,
            "frozen for this operation"
        );
        drop(op);

        // A NEW operation re-stats and sees the new entry.
        let op = memo.snapshot();
        assert_eq!(
            op(&base).expect("listed").len(),
            2,
            "a later operation is current"
        );
        drop(op);

        // And `clear` forgets everything a watched-file event invalidated
        // — asserted on what the memo HOLDS, because what it ANSWERS is
        // the same either way while the stamp still matches.
        assert_eq!(memo.held(), 1, "the memo holds the directory it listed");
        memo.clear();
        assert_eq!(memo.held(), 0, "clear() forgot nothing");
        let op = memo.snapshot();
        assert_eq!(op(&base).expect("listed").len(), 2);
        assert_eq!(memo.held(), 1, "…and the next operation listed it again");
        drop(op);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The staleness the memo tolerates, MEASURED rather than reasoned,
    /// and the one thing that closes it.
    ///
    /// The module doc claims a directory can only change under an
    /// unmoved stamp in ways a universe does not care about, because a
    /// `.nml` create or delete arrives separately as a watched-file event
    /// and [`super::Listings::clear`] drops the whole memo on one. This
    /// builds the window the claim rests on: a same-length rename leaves
    /// the directory's SIZE alone, and its mtime is put back by hand — so
    /// the stamp the memo reads is the one it already holds, while the
    /// entry it names is gone. That rename is a `.nml` delete and a
    /// `.nml` create in one, i.e. exactly the membership change the
    /// editor learns about from its watcher.
    ///
    /// Where the stamp moves anyway (a filesystem that sizes directories
    /// differently) the memo must be CURRENT instead: the rule under test
    /// is "the answer follows the stamp", asserted on both sides, so this
    /// case can never be red for the filesystem's reasons. The `clear`
    /// half is the same either way.
    #[cfg(unix)]
    #[test]
    fn a_membership_change_the_stamp_cannot_see_is_closed_by_the_watched_file_clear() {
        fn names(entries: Vec<std::io::Result<super::Entry>>) -> Vec<String> {
            let mut out: Vec<String> = entries
                .into_iter()
                .map(|e| e.expect("entry").0.to_string_lossy().into_owned())
                .collect();
            out.sort();
            out
        }

        let base = std::env::temp_dir().join(format!("nml-wasi-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        std::fs::write(base.join("one.nml"), b"x").expect("file");
        let before = std::fs::metadata(&base).expect("stat");
        let memo = super::Listings::default();

        let op = memo.snapshot();
        assert_eq!(names(op(&base).expect("listed")), ["one.nml"]);
        drop(op);
        assert_eq!(memo.held(), 1, "the listing was not held at all");

        std::fs::rename(base.join("one.nml"), base.join("uno.nml")).expect("rename");
        let dir = std::fs::File::open(&base).expect("open the directory");
        dir.set_times(std::fs::FileTimes::new().set_modified(before.modified().expect("mtime")))
            .expect("restore the directory's mtime");
        let after = std::fs::metadata(&base).expect("stat");
        let forged = after.len() == before.len() && after.modified().ok() == before.modified().ok();

        let op = memo.snapshot();
        let seen = names(op(&base).expect("listed"));
        drop(op);
        if forged {
            assert_eq!(
                seen,
                ["one.nml"],
                "the memo's answer must follow its stamp, and the stamp did not move"
            );
        } else {
            assert_eq!(seen, ["uno.nml"], "a moved stamp must be re-listed");
        }

        // What the editor does on a watched-file create or delete — and,
        // in the forged case above, the only thing that can.
        memo.clear();
        let op = memo.snapshot();
        assert_eq!(
            names(op(&base).expect("listed")),
            ["uno.nml"],
            "a cleared memo answered from a listing it was told to forget"
        );
        drop(op);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Source-level ratchet: EVERY directory listing in this crate goes
    /// through [`super::read_dir`]. A raw `std::fs::read_dir` compiles
    /// clean, passes every native test, and aborts the wasm server on the
    /// second listing of a workspace folder under `wasm-wasi-core` (its
    /// `fd_close` on a mount root's descriptor returns `EBADF` and std's
    /// `ReadDir` panics in `Drop`) — the exact failure that shipped as an
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
                let collapsed: String = blank_comments_and_strings(&text)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("");
                let scrubbed = collapsed.replace("wasi_fs::read_dir(", "\u{0}LEGAL\u{0}(");
                if scrubbed.contains("read_dir") {
                    for (i, line) in text.lines().enumerate() {
                        let code = blank_comments_and_strings(line);
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
             (std's panicking ReadDir Drop aborts the wasm server on the second \
             listing of a workspace folder under wasm-wasi-core):\n{}",
            offenders.join("\n")
        );
    }

    /// The scrubber itself — the one shared lexer
    /// (`nml_validate::test_support::scan`) under this ratchet's collapse:
    /// each bypass class the ratchet must catch, and the masking shapes
    /// it must NOT be fooled by.
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
            let collapsed: String = blank_comments_and_strings(src).split_whitespace().collect();
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
            let collapsed: String = blank_comments_and_strings(src).split_whitespace().collect();
            let scrubbed = collapsed.replace("wasi_fs::read_dir(", "\u{0}LEGAL\u{0}(");
            assert!(!scrubbed.contains("read_dir"), "must pass: {src}");
        }
    }
}
