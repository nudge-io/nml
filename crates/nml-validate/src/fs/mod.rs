//! The crate's ONE filesystem leaf (E28/E35), and Layer A's ONLY window on
//! the world (RFC 0019 item 0, A15′): the race-free `openat`-beneath chain,
//! the capped reader, the listing rule and the [`PathFs`]/[`LstatFs`]
//! oracle. It draws NO arrow to any other module, and three layers need it
//! — the workspace kernel, `package` (a package directory's manifest and
//! sources) and `store` (a slot pointer) — so it is the CRATE's leaf, not
//! the kernel's, published at its own path since apiVersion 5 (the module
//! whose subject is which binding governs a file no longer publishes
//! twenty-two filesystem names).
//!
//! Every filesystem observation the workspace kernel makes goes through
//! the injected [`PathFs`] oracle — a Layer-A function is a pure function
//! of its inputs and the oracle's answers, so the same code runs over the
//! real disk ([`StdFs`], `disk.rs`), the editor's unsaved buffers
//! ([`OverlayFs`], `overlay.rs`), the wasm editor's shimmed listings
//! ([`WasiFs`], `wasi.rs`) and a scripted tree with a probe log (the
//! `MockFs` behind `test-support`). This DIRECTORY is the one place in
//! the crate that holds ambient filesystem authority — a source ratchet
//! (`workspace::tests::ratchet::source_ratchet_workspace_has_no_ambient_fs`,
//! a `syn` walk) forbids it everywhere under `workspace/`, which no
//! longer contains this directory at all. It is the CRATE's leaf
//! (`src/fs/`): the workspace kernel, `package` and `store` all read
//! through it, and it draws no arrow back to any of them
//! (`tests/module_arrows.rs`), and it is published at its own path —
//! `nml_validate::fs::{read_beneath, read_leaf, …}` — for every front end.
//!
//! The oracle is ONE component primitive plus two helpers. [`LstatFs::child`]
//! is `lstat` first, then — only when the entry is not a symlink — the
//! platform realpath for the on-disk spelling (E25: `canonicalize` returns
//! the on-disk case/form of every existing component, so an existing
//! directory's key spelling is the filesystem's, never the caller's). A
//! symlink is reported as a symlink and its target is never touched by
//! `child`; [`PathFs::resolve_symlink`] exists for OPEN contexts only —
//! a closed universe halts at the symlink (E26) and never calls it.
//!
//! The oracle is split in two traits (E35): [`LstatFs`] is the
//! non-resolving half (`child`, `list_dir`) and [`PathFs`] adds the one
//! resolution. The closed policy walk and [`crate::workspace::Keyed::verify`] are
//! typed over `&dyn LstatFs`, so "a closed universe never resolves a
//! symlink" is a fact of the TYPE — the closed walk cannot name
//! `resolve_symlink` — and not only a probe-log invariant.
//!
//! The READ side lives here too (E35, the race-free read-through,
//! `beneath.rs`): [`read_beneath`] is THE bounded text read — every front
//! end's, one rule — opened through [`open_beneath`] beneath the canonical
//! root without ever following a link (the bytes read are the bytes of
//! the entry the walk classified), capped, UTF-8; [`read_leaf`] is the
//! same read anchored at a file's own parent, for a read with no root in
//! hand. There is no platform arm inside the reader: the chain runs
//! everywhere `openat` does, wasi included. [`write_beneath`] is the
//! fixer's handle-anchored atomic replace. All are `std`-free of ambient
//! paths below the root.

mod beneath;
mod disk;
mod overlay;
mod wasi;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub use beneath::open_beneath;
#[cfg(unix)]
pub use beneath::write_beneath;
pub use disk::{DirEntryLike, StdFs, listing};
pub use overlay::OverlayFs;
pub use wasi::{WasiFs, wasi_fs_through};

/// A byte count as humans read caps: `256 KiB`, `4 MiB`, `17 MiB`; a
/// size that is no whole MiB prints in KiB, and under a KiB in bytes.
pub fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB && n % GIB == 0 {
        format!("{} GiB", n / GIB)
    } else if n >= MIB && n % MIB == 0 {
        format!("{} MiB", n / MIB)
    } else if n >= KIB {
        format!("{} KiB", n / KIB)
    } else {
        format!("{n} bytes")
    }
}

