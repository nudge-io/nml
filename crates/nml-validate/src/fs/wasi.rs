//! The wasm editor's oracle ([`WasiFs`]): `lstat` through `std`, listings
//! through an injected shim, spelling by byte-exact listing membership.
//! On the wasm target, realpath is unavailable ([`FsError::NoRealpath`]);
//! on hosts that run this backend in tests and tooling, symlink resolution
//! uses the same canonicalize as [`StdFs`].

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::disk::{DirEntryLike, absent_or_error, kind_of, listing};
use super::{EntryKind, FsError, Listing, LstatFs, PathFs, Step};

/// The wasm editor's backend: `lstat` works; realpath only on the wasm
/// target is refused. Listings
/// come through an injected shim (the LSP's abort-proof `read_dir`
/// wrapper), and a spelling is proven by BYTE-EXACT membership in the
/// parent's listing — an insensitive filesystem cannot hold two case
/// variants of one name, so an exact entry proves the spelling without
/// realpath. NML2083 form 2 therefore fires only for a genuinely
/// respelled lookup, never for every closed file in the wasm editor.
pub struct WasiFs<L> {
    pub(crate) list: L,
}

/// The wasm editor's oracle over an OPENER only — the LSP's abort-proof
/// `read_dir` wrapper, handing the kernel the opened directory as `std`
/// yields it (an unreadable entry as its error): the listing itself is
/// the kernel's one rule ([`listing`]), so the shim can neither drop an
/// entry the native oracle refuses nor sort or type one differently.
/// The `list` closure of [`WasiFs`] stays the scripted door the pins
/// use.
pub fn wasi_fs_through<O, I, E>(open: O) -> WasiFs<impl Fn(&Path) -> Listing>
where
    O: Fn(&Path) -> std::io::Result<I>,
    I: IntoIterator<Item = std::io::Result<E>>,
    E: DirEntryLike,
{
    WasiFs {
        list: move |dir: &Path| listing(open(dir)),
    }
}

impl<L> LstatFs for WasiFs<L>
where
    L: Fn(&Path) -> Result<Vec<(OsString, EntryKind)>, FsError>,
{
    fn child(&self, dir: &Path, name: &OsStr) -> Result<Option<Step>, FsError> {
        let meta = match std::fs::symlink_metadata(dir.join(name)) {
            Ok(m) => m,
            Err(e) => return absent_or_error(e).map(|_| None),
        };
        let kind = kind_of(meta.file_type());
        let spelling_verified =
            kind != EntryKind::Symlink && (self.list)(dir)?.iter().any(|(n, _)| n == name);
        Ok(Some(Step {
            spelling: name.to_os_string(),
            kind,
            spelling_verified,
        }))
    }

    fn list_dir(&self, dir: &Path) -> Result<Vec<(OsString, EntryKind)>, FsError> {
        (self.list)(dir)
    }
}

