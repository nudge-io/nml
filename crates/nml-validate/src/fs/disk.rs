//! The real filesystem oracle ([`StdFs`]) — the one place under
//! `workspace/` that calls `std::fs`. Its helpers (`kind_of`,
//! `absent_or_error`) are shared with the wasi backend, which `lstat`s
//! through `std` too.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::{EntryKind, FsError, Listing, LstatFs, PathFs, Step};

/// The real filesystem. Realpath is `dunce::canonicalize` on Windows (no
/// `\\?\` verbatim prefix — the LSP round-trips these paths through
/// URIs) and `std::fs::canonicalize` everywhere else; the two are
/// byte-identical off Windows.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdFs;

#[cfg(windows)]
fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    dunce::canonicalize(path)
}

#[cfg(not(windows))]
fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

pub(super) fn kind_of(ft: std::fs::FileType) -> EntryKind {
    if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

/// The typed reading of an `lstat`/`readdir` failure. Absence — of the
/// entry, or of a directory on the way to it — is `Ok(None)`; everything
/// else is a failure the caller must not read as absence.
pub(super) fn absent_or_error(e: std::io::Error) -> Result<Option<()>, FsError> {
    match e.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => Ok(None),
        _ => Err(fs_error(e)),
    }
}

fn fs_error(e: std::io::Error) -> FsError {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        FsError::Denied
    } else if is_symlink_loop(&e) {
        FsError::SymlinkLoop
    } else {
        FsError::Io(e.raw_os_error())
    }
}

/// ELOOP, by raw OS error: `ErrorKind::FilesystemLoop` is not stable on
/// the MSRV, and the number is per platform (Linux 40, the BSD family and
/// macOS 62, Windows `ERROR_CANT_RESOLVE_FILENAME` 1921).
fn is_symlink_loop(e: &std::io::Error) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const ELOOP: i32 = 40;
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    const ELOOP: i32 = 62;
    #[cfg(windows)]
    const ELOOP: i32 = 1921;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
        windows
    )))]
    const ELOOP: i32 = i32::MIN;
    e.raw_os_error() == Some(ELOOP)
}

impl LstatFs for StdFs {
    fn child(&self, dir: &Path, name: &OsStr) -> Result<Option<Step>, FsError> {
        let path = dir.join(name);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => return absent_or_error(e).map(|_| None),
        };
        let kind = kind_of(meta.file_type());
        if kind == EntryKind::Symlink {
            return Ok(Some(Step {
                spelling: name.to_os_string(),
                kind,
                spelling_verified: false,
            }));
        }
        // The on-disk spelling of an EXISTING non-symlink entry under a
        // canonical directory: realpath's last component. A realpath
        // failure on an entry lstat just saw leaves the spelling
        // unverified — closed universes then fail closed (form 2) rather
        // than trust the caller's case.
        Ok(Some(match canonicalize(&path) {
            Ok(canonical) => Step {
                spelling: canonical
                    .file_name()
                    .map(OsStr::to_os_string)
                    .unwrap_or_else(|| name.to_os_string()),
                kind,
                spelling_verified: true,
            },
            Err(_) => Step {
                spelling: name.to_os_string(),
                kind,
                spelling_verified: false,
            },
        }))
    }

    fn list_dir(&self, dir: &Path) -> Result<Vec<(OsString, EntryKind)>, FsError> {
        listing(std::fs::read_dir(dir))
    }
}

/// What a directory entry answers for the listing rule — `std`'s
/// [`std::fs::DirEntry`] does; a scripted entry in a test does too, so
/// the rule is pinned where no real filesystem fails per entry.
pub trait DirEntryLike {
    fn name(&self) -> OsString;
    /// The entry's kind — an `lstat` on a filesystem without `d_type`,
    /// which can fail (EACCES, EIO).
    fn kind(&self) -> std::io::Result<EntryKind>;
}

impl DirEntryLike for std::fs::DirEntry {
    fn name(&self) -> OsString {
        self.file_name()
    }

    fn kind(&self) -> std::io::Result<EntryKind> {
        self.file_type().map(kind_of)
    }
}

/// THE listing rule over an opened directory: the open's failure and
/// every entry's are typed as the kernel's ([`FsError`]), and one
/// unreadable entry refuses the whole listing (`collect_listing`).
/// Every backend that lists through `std` calls this — the native
/// [`StdFs`] and the wasm editor's abort-proof `read_dir` shim
/// ([`super::wasi_fs_through`]) — so no backend can skip an entry the
/// other refuses (the shim used to `filter_map` an unreadable kind away:
/// a manifest could vanish from discovery and the universe read as OPEN).
pub fn listing<E: DirEntryLike>(
    opened: std::io::Result<impl IntoIterator<Item = std::io::Result<E>>>,
) -> Listing {
    let entries = opened.map_err(fs_error)?;
    collect_listing(
        entries
            .into_iter()
            .map(|e| e.and_then(|e| Ok((e.name(), e.kind()?)))),
    )
}