/// The bound one live package manifest or project config is read under
/// ([`crate::workspace::input_cap`]): a declaration list, re-parsed on
/// every invocation of every verb. Past it the input is refused
/// (NML2088), never held.
///
/// Declared HERE, in the reader's own leaf, because three layers read a
/// manifest — the workspace walk, `package` (a package DIRECTORY's
/// manifest) and `store` — and a bound only one of them could name
/// would be re-spelled by the other two. [`too_large`] still declares
/// none of its own: the cap is its argument.
///
/// LIMIT: reach=content guards=memory surface=kernel shown="256 KiB" — bytes of one package manifest or project config read (past it the input is refused, NML2088)
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// The bound one schema source is read under
/// ([`crate::workspace::input_cap`]): what a schema file is allowed.
/// Past it the input is refused (NML2088), never held. Every door into
/// a schema source reads under it — a manifest's declaration, a package
/// directory's, and the CLI's `--schema` — so one file is refused at
/// the same size however it is reached.
///
/// LIMIT: reach=content guards=memory surface=kernel shown="4 MiB" — bytes of one schema source read (past it the input is refused, NML2088)
pub const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;

/// The unit a cap is stated in: the largest of GiB/MiB/KiB that divides
/// it exactly, matching what [`human_bytes`] chooses for it, and plain
/// bytes for a cap smaller than a KiB.
fn cap_unit(cap: u64) -> (u64, &'static str) {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if cap >= GIB && cap % GIB == 0 {
        (GIB, "GiB")
    } else if cap >= MIB && cap % MIB == 0 {
        (MIB, "MiB")
    } else if cap >= KIB && cap % KIB == 0 {
        (KIB, "KiB")
    } else {
        (1, "bytes")
    }
}

/// A measured size, rendered so it can be COMPARED WITH ITS CAP by eye.
///
/// [`human_bytes`] names a size in the largest unit that divides it
/// exactly. That is right for a cap — every cap here is a whole unit —
/// and wrong for a size somebody's file happens to be: one byte past a
/// 256 KiB bound is no whole KiB either way, but one byte past a 4 MiB
/// bound is no whole MiB, so it rendered as `4096 KiB` beside a cap of
/// `4 MiB`, and a file one byte over a 256 KiB bound rendered as
/// `256 KiB` beside `256 KiB` — the two halves of one comparison in two
/// different units, or in the same number. Both read as a refusal of a
/// file that is exactly at the bound, which is the one thing the sentence
/// must not say. The size is stated in the CAP's unit instead, marked
/// `over` when the division is not exact; the exact byte counts, which
/// were always there, settle the rest.
fn size_against(actual: u64, cap: usize) -> String {
    let (unit, name) = cap_unit(cap as u64);
    if unit == 1 {
        return format!("{actual} bytes ({actual} bytes)");
    }
    let whole = actual / unit;
    let over = if actual % unit == 0 { "" } else { "over " };
    format!("{over}{whole} {name} ({actual} bytes)")
}

/// The ONE sentence for an input past its cap — `too large: 5 MiB
/// (5242880 bytes) — a declared schema source is read only up to 4 MiB
/// (4194304 bytes)` — spoken by every front end (the CLI's readers and
/// the editor's, disk or buffer), so the same file is refused in the
/// same words wherever it is met. `actual` is the input's size when
/// known, rendered in the CAP's own unit so the two numbers can be
/// compared by eye; `what` is the noun the bound applies to
/// (`a package manifest`, `a check target`). It
/// lives HERE, with the capped reader that speaks it first: the cap is an
/// ARGUMENT, so the sentence declares no bound of its own, and `fs` no
/// longer reaches up into the walk that reads it.
pub fn too_large(actual: Option<u64>, cap: usize, what: &str) -> String {
    let actual = actual
        .map(|s| size_against(s, cap))
        .unwrap_or_else(|| format!("more than {cap} bytes"));
    format!(
        "too large: {actual} — {what} is read only up to {} ({cap} bytes)",
        human_bytes(cap as u64)
    )
}

