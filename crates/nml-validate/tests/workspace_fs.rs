//! The path kernel over the REAL filesystem (RFC 0019 item 0, step 0b):
//! the platform-spelling premise (E25) as an executable pin, and the
//! symlink verdict over real links. Integration tests on purpose:
//! `CARGO_TARGET_TMPDIR` exists here (CI `/tmp` is often a different
//! filesystem from the checkout), and the kernel's own source ratchet
//! keeps ambient `std::fs` out of `src/workspace/`.

use std::path::{Path, PathBuf};

#[cfg(unix)]
use nml_validate::fs::EntryKind;
use nml_validate::fs::StdFs;
#[cfg(unix)]
use nml_validate::workspace::SymlinkVerdict;
use nml_validate::workspace::{Keyed, PathError, SourceKey, Trust, WorkspaceRoot};

/// A scratch directory that removes itself on every path — a failed
/// assertion included (a leftover `target/tmp/workspace-fs-*` was the
/// r51 evidence that a success-path cleanup is none).
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

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

fn scratch(tag: &str) -> Scratch {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("workspace-fs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Scratch(dir)
}

fn mint(root: &WorkspaceRoot, path: &Path, trust: Trust) -> Result<Keyed, PathError> {
    SourceKey::mint(root, path, &StdFs, trust)
}

/// E25, executable on every platform CI runs: `canonicalize` returns the
/// ON-DISK spelling of every existing component. Self-selecting: creates
/// `Admin/`; if `admin` looks up, the filesystem is lookup-insensitive
/// and the kernel must respell `ADMIN/x` to `Admin/x` (and NFD to the
/// on-disk NFC); else the two spellings are two keys. A red here on a
/// Linux casefold directory is the documented E25 residue.
#[test]
fn platform_spelling_pin() {
    let dir = scratch("spelling");
    std::fs::create_dir_all(dir.join("Admin")).unwrap();
    std::fs::write(dir.join("Admin/s.nml"), "").unwrap();
    std::fs::write(dir.join("x.package.nml"), "").unwrap();
    let root = WorkspaceRoot::explicit(&dir, &StdFs).unwrap();
    let insensitive = std::fs::symlink_metadata(dir.join("admin")).is_ok();
    if insensitive {
        let keyed = mint(&root, Path::new("ADMIN/S.NML"), Trust::Closed).unwrap();
        assert_eq!(
            keyed.key.as_str(),
            "Admin/S.NML",
            "lookup-insensitive filesystem without on-disk respelling: \
             unsupported for closed bindings (E25 residue — Linux casefold?)"
        );
        let v = keyed.verify(&StdFs).unwrap().expect("exists");
        assert_eq!(v.key.as_str(), "Admin/s.nml");
        assert!(v.respelled, "the leaf respells at verify (A11)");
        // Unicode form: an NFC-created directory looked up as NFD keys as
        // the on-disk NFC bytes.
        std::fs::create_dir_all(dir.join("caf\u{e9}")).unwrap();
        std::fs::write(dir.join("caf\u{e9}/x.nml"), "").unwrap();
        if std::fs::symlink_metadata(dir.join("cafe\u{301}")).is_ok() {
            let keyed = mint(&root, Path::new("cafe\u{301}/x.nml"), Trust::Closed).unwrap();
            assert_eq!(keyed.key.as_str(), "caf\u{e9}/x.nml");
        }
    } else {
        let a = mint(&root, Path::new("Admin/s.nml"), Trust::Closed).unwrap();
        let b = mint(&root, Path::new("admin/s.nml"), Trust::Closed).unwrap();
        assert_ne!(a.key, b.key, "case-sensitive: two spellings, two keys");
        assert_eq!(
            b.verify(&StdFs).unwrap(),
            None,
            "and `admin/` does not exist"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlink_verdict_indexes_first_component() {
    use std::os::unix::fs::symlink;
    let dir = scratch("symlink");
    std::fs::create_dir_all(dir.join("admin/secretdir")).unwrap();
    std::fs::write(dir.join("admin/secretdir/x.nml"), "").unwrap();
    std::fs::write(dir.join("admin/secret.nml"), "").unwrap();
    std::fs::create_dir_all(dir.join("tenants/cu")).unwrap();
    std::fs::write(dir.join("x.package.nml"), "").unwrap();
    symlink("../../admin/secretdir", dir.join("tenants/cu/lib")).unwrap();
    symlink("../../admin/secret.nml", dir.join("tenants/cu/leaf.nml")).unwrap();
    symlink("../../admin/nope", dir.join("tenants/cu/dangling")).unwrap();
    let root = WorkspaceRoot::explicit(&dir, &StdFs).unwrap();

    // Closed: halts at the first symlinked component, existing or dangling
    // target alike, with the same lexical key.
    for link in ["lib", "dangling"] {
        let err = mint(
            &root,
            Path::new(&format!("tenants/cu/{link}/x.nml")),
            Trust::Closed,
        )
        .unwrap_err();
        assert_eq!(
            err,
            PathError::SymlinkComponent {
                component: link.into(),
                key: SourceKey::checked(&format!("tenants/cu/{link}/x.nml")).unwrap()
            }
        );
    }
    // Open: followed; the key names the target; the verdict indexes the link.
    let keyed = mint(&root, Path::new("tenants/cu/lib/x.nml"), Trust::Open).unwrap();
    assert_eq!(keyed.key.as_str(), "admin/secretdir/x.nml");
    assert_eq!(keyed.via_symlink, SymlinkVerdict::Through(2));
    assert_eq!(keyed.verify(&StdFs).unwrap().unwrap().kind, EntryKind::File);
    // A symlink LEAF: minting never sees it; closed `verify` rejects it.
    let keyed = mint(&root, Path::new("tenants/cu/leaf.nml"), Trust::Closed).unwrap();
    assert_eq!(keyed.via_symlink, SymlinkVerdict::None);
    assert!(matches!(
        &keyed.verify(&StdFs).unwrap_err(),
        PathError::SymlinkComponent { component, .. } if component == "leaf.nml"
    ));
}

#[cfg(unix)]
#[test]
fn derive_stops_before_a_real_author_link_under_the_git_fence() {
    // E28 (2) over the real filesystem: two trees differing only in
    // whether `tenants/cu/lib`'s target exists derive the SAME root
    // without `--root`, and a `--root` naming a link to a file is not a
    // directory.
    use std::os::unix::fs::symlink;
    let mut roots = Vec::new();
    for with_target in [true, false] {
        let dir = scratch(if with_target { "derive-a" } else { "derive-b" });
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("demo.package.nml"), "").unwrap();
        std::fs::create_dir_all(dir.join("tenants/cu")).unwrap();
        symlink("../../vendor", dir.join("tenants/cu/lib")).unwrap();
        if with_target {
            std::fs::create_dir_all(dir.join("vendor")).unwrap();
            std::fs::write(dir.join("vendor/base.flow.nml"), "").unwrap();
        }
        let root =
            WorkspaceRoot::derive(&dir.join("tenants/cu/lib/base.flow.nml"), &StdFs).unwrap();
        assert_eq!(root.path(), std::fs::canonicalize(&dir).unwrap());
        roots.push(root.origin().clone());
    }
    assert_eq!(roots[0], roots[1]);
    let dir = scratch("rootlink");
    std::fs::write(dir.join("x.nml"), "").unwrap();
    symlink(dir.join("x.nml"), dir.join("rootlink")).unwrap();
    assert_eq!(
        WorkspaceRoot::explicit(&dir.join("rootlink"), &StdFs).unwrap_err(),
        nml_validate::workspace::RootError::NotADirectory
    );
}

#[test]
fn derive_over_real_tree_finds_outermost_manifest_within_git_fence() {
    let dir = scratch("derive");
    std::fs::write(dir.join("evil.package.nml"), "").unwrap();
    let app = dir.join("app");
    std::fs::create_dir_all(app.join(".git")).unwrap();
    // The operator's root config anchors the universe at `app/`; the
    // manifest itself may live deeper (it governs its own subtree).
    std::fs::write(app.join("nml-project.nml"), "").unwrap();
    std::fs::create_dir_all(app.join("schemas")).unwrap();
    std::fs::write(app.join("schemas/demo.package.nml"), "").unwrap();
    std::fs::create_dir_all(app.join("tenants/cu")).unwrap();
    std::fs::write(app.join("tenants/cu/nml-project.nml"), "").unwrap();
    std::fs::write(app.join("tenants/cu/x.nml"), "").unwrap();
    // The manifest ABOVE the fence (`evil.package.nml`) never governs:
    // `app/.git` is a DIRECTORY (a real checkout), so E21's fence keeps
    // the marker out and the shadow check (r80-sec F6) DISCLOSES it over
    // a real tree — never silently ignored, never a refusal that a
    // stray marker in a shared parent could inflict on every checkout
    // beneath it.
    let root = WorkspaceRoot::derive(&app.join("tenants/cu/x.nml"), &StdFs).unwrap();
    assert_eq!(root.path(), std::fs::canonicalize(&app).unwrap());
    assert_eq!(
        *root.origin(),
        nml_validate::workspace::RootOrigin::Derived {
            fence: nml_validate::workspace::Fence::Vcs {
                kind: nml_validate::fs::EntryKind::Dir
            },
            shadowed: Some(nml_validate::workspace::Shadow::Marker(
                std::fs::canonicalize(&dir)
                    .unwrap()
                    .join("evil.package.nml")
            )),
        }
    );
    // The same marker above a `.git` FILE fence (what git writes for a
    // submodule) REFUSES.
    std::fs::remove_dir_all(app.join(".git")).unwrap();
    std::fs::write(app.join(".git"), "gitdir: /nonexistent\n").unwrap();
    let err = WorkspaceRoot::derive(&app.join("tenants/cu/x.nml"), &StdFs).unwrap_err();
    assert_eq!(
        err,
        nml_validate::workspace::RootError::Shadowed {
            marker: std::fs::canonicalize(&dir)
                .unwrap()
                .join("evil.package.nml"),
            fence: std::fs::canonicalize(&app).unwrap().join(".git"),
        }
    );
    std::fs::remove_file(app.join(".git")).unwrap();
    std::fs::create_dir_all(app.join(".git")).unwrap();
    std::fs::remove_file(dir.join("evil.package.nml")).unwrap();
    let root = WorkspaceRoot::derive(&app.join("tenants/cu/x.nml"), &StdFs).unwrap();
    assert_eq!(root.path(), std::fs::canonicalize(&app).unwrap());
    // The checkout's own `.git` above the scratch shadows it — reported
    // as the entry itself, not refused (no marker sits between).
    if let nml_validate::workspace::RootOrigin::Derived { shadowed, .. } = root.origin() {
        if let Some(nml_validate::workspace::Shadow::Git(entry)) = shadowed {
            assert!(
                std::fs::symlink_metadata(entry).is_ok(),
                "{}",
                entry.display()
            );
            assert_eq!(entry.file_name().unwrap(), ".git");
        } else {
            assert!(shadowed.is_none(), "{shadowed:?}");
        }
    } else {
        panic!("{:?}", root.origin());
    }
    let key = mint(&root, &app.join("tenants/cu/x.nml"), Trust::Closed).unwrap();
    assert_eq!(key.key.as_str(), "tenants/cu/x.nml");
}

/// A directory that is SEARCHABLE but not LISTABLE (mode `0111`) is not
/// a directory with no root marker in it: the answer is unknown, and a
/// universe is never derived on an unknown answer — the rule the shadow
/// check already applied to its own work bound
/// (`RootError::ShadowUnchecked`) and the walk applies to a single
/// unreadable directory entry (`collect_listing`).
///
/// Without it (`list_dir(dir).ok()`) the whole binding system was one
/// `chmod 0111` away from silence: the operator's `demo.package.nml`
/// vanished from the derivation, the universe shrank to the target's own
/// directory — an OPEN context that governs nothing — and `nml check`
/// went from exit 1 under the binding's `strict` to `ok`, exit 0.
///
/// ABOVE the fence the same `chmod` costs what THAT fence can deny, and
/// only that: above a `.git` entry that is no directory an unseen marker
/// is the `Shadowed` refusal, so the blinded check refuses (the green
/// run this pins shut); above a DIRECTORY fence a marker is the
/// disclosed `Shadow::Marker` and nothing more, so the blinded check
/// costs the DISCLOSURE and the universe still derives — refusing there
/// put every run behind the mode of every directory up to `/` for a
/// sentence that could not deny anything. `MockFs` cannot script this
/// shape (its denied node refuses lookups too), so the pin is on real
/// modes.
#[cfg(unix)]
#[test]
fn derive_refuses_a_searchable_but_unlistable_directory_instead_of_deriving_without_it() {
    use std::os::unix::fs::PermissionsExt;

    let mode = |p: &Path, bits: u32| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(bits)).unwrap();
    };
    let dir = scratch("unlistable");
    let app = dir.join("app");
    std::fs::create_dir_all(app.join(".git")).unwrap();
    std::fs::write(app.join("demo.package.nml"), "").unwrap();
    std::fs::create_dir_all(app.join("tenants/cu")).unwrap();
    std::fs::write(app.join("tenants/cu/x.nml"), "").unwrap();
    let target = app.join("tenants/cu/x.nml");

    // Control: the marker anchors the universe at `app/`.
    let root = WorkspaceRoot::derive(&target, &StdFs).unwrap();
    assert_eq!(root.path(), std::fs::canonicalize(&app).unwrap());

    // (a) the MARKER search: `app` searchable, not listable.
    mode(&app, 0o111);
    let listable = std::fs::read_dir(&app).is_ok();
    let got = WorkspaceRoot::derive(&target, &StdFs);
    mode(&app, 0o755);
    if listable {
        // Running as root (or on a filesystem that ignores the mode):
        // the shape cannot be produced, and the control above is all
        // this run can prove.
        eprintln!("unlistable pin: 0111 still lists here — skipped (root?)");
    } else {
        assert_eq!(
            got,
            Err(nml_validate::workspace::RootError::Fs(
                nml_validate::fs::FsError::Denied
            )),
            "an unlistable directory must refuse the derivation, not derive without its marker"
        );
    }

    // (b) the SHADOW check above a DIRECTORY fence: the scratch holds a
    // marker and is searchable but not listable. Nothing this listing
    // could have found denies anything — a marker above a directory
    // fence is the DISCLOSED shadow — so the universe still derives, at
    // the same root, with the disclosure lost and nothing else.
    std::fs::write(dir.join("evil.package.nml"), "").unwrap();
    mode(&dir, 0o111);
    let listable = std::fs::read_dir(&dir.0).is_ok();
    let got = WorkspaceRoot::derive(&target, &StdFs);
    mode(&dir, 0o755);
    if listable {
        eprintln!("unlistable pin (shadow): 0111 still lists here — skipped (root?)");
        return;
    }
    let got = got.expect("a blinded disclosure never denies a universe");
    assert_eq!(got.path(), std::fs::canonicalize(&app).unwrap());
    // The blinded ancestor's marker is the one fact lost; the walk goes
    // on above it, so whatever sits HIGHER (this checkout's own `.git`)
    // is still disclosed.
    let marker = std::fs::canonicalize(&dir)
        .unwrap()
        .join("evil.package.nml");
    assert!(
        !matches!(
            got.origin(),
            nml_validate::workspace::RootOrigin::Derived {
                shadowed: Some(nml_validate::workspace::Shadow::Marker(m)),
                ..
            } if *m == marker
        ),
        "the blinded marker cannot be reported: {:?}",
        got.origin()
    );
    // And listable again, the marker above the DIRECTORY fence is the
    // disclosed shadow it always was.
    let root = WorkspaceRoot::derive(&target, &StdFs).unwrap();
    assert_eq!(root.path(), std::fs::canonicalize(&app).unwrap());
    assert!(matches!(
        root.origin(),
        nml_validate::workspace::RootOrigin::Derived {
            shadowed: Some(nml_validate::workspace::Shadow::Marker(_)),
            ..
        }
    ));

    // (c) the same `chmod` above a fence that is NO directory — the
    // submodule/linked-worktree/planted `.git` FILE — is the shape the
    // check exists for: a marker there IS `RootError::Shadowed`, so an
    // ancestor that cannot be listed refuses rather than derive a
    // universe the marker above would have shrunk. (The `.git` directory
    // becomes a file in place, so the fence and the target are the same
    // two paths as above.)
    std::fs::remove_dir_all(app.join(".git")).unwrap();
    std::fs::write(app.join(".git"), "gitdir: ../elsewhere\n").unwrap();
    // Readable, the marker above the FILE fence refuses by name.
    assert_eq!(
        WorkspaceRoot::derive(&target, &StdFs),
        Err(nml_validate::workspace::RootError::Shadowed {
            marker: std::fs::canonicalize(&dir)
                .unwrap()
                .join("evil.package.nml"),
            fence: std::fs::canonicalize(&app).unwrap().join(".git"),
        })
    );
    // Unlistable, the same refusal is unreachable BY NAME — and the
    // universe is denied all the same, never derived on the unknown.
    mode(&dir, 0o111);
    let got = WorkspaceRoot::derive(&target, &StdFs);
    mode(&dir, 0o755);
    assert_eq!(
        got,
        Err(nml_validate::workspace::RootError::Fs(
            nml_validate::fs::FsError::Denied
        )),
        "above a fence that is no directory an unseen marker is a refusal, so a blinded \
         listing refuses too"
    );
}

/// E35 (r62-sec finding 1): the race-free read-through over a real
/// tree. A regular file reads; a symlink component — existing or
/// dangling — is refused at the open with the SAME shape; a link leaf
/// likewise; a FIFO leaf is refused typed without blocking; a directory
/// leaf and an absent leaf map to the errno a path open would have
/// produced (byte-identical messages); the chain fences its own
/// components.
#[cfg(unix)]
#[test]
fn open_beneath_reads_regular_files_and_refuses_everything_else() {
    use std::io::Read;

    use nml_validate::fs::{OpenError, open_beneath};

    let dir = scratch("beneath");
    let root = dir.join("proj");
    std::fs::create_dir_all(root.join("tenants/cu/lib")).unwrap();
    std::fs::create_dir_all(dir.join("secret")).unwrap();
    std::fs::write(root.join("tenants/cu/lib/base.flow.nml"), "thing b:\n").unwrap();
    std::fs::write(dir.join("secret/s.flow.nml"), "thing SECRET:\n").unwrap();
    std::os::unix::fs::symlink("../../../secret", root.join("tenants/cu/link")).unwrap();
    std::os::unix::fs::symlink(
        "../../../secret/s.flow.nml",
        root.join("tenants/cu/leaf.flow.nml"),
    )
    .unwrap();
    let fifo = root.join("tenants/cu/fifo.flow.nml");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs")
            .success()
    );
    let root = std::fs::canonicalize(&root).unwrap();

    let mut text = String::new();
    open_beneath(&root, &["tenants", "cu", "lib", "base.flow.nml"])
        .expect("a regular file opens")
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "thing b:\n");

    // r80-cov (mutant B4 survived — the fence rows at the end of this
    // test run after `secret/` is deleted, so `..` failed ENOENT rather
    // than at the fence): with the outside file PRESENT, a `..` or `.`
    // component is refused by the chain's OWN fence (InvalidInput),
    // never handed to the OS — on the `openat` chain `..` would walk up.
    // The fence's VOCABULARY (every non-plain shape, `plain`/`split_leaf`
    // at the unit) is `fs::tests::the_chain_refuses_every_non_plain_
    // component_itself`; this pins the one end-to-end fact — the fence
    // fires before the OS sees the component.
    for components in [
        &["..", "secret", "s.flow.nml"][..],
        &["tenants", "cu", "..", "..", "..", "secret", "s.flow.nml"][..],
        &["tenants", ".", "cu", "lib", "base.flow.nml"][..],
    ] {
        let dot = components
            .iter()
            .find(|c| **c == ".." || **c == ".")
            .unwrap();
        match open_beneath(&root, components) {
            Err(OpenError::Io(e)) => assert_eq!(
                (e.kind(), e.to_string()),
                (
                    std::io::ErrorKind::InvalidInput,
                    format!("`{dot}` is not a plain path component")
                ),
                "{components:?}"
            ),
            other => panic!("{components:?}: expected the chain's fence, got {other:?}"),
        }
    }

    let symlink_at = |components: &[&str]| match open_beneath(&root, components) {
        Err(OpenError::Symlink { component }) => component,
        other => panic!("{components:?}: expected a symlink refusal, got {other:?}"),
    };
    assert_eq!(symlink_at(&["tenants", "cu", "link", "s.flow.nml"]), "link");
    assert_eq!(
        symlink_at(&["tenants", "cu", "leaf.flow.nml"]),
        "leaf.flow.nml"
    );
    // Dangling now: the same refusal, the same component.
    std::fs::remove_dir_all(dir.join("secret")).unwrap();
    assert_eq!(symlink_at(&["tenants", "cu", "link", "s.flow.nml"]), "link");
    assert_eq!(
        symlink_at(&["tenants", "cu", "leaf.flow.nml"]),
        "leaf.flow.nml"
    );

    // A FIFO leaf: typed, and back within the watchdog (O_NONBLOCK).
    let started = std::time::Instant::now();
    match open_beneath(&root, &["tenants", "cu", "fifo.flow.nml"]) {
        Err(OpenError::NotRegular { component, dir }) => {
            assert_eq!((component.as_str(), dir), ("fifo.flow.nml", false));
        }
        other => panic!("expected a non-regular refusal, got {other:?}"),
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(
        OpenError::NotRegular {
            component: "fifo.flow.nml".into(),
            dir: false
        }
        .into_io()
        .to_string(),
        "`fifo.flow.nml` is not a regular file (refused at open)"
    );

    // A directory leaf and an absent leaf: the path open's own errno —
    // messages byte-identical to `File::open`'s.
    let by_path = |rel: &str| std::fs::File::open(root.join(rel)).unwrap_err().to_string();
    match open_beneath(&root, &["tenants", "cu", "lib"]) {
        Err(e @ OpenError::NotRegular { dir: true, .. }) => {
            assert_eq!(e.into_io().to_string(), "Is a directory (os error 21)");
        }
        other => panic!("expected a directory refusal, got {other:?}"),
    }
    match open_beneath(&root, &["tenants", "cu", "absent.flow.nml"]) {
        Err(e @ OpenError::Io(_)) => {
            assert_eq!(
                e.into_io().to_string(),
                by_path("tenants/cu/absent.flow.nml")
            );
        }
        other => panic!("expected ENOENT, got {other:?}"),
    }
    match open_beneath(&root, &["tenants", "cu", "lib", "base.flow.nml", "x"]) {
        Err(e @ OpenError::NotADirectory { .. }) => {
            assert!(
                matches!(&e, OpenError::NotADirectory { component } if component == "base.flow.nml")
            );
            assert_eq!(
                e.into_io().to_string(),
                by_path("tenants/cu/lib/base.flow.nml/x")
            );
        }
        other => panic!("expected ENOTDIR, got {other:?}"),
    }

    // The chain's own fence: nothing but a plain name is handed to the OS.
    for components in [
        &["..", "secret", "s.flow.nml"][..],
        &["tenants", "cu", "..", "..", "..", "secret", "s.flow.nml"][..],
        &["tenants", ".", "cu", "lib", "base.flow.nml"][..],
        &["", "base.flow.nml"][..],
        &["tenants/cu/lib/base.flow.nml"][..],
        &[][..],
    ] {
        assert!(
            open_beneath(&root, components).is_err(),
            "{components:?} must be refused"
        );
    }
}

