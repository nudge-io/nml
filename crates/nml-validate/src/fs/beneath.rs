//! The race-free read-through (E35): [`open_beneath`] on every platform
//! (the `openat(O_NOFOLLOW)` chain on unix and wasi, the documented
//! classify-then-open shape elsewhere) and the fixer's handle-anchored
//! [`write_beneath`] (unix).

#[cfg(not(any(unix, target_os = "wasi")))]
use std::path::Path;

#[cfg(not(any(unix, target_os = "wasi")))]
use super::{OpenError, split_leaf};

#[cfg(any(unix, target_os = "wasi"))]
pub use chain::open_beneath;
#[cfg(unix)]
pub use chain::write_beneath;

/// Open `components` (a key's plain names) beneath the canonical `root`
/// as a regular file for reading, following NO symlink below the root.
/// Unix and wasi: the [`beneath`] chain. Any other platform (Windows):
/// the classify-then-open shape — the leaf is opened by path and its
/// kind checked afterwards; a same-host racer swapping a directory for
/// a link between the walk and the open is NOT closed there (E23's
/// boundary stands on that lane; an `NtCreateFile`-with-`RootDirectory`
/// backend is the recorded next step).
#[cfg(not(any(unix, target_os = "wasi")))]
pub fn open_beneath(root: &Path, components: &[&str]) -> Result<std::fs::File, OpenError> {
    let (leaf, _) = split_leaf(components)?;
    let mut path = root.to_path_buf();
    for name in components {
        path.push(name);
    }
    let file = std::fs::File::open(&path).map_err(OpenError::Io)?;
    let kind = file.metadata().map_err(OpenError::Io)?.file_type();
    if kind.is_file() {
        return Ok(file);
    }
    Err(OpenError::NotRegular {
        component: leaf.to_string(),
        dir: kind.is_dir(),
    })
}

/// The race-free read-through (unix, wasi) and the handle-anchored write
/// (unix).
///
/// The root is opened once by path (it is the operator's canonical
/// directory); every component below is `openat(dirfd, name, O_RDONLY |
/// O_CLOEXEC | O_NOFOLLOW | O_DIRECTORY)` and the leaf `O_NOFOLLOW |
/// O_NOCTTY | O_NONBLOCK`, then `fstat` must say regular file — a
/// directory swapped for a symlink between the kernel's `lstat` and the
/// open is refused AT the open, never followed, and a FIFO or device is
/// never blocked on. On Linux one `openat2(RESOLVE_BENEATH |
/// RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)` is tried first (one
/// syscall, kernel-atomic); the chain is the fallback (pre-5.6 kernels,
/// seccomp) and the ATTRIBUTOR (it names the component that refused).
/// On wasi the same chain runs through wasi-libc: each
/// `openat(O_NOFOLLOW)` is a `path_open` with `lookupflags = 0` (no
/// `SYMLINK_FOLLOW`), and the host confines every step to the preopen;
/// `std`'s own `lookup_flags(0)` is unstable (`wasi_ext`), so it is not
/// used. Safe bindings only (`rustix`): the crate forbids `unsafe`.
#[cfg(any(unix, target_os = "wasi"))]
mod chain {
    use std::fs::File;
    use std::os::fd::OwnedFd;
    use std::path::Path;