/// What a directory listing answers: its entries, sorted, each with its
/// kind — or the kernel's typed failure, for the whole listing.
pub type Listing = Result<Vec<(OsString, EntryKind)>, FsError>;

/// Read at most `cap` bytes from `reader` — `cap + 1` are taken, so an
/// oversized input is refused without being read in, in the kernel's one
/// sentence ([`too_large`]) — the byte half of [`read_beneath`]. Here,
/// under `fs/`, because it is the one place in the kernel that touches
/// `std::io` (the source ratchet keeps ambient authority out of every
/// other kernel file).
fn read_capped<R: std::io::Read>(
    reader: R,
    size: Option<u64>,
    cap: usize,
    what: &str,
) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    reader
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > cap {
        return Err(too_large(size, cap, what));
    }
    Ok(bytes)
}

/// Why the one reader ([`read_beneath`], [`read_leaf`]) refused. The open's
/// refusal is TYPED — a front end that speaks its own sentence for a link
/// met at the leaf matches it — and displays as a path-based open would
/// have spelled it ([`OpenError::into_io`]), so a front end's message is
/// byte-identical to the classify-then-open one wherever no race was
/// detected; the cap and a mid-read failure speak the kernel's sentence;
/// bytes that are not UTF-8 are refused, never decoded lossily.
#[derive(Debug)]
pub enum ReadError {
    /// The open refused, typed.
    Open(OpenError),
    /// Past `cap` ([`too_large`]), or unreadable mid-read in the OS's
    /// words.
    Refused(String),
    NotUtf8,
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The two shapes a path-based open reports in the OS's words
            // (`into_io`'s raw errno); every other in the chain's own.
            Self::Open(OpenError::NotADirectory { .. }) => write!(f, "{}", not_a_directory()),
            Self::Open(OpenError::NotRegular { dir: true, .. }) => {
                write!(f, "{}", is_a_directory())
            }
            Self::Open(e) => write!(f, "{e}"),
            Self::Refused(sentence) => f.write_str(sentence),
            Self::NotUtf8 => f.write_str("not UTF-8"),
        }
    }
}

impl std::error::Error for ReadError {}

/// THE bounded text read (E35 + E28), every front end's: `components` (a
/// key's plain names) beneath the canonical `root`, opened through the
/// race-free chain ([`open_beneath`] — a directory swapped for a link
/// between the walk's `lstat` and this open is refused AT the open, never
/// followed; a FIFO or device is never blocked on), at most `cap` bytes
/// (`cap + 1` are taken, so an oversized input is refused in the kernel's
/// one sentence without being read in; `what` names what the bound
/// applies to), UTF-8. The CLI's discovery and target reads and the
/// editor's discovery reads all come here, so the two front ends cannot
/// read one file differently. The one platform arm is inside
/// (`open_input`).
pub fn read_beneath(
    root: &Path,
    components: &[&str],
    cap: usize,
    what: &str,
) -> Result<String, ReadError> {
    let file = open_input(root, components).map_err(ReadError::Open)?;
    let size = file.metadata().map(|m| m.len()).ok();
    let bytes = read_capped(file, size, cap, what).map_err(ReadError::Refused)?;
    String::from_utf8(bytes).map_err(|_| ReadError::NotUtf8)
}

/// [`read_beneath`] anchored at `path`'s own parent — the read for a file
/// with no workspace root in hand (the editor's index and note reads,
/// the CLI's bare `parse`/`fmt` target): the parent is the caller's
/// spelling, opened by path (links in it are followed, as a by-path open
/// followed them), and the LEAF is opened `O_NOFOLLOW | O_NONBLOCK` and
/// `fstat`-checked — a leaf swapped for a link, a FIFO or a directory
/// between a classification and this read is refused, never followed or
/// blocked on. A spelling with no file name names no file.
pub fn read_leaf(path: &Path, cap: usize, what: &str) -> Result<String, ReadError> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Err(ReadError::Open(OpenError::NotRegular {
            component: String::new(),
            dir: true,
        }));
    };
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    read_beneath(parent, &[name], cap, what)
}

