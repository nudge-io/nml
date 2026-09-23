//! The editor's oracle ([`OverlayFs`]): unsaved buffers over a disk
//! backend (RFC 0019 item 0, step 0e).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::{EntryKind, FsError, LstatFs, PathFs, Step};

/// The editor's view: unsaved buffers exist as files over the disk
/// (RFC 0019 item 0, step 0e). The disk is consulted FIRST — an entry the
/// disk has keeps the disk's kind and spelling (a buffer opened through a
/// link is still a link, E26) — and buffers fill in what the disk lacks:
/// a buffer at a path whose directory does not exist on disk makes that
/// directory exist as a `Dir` for the overlay, so `child` walks to it and
/// `verify` finds the buffer. A listing the disk refuses is refused here
/// too: an unreadable directory must never read as "just buffers" (a
/// truncation would go unseen); only a directory ABSENT on disk lists its
/// buffers alone.
pub struct OverlayFs<'a, F: PathFs> {
    pub disk: &'a F,
    /// Absolute paths of the open buffers.
    pub buffers: &'a [PathBuf],
}

impl<F: PathFs> OverlayFs<'_, F> {
    /// The entries the buffers imply directly under `dir`: a buffer there
    /// is a file, a buffer deeper down implies a directory.
    fn buffered(&self, dir: &Path) -> Vec<(OsString, EntryKind)> {
        let mut out: Vec<(OsString, EntryKind)> = Vec::new();
        for buffer in self.buffers {
            let Ok(rel) = buffer.strip_prefix(dir) else {
                continue;
            };
            let mut components = rel.components();
            let Some(std::path::Component::Normal(first)) = components.next() else {
                continue;
            };
            let kind = if components.next().is_some() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            let entry = (first.to_os_string(), kind);
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
        out
    }

    /// The disk's kind for `dir` itself (`None` = absent on disk).
    fn disk_kind(&self, dir: &Path) -> Result<Option<EntryKind>, FsError> {
        match (dir.parent(), dir.file_name()) {
            (Some(parent), Some(name)) => Ok(self.disk.child(parent, name)?.map(|s| s.kind)),
            _ => Ok(Some(EntryKind::Dir)),
        }
    }
}

impl<F: PathFs> LstatFs for OverlayFs<'_, F> {
    fn child(&self, dir: &Path, name: &OsStr) -> Result<Option<Step>, FsError> {
        if let Some(step) = self.disk.child(dir, name)? {
            return Ok(Some(step));
        }
        Ok(self
            .buffered(dir)
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(spelling, kind)| Step {
                spelling,
                kind,
                spelling_verified: true,
            }))
    }

    fn list_dir(&self, dir: &Path) -> Result<Vec<(OsString, EntryKind)>, FsError> {
        let buffered = self.buffered(dir);
        let mut out = match self.disk.list_dir(dir) {
            Ok(entries) => entries,
            // Only a directory the disk does not HAVE lists its buffers
            // alone; a directory it has but cannot list stays an error.
            Err(e) => {
                if buffered.is_empty() || self.disk_kind(dir)?.is_some() {
                    return Err(e);
                }
                Vec::new()
            }
        };
        for entry in buffered {
            if !out.iter().any(|(n, _)| *n == entry.0) {
                out.push(entry);
            }
        }
        out.sort();
        Ok(out)
    }
}

impl<F: PathFs> PathFs for OverlayFs<'_, F> {
    fn resolve_symlink(&self, dir: &Path, name: &OsStr) -> Result<Option<PathBuf>, FsError> {
        self.disk.resolve_symlink(dir, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{MockFs, Probe};

    /// The overlay resolves a link THROUGH the disk
    /// — it holds no resolver of its own — and a buffer never masquerades
    /// as a link target.
    #[test]
    fn resolve_symlink_passes_through_to_the_disk() {
        let disk = MockFs::new()
            .dir("/ws/vendor")
            .symlink("/ws/tenants/cu/lib", "../../vendor");
        let buffers = [PathBuf::from("/ws/tenants/cu/lib/buffered.nml")];
        let fs = OverlayFs {
            disk: &disk,
            buffers: &buffers,
        };
        assert_eq!(
            fs.resolve_symlink(Path::new("/ws/tenants/cu"), OsStr::new("lib"))
                .unwrap(),
            Some(PathBuf::from("/ws/vendor"))
        );
        assert_eq!(
            disk.probes().last(),
            Some(&Probe::ResolveSymlink(
                PathBuf::from("/ws/tenants/cu"),
                OsString::from("lib")
            ))
        );
        // A name the disk has no entry for resolves to nothing, buffers
        // notwithstanding (a buffer is a file, never a link).
        assert_eq!(
            fs.resolve_symlink(Path::new("/ws/tenants/cu"), OsStr::new("buffered.nml"))
                .unwrap(),
            None
        );
    }

    /// `disk_kind`'s root arm: the filesystem root is a directory by
    /// definition, so a root listing the disk refuses stays refused even
    /// with buffers under it — only a directory ABSENT on disk lists its
    /// buffers alone.
    #[test]
    fn a_refused_root_listing_never_degrades_to_buffers_alone() {
        let disk = MockFs::new().denied("/");
        let buffers = [PathBuf::from("/x.nml")];
        let fs = OverlayFs {
            disk: &disk,
            buffers: &buffers,
        };
        assert_eq!(fs.list_dir(Path::new("/")).unwrap_err(), FsError::Denied);
        assert_eq!(fs.disk_kind(Path::new("/")).unwrap(), Some(EntryKind::Dir));
        // Contrast: a directory the disk does not HAVE lists its buffers.
        let disk = MockFs::new().dir("/ws");
        let buffers = [PathBuf::from("/ws/new/x.nml")];
        let fs = OverlayFs {
            disk: &disk,
            buffers: &buffers,
        };
        assert_eq!(fs.disk_kind(Path::new("/ws/new")).unwrap(), None);
        assert_eq!(
            fs.list_dir(Path::new("/ws/new")).unwrap(),
            vec![(OsString::from("x.nml"), EntryKind::File)]
        );
        // And a directory the disk HAS but cannot list stays an error.
        let disk = MockFs::new().denied("/ws/locked");
        let buffers = [PathBuf::from("/ws/locked/x.nml")];
        let fs = OverlayFs {
            disk: &disk,
            buffers: &buffers,
        };
        assert_eq!(
            fs.list_dir(Path::new("/ws/locked")).unwrap_err(),
            FsError::Denied
        );
    }

    /// The [`LstatFs::list_dir`] contract at this boundary: a buffered
    /// entry takes its SORTED place among the disk's, never the tail it
    /// was appended at — the walk and the hidden audit order by the
    /// listing and sort nothing themselves.
    #[test]
    fn a_buffered_entry_lists_in_its_sorted_place() {
        let disk = MockFs::new().dir("/ws").file("/ws/b.nml");
        let buffers = [PathBuf::from("/ws/c.nml"), PathBuf::from("/ws/a.nml")];
        let fs = OverlayFs {
            disk: &disk,
            buffers: &buffers,
        };
        let names: Vec<OsString> = fs
            .list_dir(Path::new("/ws"))
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["a.nml", "b.nml", "c.nml"].map(OsString::from));
    }
}