/// The sorted listing of a directory's entries — REFUSED whole when any
/// entry cannot be read. `readdir` yields an entry whose `file_type`
/// may need an `lstat` (a filesystem without `d_type`), and that `lstat`
/// can fail (EACCES, EIO); an entry silently dropped would let a
/// manifest vanish from discovery and the universe read as OPEN — the
/// one fail-open shape everything else in the kernel avoids — so the
/// failure is the listing's (`FsError` → `Truncation::Unreadable`,
/// closed-denied, error severity). Pure over the entry iterator, so the
/// drop is pinned without a filesystem that fails per entry.
fn collect_listing(
    entries: impl Iterator<Item = std::io::Result<(OsString, EntryKind)>>,
) -> Result<Vec<(OsString, EntryKind)>, FsError> {
    let mut out = Vec::new();
    for entry in entries {
        out.push(entry.map_err(fs_error)?);
    }
    out.sort();
    Ok(out)
}

pub(super) fn resolve_symlink_component(
    dir: &Path,
    name: &OsStr,
) -> Result<Option<PathBuf>, FsError> {
    match canonicalize(&dir.join(name)) {
        Ok(target) => Ok(Some(target)),
        Err(e) => absent_or_error(e).map(|_| None),
    }
}

impl PathFs for StdFs {
    fn resolve_symlink(&self, dir: &Path, name: &OsStr) -> Result<Option<PathBuf>, FsError> {
        resolve_symlink_component(dir, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted entry: its name, and its kind or the `lstat` failure
    /// reading it.
    struct Scripted(&'static str, Option<EntryKind>);

    impl DirEntryLike for Scripted {
        fn name(&self) -> OsString {
            OsString::from(self.0)
        }

        fn kind(&self) -> std::io::Result<EntryKind> {
            self.1
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }
    }

    /// The rule every std-listing backend goes through ([`listing`]):
    /// an entry whose KIND cannot be read refuses the whole listing
    /// (`Denied`), an open that fails refuses it typed the same way, and
    /// a readable listing comes back sorted — so the wasm shim, which
    /// hands the kernel its opened `read_dir`, cannot drop an entry the
    /// native oracle refuses (a `filter_map(.. .ok()?)` in the shim is
    /// RED here).
    #[test]
    fn the_listing_rule_refuses_an_unreadable_entry_kind_on_every_backend() {
        let denied = listing(Ok(vec![
            Ok(Scripted("a", Some(EntryKind::File))),
            Ok(Scripted("zeta.package.nml", None)),
        ]));
        assert!(
            matches!(denied, Err(FsError::Denied)),
            "an entry whose kind cannot be read must refuse the listing, got {denied:?}"
        );
        let unopened = listing(Err::<Vec<std::io::Result<Scripted>>, _>(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        ));
        assert!(matches!(unopened, Err(FsError::Denied)), "{unopened:?}");
        let sorted = listing(Ok(vec![
            Ok(Scripted("b", Some(EntryKind::Dir))),
            Ok(Scripted("a", Some(EntryKind::Symlink))),
        ]))
        .expect("a readable listing");
        assert_eq!(
            sorted,
            vec![
                (OsString::from("a"), EntryKind::Symlink),
                (OsString::from("b"), EntryKind::Dir),
            ]
        );
    }

    /// A2: an entry whose kind cannot be read fails the LISTING — typed
    /// as the kernel's own error (EACCES → `Denied`, anything else →
    /// `Io`), never dropped: a `filter_map(Result::ok)` is RED here.
    #[test]
    fn a_failed_entry_fails_the_listing_instead_of_vanishing() {
        let ok = |n: &str, k: EntryKind| Ok((OsString::from(n), k));
        let listing = collect_listing(
            [
                ok("zeta.package.nml", EntryKind::File),
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                ok("alpha", EntryKind::Dir),
            ]
            .into_iter(),
        );
        assert!(
            matches!(listing, Err(FsError::Denied)),
            "a denied entry must refuse the listing, got {listing:?}"
        );
        let io = collect_listing(
            [
                ok("a", EntryKind::File),
                Err(std::io::Error::from_raw_os_error(5)),
            ]
            .into_iter(),
        );
        assert_eq!(io.unwrap_err(), FsError::Io(Some(5)));
        let sorted = collect_listing(
            [
                ok("zeta.package.nml", EntryKind::File),
                ok("alpha", EntryKind::Dir),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(
            sorted,
            vec![
                (OsString::from("alpha"), EntryKind::Dir),
                (OsString::from("zeta.package.nml"), EntryKind::File)
            ]
        );
    }
}