/// The reader's open: the race-free chain, on every platform that has
/// `openat` — wasi included. The wasm editor once opened BY PATH here,
/// on the theory that VS Code's `wasm-wasi-core` could not survive
/// per-component directory descriptors; the host was then measured and
/// the theory was wrong (the chain runs there, and its `OwnedFd`s ignore
/// the failing close that aborts std's `ReadDir`), so the arm is gone and
/// a swapped parent is refused in the editor exactly as on the CLI.
fn open_input(root: &Path, components: &[&str]) -> Result<std::fs::File, OpenError> {
    open_beneath(root, components)
}

/// What one directory entry is, by `lstat` (a symlink is a symlink, never
/// its target).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    /// A device, socket, FIFO — exists, is neither file nor directory.
    Other,
}

/// Typed oracle failures. Absence is NOT an error (`Ok(None)` from
/// [`LstatFs::child`]) — the split that keeps the key pipeline honest about
/// what it can and cannot establish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    /// Realpath is unavailable on this backend (wasi). Open contexts fall
    /// back to lexical keys marked unverifiable; closed bindings fail
    /// closed (NML2083 form 2).
    NoRealpath,
    /// EACCES on an ancestor: neither identity nor existence is
    /// establishable — fail closed, never "absent".
    Denied,
    /// ELOOP while resolving a symlink chain.
    SymlinkLoop,
    /// Any other I/O failure, with the raw OS error when there is one.
    Io(Option<i32>),
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRealpath => {
                f.write_str("the filesystem backend cannot verify on-disk spelling")
            }
            Self::Denied => f.write_str("permission denied on a path component"),
            Self::SymlinkLoop => f.write_str("symlink loop"),
            Self::Io(Some(code)) => write!(f, "I/O error (os error {code})"),
            Self::Io(None) => f.write_str("I/O error"),
        }
    }
}

impl std::error::Error for FsError {}

/// One resolved path component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The on-disk spelling when `spelling_verified`, else the lookup name
    /// as given.
    pub spelling: OsString,
    pub kind: EntryKind,
    /// Whether `spelling` is the filesystem's own directory-entry spelling.
    /// Always `false` for a symlink (its name is never a surviving key
    /// component: a closed universe halts on it, an open one replaces it
    /// with the target) and for a backend that cannot verify.
    pub spelling_verified: bool,
}

/// The NON-RESOLVING half of the oracle (E35): `lstat` and listings
/// only. A closed-trust walk is typed over `&dyn LstatFs` (a `&dyn
/// PathFs` upcasts to it — trait upcasting is stable since 1.86, the
/// MSRV), so it has no way to resolve a link.
pub trait LstatFs {
    /// One component under a CANONICAL `dir`: `lstat(dir/name)`, then, if
    /// the entry is not a symlink, the on-disk spelling. `Ok(None)` = no
    /// such entry (including a parent that is not a directory).
    fn child(&self, dir: &Path, name: &OsStr) -> Result<Option<Step>, FsError>;
    /// The entries of `dir` — names with their `lstat` kinds — in a
    /// deterministic (sorted) order.
    fn list_dir(&self, dir: &Path) -> Result<Vec<(OsString, EntryKind)>, FsError>;
}

/// The filesystem oracle (A15′): the non-resolving half plus the one
/// open-context-only symlink resolution.
pub trait PathFs: LstatFs {
    /// Resolve the symlink `dir/name` to its canonical target. OPEN
    /// contexts only — closed universes never call this (E26: a symlink
    /// component halts the walk before its target is ever resolved).
    /// `Ok(None)` = dangling.
    fn resolve_symlink(&self, dir: &Path, name: &OsStr) -> Result<Option<PathBuf>, FsError>;
}

// ─────────────────────────────── the race-free read-through (E35) ──

/// Why an open beneath the root refused. [`OpenError::into_io`] maps the
/// shapes a path-based open would also have produced — absent, a
/// directory, not a directory — to the SAME `io::Error` (the raw OS
/// errno), so a caller's message is byte-identical to the classify-
/// then-open one wherever no race was detected; the two shapes only the
/// chain can produce (a symlink met at the open, a non-regular leaf)
/// carry their own sentence.
#[derive(Debug)]
pub enum OpenError {
    /// `component` is a symlink NOW (whatever the walk saw): refused at
    /// the open, never followed — identically whether or not its target
    /// exists.
    Symlink {
        component: String,
    },
    /// A non-leaf `component` is not a directory now.
    NotADirectory {
        component: String,
    },
    /// The leaf is not a regular file: a directory (`dir`), or a FIFO, a
    /// device, a socket — refused before any read could block on it.
    NotRegular {
        component: String,
        dir: bool,
    },
    Io(std::io::Error),
}