/// E35 (r62-sec finding 1b): the fixer's write replaces the file through
/// the parent's descriptor — the original's mode preserved, no temp file
/// left behind — and a parent swapped for a symlink after classification
/// is refused before a byte is written anywhere.
#[cfg(unix)]
#[test]
fn write_beneath_replaces_through_the_parent_handle() {
    use std::os::unix::fs::PermissionsExt;

    use nml_validate::fs::{OpenError, write_beneath};

    let dir = scratch("write-beneath");
    let root = dir.join("proj");
    std::fs::create_dir_all(root.join("tenants/cu")).unwrap();
    std::fs::create_dir_all(dir.join("outside")).unwrap();
    let file = root.join("tenants/cu/f.flow.nml");
    std::fs::write(&file, "old\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let root = std::fs::canonicalize(&root).unwrap();
    let key = ["tenants", "cu", "f.flow.nml"];

    // r65 (r64 NIT 5): a stale temp from a crashed run under a reused pid
    // is unlinked before the `O_EXCL` create — never `EEXIST`.
    let stale = root.join(format!("tenants/cu/.f.flow.nml.tmp-{}", std::process::id()));
    std::fs::write(&stale, "stale\n").unwrap();

    write_beneath(&root, &key, b"new\n").expect("replaces");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600,
        "the original's mode survives the rewrite"
    );
    let names: Vec<String> = std::fs::read_dir(root.join("tenants/cu"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["f.flow.nml"], "no temp file left behind");

    // An absent target is created (the create default mode).
    write_beneath(&root, &["tenants", "cu", "g.flow.nml"], b"fresh\n").expect("creates");
    assert_eq!(
        std::fs::read_to_string(root.join("tenants/cu/g.flow.nml")).unwrap(),
        "fresh\n"
    );

    // A leaf that is a link is refused, never written through.
    std::os::unix::fs::symlink("../../../outside/o.nml", root.join("tenants/cu/l.flow.nml"))
        .unwrap();
    assert!(matches!(
        write_beneath(&root, &["tenants", "cu", "l.flow.nml"], b"x"),
        Err(OpenError::Symlink { component }) if component == "l.flow.nml"
    ));
    assert!(!dir.join("outside/o.nml").exists());

    // The parent swapped for a link to an outside directory: refused at
    // `cu`, and the outside directory stays empty.
    std::fs::rename(root.join("tenants/cu"), root.join("tenants/cu.real")).unwrap();
    std::os::unix::fs::symlink("../../outside", root.join("tenants/cu")).unwrap();
    assert!(matches!(
        write_beneath(&root, &key, b"redirected"),
        Err(OpenError::Symlink { component }) if component == "cu"
    ));
    assert_eq!(std::fs::read_dir(dir.join("outside")).unwrap().count(), 0);
}

/// r80-sec F8: `write_beneath` preserves the original's PERMISSION bits
/// only — the replacement is a new inode owned by the fixer's user, so a
/// set-uid, set-gid or sticky bit an author put on the original is never
/// minted onto the operator's file: 4755 → 755, 1644 → 644, 600 → 600,
/// and an absent target is created at the default 644 (under a 022
/// umask).
#[cfg(unix)]
#[test]
fn write_beneath_never_mints_set_id_or_sticky_bits() {
    use std::os::unix::fs::PermissionsExt;

    use nml_validate::fs::write_beneath;

    let dir = scratch("write-mode");
    let root = dir.join("proj");
    std::fs::create_dir_all(root.join("t")).unwrap();
    let root = std::fs::canonicalize(&root).unwrap();
    for (requested, expected) in [(0o4755u32, 0o755u32), (0o1644, 0o644), (0o600, 0o600)] {
        let file = root.join("t/f.nml");
        std::fs::write(&file, "old\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(requested)).unwrap();
        let before = std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777;
        write_beneath(&root, &["t", "f.nml"], b"new\n").expect("replaces");
        let after = std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            after, expected,
            "requested {requested:o} (on disk {before:o}): permission bits only survive"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");
        std::fs::remove_file(&file).unwrap();
    }
    // Absent: the create default, no special bits.
    write_beneath(&root, &["t", "g.nml"], b"fresh\n").expect("creates");
    let mode = std::fs::metadata(root.join("t/g.nml"))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(
        mode & 0o7000,
        0,
        "no special bits on a created file: {mode:o}"
    );
    assert_eq!(mode & 0o600, 0o600, "{mode:o}");
}

/// r85 (r84-sec §6): the temp file is CREATED at the original's
/// permission bits — not `0o666 & ~umask` and chmod'ed after the write
/// — so a `0600` file's content is never world-readable for the
/// duration of the write. A racer thread `lstat`s the temp name
/// throughout a 32 MiB write and never observes group or other bits;
/// the final file keeps `0600` and the new content.
#[cfg(unix)]
#[test]
fn write_beneath_creates_the_temp_at_the_originals_mode() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use nml_validate::fs::write_beneath;

    let dir = scratch("write-temp-mode");
    let root = dir.join("proj");
    std::fs::create_dir_all(root.join("t")).unwrap();
    let root = std::fs::canonicalize(&root).unwrap();
    let file = root.join("t/f.nml");
    std::fs::write(&file, "old\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let tmp = root.join(format!("t/.f.nml.tmp-{}", std::process::id()));
    let stop = Arc::new(AtomicBool::new(false));
    let racer = {
        let stop = Arc::clone(&stop);
        let tmp = tmp.clone();
        std::thread::spawn(move || {
            let mut seen: Vec<u32> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                if let Ok(meta) = std::fs::symlink_metadata(&tmp) {
                    seen.push(meta.permissions().mode() & 0o777);
                }
            }
            seen
        })
    };
    let contents = vec![b'x'; 32 * 1024 * 1024];
    write_beneath(&root, &["t", "f.nml"], &contents).expect("replaces");
    stop.store(true, Ordering::Relaxed);
    let seen = racer.join().expect("racer");
    let wide: Vec<u32> = seen.iter().copied().filter(|m| m & 0o177 != 0).collect();
    assert!(
        wide.is_empty(),
        "the temp was observed wider than 0600: {wide:?} (observed {} time(s))",
        seen.len()
    );
    let after = std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777;
    assert_eq!(after, 0o600);
    assert_eq!(
        std::fs::metadata(&file).unwrap().len(),
        contents.len() as u64
    );
    eprintln!("racer observed the temp {} time(s)", seen.len());
}

/// The reader's leaf guarantees, through the ONE reader on every
/// platform (there is no by-path arm any more): the open never blocks (a
/// FIFO with no writer parks a plain `open(2)` forever) and never hands
/// back a non-regular file — a FIFO and a directory are refused typed,
/// within the moment; a regular file reads; an absent path is the OS's
/// `NotFound`; and a LEAF that is a symlink is refused, where the
/// deleted by-path open followed it.
#[cfg(unix)]
#[test]
fn the_reader_never_blocks_and_refuses_every_non_regular_leaf() {
    use nml_validate::fs::{MAX_SOURCE_BYTES, ReadError, read_leaf};
    let dir = scratch("reader-leaf");
    std::fs::write(dir.join("f.nml"), "text\n").unwrap();
    std::os::unix::fs::symlink("f.nml", dir.join("l.nml")).unwrap();
    std::fs::create_dir(dir.join("d")).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(dir.join("p.nml"))
        .status()
        .expect("mkfifo runs");
    assert!(made.success());
    let (tx, rx) = std::sync::mpsc::channel();
    let base = dir.0.clone();
    std::thread::spawn(move || {
        let read = |name: &str| {
            read_leaf(
                &base.join(name),
                MAX_SOURCE_BYTES,
                "a declared schema source",
            )
            .map_err(|e| match e {
                ReadError::Open(nml_validate::fs::OpenError::Io(e)) => {
                    format!("io:{:?}", e.kind())
                }
                other => other.to_string(),
            })
        };
        let _ = tx.send((
            read("p.nml"),
            read("d"),
            read("nope.nml"),
            read("f.nml"),
            read("l.nml"),
        ));
    });
    let (fifo, directory, absent, plain, linked) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("every open returned — a FIFO must never block the open");
    assert_eq!(
        fifo.unwrap_err(),
        "`p.nml` is not a regular file (refused at open)"
    );
    assert_eq!(directory.unwrap_err(), "Is a directory (os error 21)");
    assert_eq!(absent.unwrap_err(), "io:NotFound");
    assert_eq!(plain.unwrap(), "text\n");
    assert_eq!(
        linked.unwrap_err(),
        "path component `l.nml` is a symlink (refused at open)",
        "the reader refuses a leaf that is a link; the deleted by-path arm followed it"
    );
}

/// The one reader. `read_beneath` reads a key's file under `cap` (the
/// bound is INCLUSIVE — exactly `cap` bytes read whole, one more is
/// refused in the kernel's one sentence without being read in), refuses
/// bytes that are not UTF-8 typed, and refuses a parent that is a link
/// NOW — whatever a walk classified before — in the chain's sentence;
/// `read_input` is the same read for a discovery input under its root,
/// per-kind cap, the refusal spelled as both front ends print it, a
/// path outside the root refused before any open; `read_leaf` is the
/// same read anchored at a file's own parent, the leaf's swap refused.
#[cfg(unix)]
#[test]
fn the_one_reader_caps_refuses_non_utf8_and_a_parent_that_is_a_link() {
    use nml_validate::fs::{OpenError, ReadError, read_beneath, read_leaf};
    use nml_validate::workspace::{InputKind, input_cap, read_input};
    let dir = scratch("one-reader");
    std::fs::create_dir_all(dir.join("root/tenants/cu")).unwrap();
    std::fs::create_dir_all(dir.join("outside")).unwrap();
    let root = std::fs::canonicalize(dir.join("root")).unwrap();
    std::fs::write(root.join("tenants/cu/f.nml"), vec![b' '; 64]).unwrap();
    std::fs::write(root.join("tenants/cu/g.nml"), vec![b' '; 65]).unwrap();
    std::fs::write(root.join("tenants/cu/bad.nml"), b"v = \"\xff\xfe\"\n").unwrap();
    std::fs::write(dir.join("outside/f.nml"), "OUTSIDE").unwrap();
    let key = ["tenants", "cu", "f.nml"];
    assert_eq!(
        read_beneath(&root, &key, 64, "a test input").unwrap().len(),
        64,
        "exactly the cap reads whole"
    );
    match read_beneath(&root, &["tenants", "cu", "g.nml"], 64, "a test input") {
        Err(ReadError::Refused(sentence)) => assert_eq!(
            sentence,
            "too large: 65 bytes (65 bytes) — a test input is read only up to 64 bytes (64 bytes)"
        ),
        other => panic!("one byte more is refused in the kernel's sentence: {other:?}"),
    }
    let bad = read_beneath(&root, &["tenants", "cu", "bad.nml"], 1024, "a test input").unwrap_err();
    assert!(matches!(bad, ReadError::NotUtf8), "{bad}");
    assert_eq!(bad.to_string(), "not UTF-8");
    // `read_input`: the walk's path, the kind's cap, both front ends' sentence.
    let ws = WorkspaceRoot::explicit(&root, &StdFs).unwrap();
    assert_eq!(
        read_input(&ws, InputKind::Source, &root.join("tenants/cu/f.nml"))
            .unwrap()
            .len(),
        64
    );
    let big = root.join("big.nml");
    std::fs::write(&big, vec![b' '; input_cap(InputKind::Manifest) + 1]).unwrap();
    assert_eq!(
        read_input(&ws, InputKind::Manifest, &big).unwrap_err(),
        "too large: over 256 KiB (262145 bytes) — a package manifest is read only up to 256 KiB \
         (262144 bytes)"
    );
    assert_eq!(
        read_input(&ws, InputKind::Source, &big).unwrap().len(),
        input_cap(InputKind::Manifest) + 1,
        "the same bytes are a fine declared source"
    );
    assert_eq!(
        read_input(&ws, InputKind::Source, &dir.join("outside/f.nml")).unwrap_err(),
        "path is not under the workspace root"
    );
    // The walk classified `tenants/cu` as a directory; it is a link now.
    std::fs::rename(root.join("tenants/cu"), root.join("tenants/cu.real")).unwrap();
    std::os::unix::fs::symlink(dir.join("outside"), root.join("tenants/cu")).unwrap();
    let swapped = read_beneath(&root, &key, 1024, "a test input").unwrap_err();
    assert!(
        matches!(&swapped, ReadError::Open(OpenError::Symlink { component }) if component == "cu"),
        "{swapped}"
    );
    assert_eq!(
        swapped.to_string(),
        "path component `cu` is a symlink (refused at open)"
    );
    assert_eq!(
        read_input(&ws, InputKind::Source, &root.join("tenants/cu/f.nml")).unwrap_err(),
        "path component `cu` is a symlink (refused at open)",
        "the discovery reader's refusal, in both front ends' words"
    );
    // `read_leaf` anchors at the parent as spelled — a link in it is the
    // caller's to have refused (the walk does; the index never hands one
    // out) — and refuses the LEAF's swap: a link, a directory.
    assert_eq!(
        read_leaf(&root.join("tenants/cu.real/f.nml"), 1024, "a test input")
            .unwrap()
            .len(),
        64
    );
    std::os::unix::fs::symlink("f.nml", root.join("tenants/cu.real/l.nml")).unwrap();
    let leaf = read_leaf(&root.join("tenants/cu.real/l.nml"), 1024, "a test input").unwrap_err();
    assert_eq!(
        leaf.to_string(),
        "path component `l.nml` is a symlink (refused at open)"
    );
    let directory = read_leaf(&root.join("tenants/cu.real"), 1024, "a test input").unwrap_err();
    assert!(
        matches!(
            &directory,
            ReadError::Open(OpenError::NotRegular { dir: true, .. })
        ),
        "{directory}"
    );
    assert!(
        directory.to_string().starts_with("Is a directory"),
        "the OS's words, as a path-based open spelled them: {directory}"
    );
    assert!(
        matches!(
            read_leaf(Path::new("/"), 1024, "a test input").unwrap_err(),
            ReadError::Open(OpenError::NotRegular { dir: true, .. })
        ),
        "no file name names no file"
    );
}