    use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawMode, fstat, openat, statat};
    use rustix::io::Errno;

    use crate::fs::{OpenError, split_leaf};

    fn dir_flags() -> OFlags {
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY
    }

    fn leaf_flags() -> OFlags {
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::NONBLOCK
    }

    fn io(e: Errno) -> OpenError {
        OpenError::Io(std::io::Error::from(e))
    }

    fn file_type(mode: RawMode) -> FileType {
        FileType::from_raw_mode(mode)
    }

    /// The root directory's handle: opened by path, symlinks in the
    /// operator's canonical spelling followed (there are none).
    fn open_root(root: &Path) -> Result<OwnedFd, OpenError> {
        rustix::fs::open(
            root,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .map_err(io)
    }

    /// The parents' chain: the handle of the directory holding the leaf.
    fn open_parents(root: &Path, parents: &[&str]) -> Result<OwnedFd, OpenError> {
        let mut dir = open_root(root)?;
        for name in parents {
            let next = match openat(&dir, *name, dir_flags(), Mode::empty()) {
                Ok(fd) => fd,
                Err(e) => return Err(refusal(&dir, name, e)),
            };
            dir = next;
        }
        Ok(dir)
    }

    /// The opened leaf must be a regular file.
    fn leaf(fd: OwnedFd, component: &str) -> Result<File, OpenError> {
        let st = fstat(&fd).map_err(io)?;
        let mode: RawMode = st.st_mode;
        match file_type(mode) {
            FileType::RegularFile => Ok(File::from(fd)),
            FileType::Directory => Err(OpenError::NotRegular {
                component: component.to_string(),
                dir: true,
            }),
            _ => Err(OpenError::NotRegular {
                component: component.to_string(),
                dir: false,
            }),
        }
    }

    /// Linux fast path: one `openat2` beneath `root`, symlinks and magic
    /// links refused by the kernel atomically. `None` = unavailable (a
    /// pre-5.6 kernel, seccomp) OR refused — either way the chain runs
    /// and attributes the refusal to a component.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn openat2_beneath(root: &OwnedFd, rel: &str) -> Option<OwnedFd> {
        use rustix::fs::{ResolveFlags, openat2};
        openat2(
            root,
            rel,
            leaf_flags(),
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        )
        .ok()
    }

    /// Open `components` (a key's plain names) beneath the canonical
    /// `root` as a regular file for reading; no symlink below the root is
    /// ever followed.
    pub fn open_beneath(root: &Path, components: &[&str]) -> Result<File, OpenError> {
        let (last, parents) = split_leaf(components)?;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let dir = open_root(root)?;
            if let Some(fd) = openat2_beneath(&dir, &components.join("/")) {
                return leaf(fd, last);
            }
        }
        let dir = open_parents(root, parents)?;
        match openat(&dir, last, leaf_flags(), Mode::empty()) {
            Ok(fd) => leaf(fd, last),
            Err(e) => Err(refusal(&dir, last, e)),
        }
    }

    /// `ELOOP` (macOS, the Linux leaf) / `EMLINK` (FreeBSD) from
    /// `O_NOFOLLOW` is "this component is a symlink". `ENOTDIR` from
    /// `O_DIRECTORY` is "not a directory" — but Linux reports a symlink
    /// component under `O_NOFOLLOW | O_DIRECTORY` as `ENOTDIR` too, so
    /// the attribution asks `fstatat(AT_SYMLINK_NOFOLLOW)` which it is.
    /// Attribution only: the refusal already happened, and it happened
    /// identically whether the link's target exists or not. The `statat`
    /// is a SECOND lookup: an entry swapped again between the failed
    /// `openat` and it can be labelled "not a directory" where the truth
    /// was a link, or vice versa — the message may mislabel under a
    /// second swap, the verdict cannot (`refusal` never returns `Ok`).
    fn refusal(dir: &OwnedFd, name: &str, e: Errno) -> OpenError {
        match e {
            Errno::LOOP | Errno::MLINK => OpenError::Symlink {
                component: name.to_string(),
            },
            Errno::NOTDIR => {
                let is_link = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|st| {
                    let mode: RawMode = st.st_mode;
                    file_type(mode) == FileType::Symlink
                });
                if is_link {
                    OpenError::Symlink {
                        component: name.to_string(),
                    }
                } else {
                    OpenError::NotADirectory {
                        component: name.to_string(),
                    }
                }
            }
            other => io(other),
        }
    }

    #[cfg(unix)]
    fn write_all(fd: &OwnedFd, mut bytes: &[u8]) -> Result<(), Errno> {
        while !bytes.is_empty() {
            match rustix::io::write(fd, bytes) {
                Ok(0) => return Err(Errno::IO),
                Ok(n) => bytes = &bytes[n..],
                Err(Errno::INTR) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// The fixer's write (E35, sec 1b): replace the regular file at
    /// `components` beneath `root` with `contents`, atomically, through
    /// the SAME chain the read used — the temp file is created with
    /// `openat(parent_fd, O_CREAT | O_EXCL | O_NOFOLLOW)` and moved into
    /// place with `renameat(parent_fd, tmp, parent_fd, leaf)`, so a
    /// parent swapped for a symlink after classification can never
    /// redirect the write outside the root. The original's permission
    /// bits are preserved (read by handle, never followed); an absent
    /// original keeps the create default; a leaf that is a link or not a
    /// regular file is refused before anything is written. The temp name
    /// carries this process's pid; a stale one left by a crashed run
    /// under a reused pid is unlinked (never followed, never opened)
    /// before the `O_EXCL` create, so it can never surface as `EEXIST`.
    /// The temp name is `.{leaf}.tmp-{pid}`, and that pre-create unlink
    /// assumes ONE writer per pid per filesystem — true for the sole,
    /// sequential caller, `cmd_fix`; a concurrent or parallel caller
    /// must move to a per-call unique suffix with an `EEXIST` retry and
    /// no unlink of foreign names.
    #[cfg(unix)]
    pub fn write_beneath(
        root: &Path,
        components: &[&str],
        contents: &[u8],
    ) -> Result<(), OpenError> {
        let (last, parents) = split_leaf(components)?;
        let dir = open_parents(root, parents)?;
        let mode = match openat(&dir, last, leaf_flags(), Mode::empty()) {
            Ok(fd) => {
                let st = fstat(&fd).map_err(io)?;
                let raw: RawMode = st.st_mode;
                match file_type(raw) {
                    // Permission bits only: the replacement is a
                    // NEW inode owned by the fixer's user, so a set-uid,
                    // set-gid or sticky bit an author put on the original
                    // must not be minted onto the operator's file.
                    FileType::RegularFile => Some(Mode::from_raw_mode(raw & 0o777)),
                    FileType::Directory => {
                        return Err(OpenError::NotRegular {
                            component: last.to_string(),
                            dir: true,
                        });
                    }
                    _ => {
                        return Err(OpenError::NotRegular {
                            component: last.to_string(),
                            dir: false,
                        });
                    }
                }
            }
            Err(Errno::NOENT) => None,
            Err(e) => return Err(refusal(&dir, last, e)),
        };
        let tmp = format!(".{last}.tmp-{}", std::process::id());
        let _ = rustix::fs::unlinkat(&dir, tmp.as_str(), AtFlags::empty());
        // Created AT the original's permission bits (under the umask), so
        // a `0600` file's content is never world-readable in its own
        // directory for the duration of the write; the `fchmod` after
        // the write still applies the bits exactly (the create honours
        // the umask, the chmod does not). An absent original is created
        // at the default `0o666 & ~umask`.
        let fd = openat(
            &dir,
            tmp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            mode.unwrap_or(Mode::from_raw_mode(0o666)),
        )
        .map_err(io)?;
        let written = write_all(&fd, contents).and_then(|()| match mode {
            Some(mode) => rustix::fs::fchmod(&fd, mode),
            None => Ok(()),
        });
        drop(fd);
        let result = written.and_then(|()| rustix::fs::renameat(&dir, tmp.as_str(), &dir, last));
        if let Err(e) = result {
            let _ = rustix::fs::unlinkat(&dir, tmp.as_str(), AtFlags::empty());
            return Err(io(e));
        }
        Ok(())
    }
}