impl OpenError {
    pub fn into_io(self) -> std::io::Error {
        match self {
            Self::NotADirectory { .. } => not_a_directory(),
            Self::NotRegular { dir: true, .. } => is_a_directory(),
            Self::Io(e) => e,
            // A symlink met at the open, a non-regular leaf: only the
            // chain produces these, and their sentence is their `Display`.
            other => std::io::Error::other(other.to_string()),
        }
    }
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Symlink { component } => {
                write!(
                    f,
                    "path component `{component}` is a symlink (refused at open)"
                )
            }
            Self::NotADirectory { component } => {
                write!(f, "path component `{component}` is not a directory")
            }
            Self::NotRegular { component, dir } => {
                if *dir {
                    write!(f, "`{component}` is a directory")
                } else {
                    write!(f, "`{component}` is not a regular file (refused at open)")
                }
            }
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OpenError {}

#[cfg(any(unix, target_os = "wasi"))]
fn not_a_directory() -> std::io::Error {
    std::io::Error::from(rustix::io::Errno::NOTDIR)
}

#[cfg(any(unix, target_os = "wasi"))]
fn is_a_directory() -> std::io::Error {
    std::io::Error::from(rustix::io::Errno::ISDIR)
}

#[cfg(not(any(unix, target_os = "wasi")))]
fn not_a_directory() -> std::io::Error {
    std::io::Error::from(std::io::ErrorKind::NotADirectory)
}

#[cfg(not(any(unix, target_os = "wasi")))]
fn is_a_directory() -> std::io::Error {
    std::io::Error::from(std::io::ErrorKind::IsADirectory)
}

/// A component the read-through will hand to the OS: a plain name only.
/// A `SourceKey` never carries anything else; this is the read-through's
/// OWN fence — `..` handed to `openat` walks UP (`RESOLVE_BENEATH`
/// refuses it; the portable chain must refuse it itself), so the chain
/// never trusts its caller.
fn plain(name: &str) -> Result<&str, OpenError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(OpenError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("`{name}` is not a plain path component"),
        )));
    }
    Ok(name)
}