impl<L> PathFs for WasiFs<L>
where
    L: Fn(&Path) -> Result<Vec<(OsString, EntryKind)>, FsError>,
{
    fn resolve_symlink(&self, dir: &Path, name: &OsStr) -> Result<Option<PathBuf>, FsError> {
        #[cfg(target_os = "wasi")]
        {
            let _ = (dir, name);
            Err(FsError::NoRealpath)
        }
        #[cfg(not(target_os = "wasi"))]
        {
            super::disk::resolve_symlink_component(dir, name)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::{Path, PathBuf};

    use super::*;
    #[cfg(target_os = "wasi")]
    use crate::workspace::{PathError, SourceKey, Trust, WorkspaceRoot};

    /// A scratch directory that removes itself (the `lstat` half of the
    /// wasi backend is `std`'s, so the pins need a real directory; the
    /// LISTING half is the scripted closure under test).
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl std::ops::Deref for Scratch {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    fn scratch(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("nml-wasi-fs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(std::fs::canonicalize(&dir).unwrap())
    }

    fn listing(entries: &[(&str, EntryKind)]) -> Vec<(OsString, EntryKind)> {
        entries
            .iter()
            .map(|(n, k)| (OsString::from(n), *k))
            .collect()
    }

    /// The shimmed backend over `std`'s own opener lists EXACTLY what
    /// the native oracle lists — names, kinds (a symlink as a symlink,
    /// never its target's kind) and order — because both go through the
    /// kernel's one listing rule; and it is the kernel's `list_dir`
    /// the walk calls, not a second walker's.
    #[cfg(unix)]
    #[test]
    fn the_shimmed_backend_lists_exactly_as_the_native_one() {
        let dir = scratch("shim-parity");
        std::fs::write(dir.join("b.nml"), "").unwrap();
        std::fs::create_dir(dir.join("a")).unwrap();
        std::os::unix::fs::symlink("b.nml", dir.join("c.nml")).unwrap();
        let shimmed = super::wasi_fs_through(|dir: &Path| std::fs::read_dir(dir));
        let native = crate::workspace::StdFs;
        let got = shimmed.list_dir(&dir).unwrap();
        assert_eq!(got, native.list_dir(&dir).unwrap());
        assert_eq!(
            got,
            listing(&[
                ("a", EntryKind::Dir),
                ("b.nml", EntryKind::File),
                ("c.nml", EntryKind::Symlink),
            ])
        );
    }

    /// Spelling by BYTE-EXACT listing membership —
    /// a lookup the scripted listing holds verbatim is verified; a case
    /// variant the filesystem still finds (a lookup-insensitive disk) is
    /// `lstat`-real but unverified, so closed trust fails closed (form
    /// 2); a name the listing lacks and the disk lacks is absent.
    #[test]
    fn membership_spelling_verifies_byte_exact_hits_only() {
        let dir = scratch("membership");
        std::fs::write(dir.join("Admin.nml"), "").unwrap();
        let asked = Cell::new(0usize);
        let fs = WasiFs {
            list: |p: &Path| {
                asked.set(asked.get() + 1);
                assert_eq!(p, dir.0.as_path());
                Ok(listing(&[("Admin.nml", EntryKind::File)]))
            },
        };
        let hit = fs.child(&dir.0, OsStr::new("Admin.nml")).unwrap().unwrap();
        assert_eq!(
            (hit.kind, hit.spelling_verified, hit.spelling.as_os_str()),
            (EntryKind::File, true, OsStr::new("Admin.nml"))
        );
        assert_eq!(asked.get(), 1, "one listing per verified child");
        match fs.child(&dir.0, OsStr::new("admin.nml")).unwrap() {
            // A lookup-insensitive disk finds the entry; the listing
            // does not hold that spelling, so it is NOT verified — and
            // the reported spelling is the caller's, never respelled.
            Some(step) => {
                assert_eq!(
                    (step.kind, step.spelling_verified),
                    (EntryKind::File, false)
                );
                assert_eq!(step.spelling, OsStr::new("admin.nml"));
                assert_eq!(asked.get(), 2);
            }
            // A sensitive disk: absent, and the listing is never asked.
            None => assert_eq!(asked.get(), 1),
        }
        assert_eq!(fs.child(&dir.0, OsStr::new("nope.nml")).unwrap(), None);
        // `list_dir` is the shim's answer, verbatim.
        assert_eq!(
            fs.list_dir(&dir.0).unwrap(),
            listing(&[("Admin.nml", EntryKind::File)])
        );
    }

    /// A listing the shim refuses fails the child (never "unverified"),
    /// and a symlink is reported without consulting the listing at all
    /// (its name is never a surviving key component).
    #[cfg(unix)]
    #[test]
    fn listing_failures_propagate_and_symlinks_skip_the_listing() {
        let dir = scratch("refused");
        std::fs::write(dir.join("x.nml"), "").unwrap();
        std::os::unix::fs::symlink("x.nml", dir.join("l.nml")).unwrap();
        let denied = WasiFs {
            list: |_: &Path| Err(FsError::Denied),
        };
        assert_eq!(
            denied.child(&dir.0, OsStr::new("x.nml")).unwrap_err(),
            FsError::Denied
        );
        assert_eq!(denied.list_dir(&dir.0).unwrap_err(), FsError::Denied);
        let never = WasiFs {
            list: |_: &Path| -> Result<Vec<(OsString, EntryKind)>, FsError> {
                panic!("a symlink's spelling is never proven by listing")
            },
        };
        let link = never.child(&dir.0, OsStr::new("l.nml")).unwrap().unwrap();
        assert_eq!(
            (link.kind, link.spelling_verified),
            (EntryKind::Symlink, false)
        );
    }

    /// No realpath on the wasm TARGET: `resolve_symlink` is `NoRealpath`
    /// for every name, so an OPEN walk marks the key unverifiable, and a
    /// CLOSED walk over a respelled lookup is NML2083 form 2 — while an
    /// exact lookup mints a verified key (the wasm editor does not deny
    /// every closed file).
    #[cfg(target_os = "wasi")]
    #[test]
    fn no_realpath_is_closed_form_two_for_respelled_lookups_only() {
        let dir = scratch("form2");
        std::fs::create_dir_all(dir.join("Admin")).unwrap();
        std::fs::write(dir.join("Admin/s.nml"), "").unwrap();
        let fs = WasiFs {
            list: |p: &Path| {
                let entries: Vec<(OsString, EntryKind)> = std::fs::read_dir(p)
                    .map_err(|_| FsError::Io(None))?
                    .map(|e| {
                        let e = e.map_err(|_| FsError::Io(None))?;
                        let kind = kind_of(e.file_type().map_err(|_| FsError::Io(None))?);
                        Ok((e.file_name(), kind))
                    })
                    .collect::<Result<_, FsError>>()?;
                Ok(entries)
            },
        };
        assert_eq!(
            fs.resolve_symlink(&dir.0, OsStr::new("Admin")).unwrap_err(),
            FsError::NoRealpath
        );
        // The root itself folds through `child` (a real directory, spelled
        // exactly), so an explicit root is fine on this backend.
        let root = WorkspaceRoot::explicit(&dir.0, &fs).expect("root folds");
        let exact = SourceKey::mint(&root, Path::new("Admin/s.nml"), &fs, Trust::Closed).unwrap();
        assert_eq!(exact.key.as_str(), "Admin/s.nml");
        let verified = exact.verify(&fs).unwrap().expect("exists");
        assert!(!verified.respelled);
        if std::fs::symlink_metadata(dir.join("admin")).is_ok() {
            // Lookup-insensitive disk: the respelled parent is form 2 under
            // closed trust, and tolerated-but-reported under open trust.
            assert_eq!(
                SourceKey::mint(&root, Path::new("admin/s.nml"), &fs, Trust::Closed).unwrap_err(),
                PathError::Unverifiable {
                    key: SourceKey::checked("admin/s.nml").unwrap()
                }
            );
            let open = SourceKey::mint(&root, Path::new("admin/s.nml"), &fs, Trust::Open).unwrap();
            assert_eq!(
                open.via_symlink,
                crate::workspace::SymlinkVerdict::Unverifiable
            );
        } else {
            assert_eq!(
                SourceKey::mint(&root, Path::new("admin/s.nml"), &fs, Trust::Closed)
                    .unwrap()
                    .verify(&fs)
                    .unwrap(),
                None,
                "case-sensitive: absent"
            );
        }
    }

    /// r105-cov: the KERNEL's answers do not depend on the order a
    /// backend lists entries in. Every real backend sorts at the one
    /// listing boundary ([`listing`]); the scripted door here does not,
    /// so a listing REVERSED against the sorted contract is the probe —
    /// over a tree whose sorted order is not the sorted order of the keys
    /// beneath it (`a` before `a-b`, `a-b/x` before `a/x`) with a
    /// dot-file, a deeper manifest and, on unix, an unkeyable name. The
    /// derived root and its origin, the walk's files, skipped rows,
    /// claims, inert notes and universe notes all come out identical to
    /// the native oracle's. (The walk sorts each round's keys and its
    /// skipped rows itself; the shadow DISCLOSURE names the first marker
    /// of a listing and so inherits the contract — which is why the
    /// contract is pinned at every backend, not here.)
    #[test]
    fn the_kernels_answers_do_not_depend_on_listing_order() {
        use std::sync::Arc;

        use crate::workspace::discover;
        use crate::workspace::{InputKind, StdFs, WorkspaceRoot};

        let dir = scratch("listing-order");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(
            dir.join("demo.package.nml"),
            "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    - \
             core:\n        file = \"core.model.nml\"\n\n[]validator validators:\n    - tenantFlows:\n        \
             files:\n            - \"tenants/**/*.flow.nml\"\n        schemas:\n            - core\n        \
             strict = true\n",
        )
        .unwrap();
        std::fs::write(dir.join("core.model.nml"), crate::test_support::DEMO_CORE).unwrap();
        for key in [
            "tenants/a/x.flow.nml",
            "tenants/a-b/x.flow.nml",
            "tenants/ab/x.flow.nml",
            "tenants/cu/plain.flow.nml",
            "tenants/cu/.secret.flow.nml",
            "tenants/du/m.package.nml",
            "admin/ops.flow.nml",
        ] {
            let path = dir.join(key);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "").unwrap();
        }
        #[cfg(unix)]
        std::fs::write(dir.join("tenants/cu/ev\\il.flow.nml"), "").unwrap();
        let reversed = WasiFs {
            list: |p: &Path| {
                let mut entries = super::listing(std::fs::read_dir(p))?;
                entries.reverse();
                Ok(entries)
            },
        };
        let read = |_: InputKind, p: &Path| std::fs::read_to_string(p).map_err(|e| e.to_string());
        let target = dir.join("tenants/cu/plain.flow.nml");
        let native_root = WorkspaceRoot::derive(&target, &StdFs).unwrap();
        let reversed_root = WorkspaceRoot::derive(&target, &reversed).unwrap();
        assert_eq!(native_root.path(), reversed_root.path());
        assert_eq!(native_root.origin(), reversed_root.origin());
        let native = discover(&native_root, &StdFs, &read, vec![], Arc::default());
        let shuffled = discover(&reversed_root, &reversed, &read, vec![], Arc::default());
        assert_eq!(native.truncated(), None);
        assert_eq!(native.files(), shuffled.files());
        assert!(
            native.files().len() >= 6,
            "the fixture must be walked: {:?}",
            native.files()
        );
        assert_eq!(
            format!("{:?}", native.skipped()),
            format!("{:?}", shuffled.skipped())
        );
        assert!(
            !native.skipped().is_empty(),
            "the dot-file must be a skipped row"
        );
        let claims = |d: &crate::workspace::Discovery| -> Vec<String> {
            d.claims()
                .iter()
                .map(|c| format!("{:?}", c.manifest()))
                .collect()
        };
        assert_eq!(claims(&native), claims(&shuffled));
        let shown = |ds: &[nml_core::diagnostic::Diagnostic]| -> Vec<String> {
            ds.iter().map(ToString::to_string).collect()
        };
        assert_eq!(shown(native.inert()), shown(shuffled.inert()));
        assert_eq!(
            shown(&native.universe_notes()),
            shown(&shuffled.universe_notes())
        );
    }
}