/// The leaf and the parents of a non-empty component list.
fn split_leaf<'a>(components: &'a [&'a str]) -> Result<(&'a str, &'a [&'a str]), OpenError> {
    for name in components {
        plain(name)?;
    }
    components
        .split_last()
        .map(|(l, p)| (*l, p))
        .ok_or(OpenError::NotRegular {
            component: String::new(),
            dir: true,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The read-through's OWN fence (r80-sec mutant m03 survived
    /// without this pin): `..`, `.`, the empty name and a separator-
    /// bearing name never reach `openat` — whatever the caller minted.
    #[test]
    fn the_chain_refuses_every_non_plain_component_itself() {
        for bad in ["..", ".", "", "a/b", "a\\b"] {
            let err = plain(bad).expect_err(bad);
            assert!(
                matches!(&err, OpenError::Io(e) if e.kind() == std::io::ErrorKind::InvalidInput),
                "{bad:?}: {err}"
            );
            assert!(
                split_leaf(&["t", bad, "x.nml"]).is_err(),
                "{bad:?} must be refused wherever it sits"
            );
            assert!(split_leaf(&[bad]).is_err(), "{bad:?} as the leaf");
        }
        assert_eq!(plain("x.nml").unwrap(), "x.nml");
        let (leaf, parents) = split_leaf(&["tenants", "cu", "x.nml"]).unwrap();
        assert_eq!((leaf, parents), ("x.nml", &["tenants", "cu"][..]));
        assert!(split_leaf(&[]).is_err(), "no components names no file");
    }

    /// A read refusal displays EXACTLY as the path-based open's error
    /// would have (`into_io`), shape by shape — the byte-identity every
    /// front end's sentence rests on — and the two non-open refusals
    /// speak the kernel's words.
    #[test]
    fn a_read_refusal_displays_as_the_path_based_open_would() {
        let shapes = || {
            vec![
                OpenError::Symlink {
                    component: "cu".to_string(),
                },
                OpenError::NotADirectory {
                    component: "cu".to_string(),
                },
                OpenError::NotRegular {
                    component: "d".to_string(),
                    dir: true,
                },
                OpenError::NotRegular {
                    component: "p".to_string(),
                    dir: false,
                },
                OpenError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)),
            ]
        };
        for (typed, via_io) in shapes().into_iter().zip(shapes()) {
            let expect = via_io.into_io().to_string();
            assert_eq!(ReadError::Open(typed).to_string(), expect);
        }
        assert_eq!(
            ReadError::Open(OpenError::Symlink {
                component: "cu".to_string()
            })
            .to_string(),
            "path component `cu` is a symlink (refused at open)"
        );
        assert_eq!(
            ReadError::Open(OpenError::NotRegular {
                component: "p".to_string(),
                dir: false
            })
            .to_string(),
            "`p` is not a regular file (refused at open)"
        );
        assert_eq!(ReadError::NotUtf8.to_string(), "not UTF-8");
        assert_eq!(ReadError::Refused("x".to_string()).to_string(), "x");
    }

    /// The cap sentence must READ as a comparison: both halves in one
    /// unit, and a size that is not a whole one marked `over`. The two
    /// regressions this pins are the ones a real file hits — a size one
    /// byte past a MiB bound used to render in KiB beside a MiB cap
    /// (`4096 KiB … up to 4 MiB`), and a size one byte past a KiB bound
    /// used to render as the SAME number as the cap (`256 KiB … up to
    /// 256 KiB`), which reads as refusing a file that fits.
    #[test]
    fn the_cap_sentence_states_the_size_in_the_cap_s_own_unit() {
        const KIB: usize = 1024;
        const MIB: usize = 1024 * KIB;
        let say = |actual: u64, cap: usize| too_large(Some(actual), cap, "a package manifest");

        assert_eq!(
            say(4 * MIB as u64 + 1, 4 * MIB),
            "too large: over 4 MiB (4194305 bytes) — a package manifest is read only up to \
             4 MiB (4194304 bytes)"
        );
        assert_eq!(
            say(256 * KIB as u64 + 1, 256 * KIB),
            "too large: over 256 KiB (262145 bytes) — a package manifest is read only up to \
             256 KiB (262144 bytes)"
        );
        // A whole multiple of the cap\'s unit keeps the plain form.
        assert_eq!(
            say(300 * KIB as u64, 256 * KIB),
            "too large: 300 KiB (307200 bytes) — a package manifest is read only up to \
             256 KiB (262144 bytes)"
        );
        assert_eq!(
            say(5 * MIB as u64, 4 * MIB),
            "too large: 5 MiB (5242880 bytes) — a package manifest is read only up to \
             4 MiB (4194304 bytes)"
        );
        // A cap smaller than a KiB is stated in bytes, and so is the size.
        assert_eq!(
            say(65, 64),
            "too large: 65 bytes (65 bytes) — a package manifest is read only up to 64 bytes \
             (64 bytes)"
        );
        // Every rendered size is >= the cap\'s own rendering, so no
        // sentence can ever read as refusing something under the bound.
        for cap in [64usize, 4 * KIB, 256 * KIB, 4 * MIB, 16 * MIB] {
            for extra in [1u64, 7, 1023, 1024, 4096] {
                let m = say(cap as u64 + extra, cap);
                let bound = format!("up to {}", human_bytes(cap as u64));
                let size = m
                    .strip_prefix("too large: ")
                    .and_then(|r| r.split_once(" \u{2014} "))
                    .map(|(size, _)| size.to_string())
                    .expect("the sentence has both halves");
                let same_number = size.starts_with(&format!("{} ", human_bytes(cap as u64)));
                assert!(
                    !same_number,
                    "cap {cap}, +{extra}: the size reads as the bound itself ({size}, {bound})"
                );
            }
        }
    }
}
