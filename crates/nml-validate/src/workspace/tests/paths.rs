//! Path-kernel pins (step 0b): P1–P4 over the scripted oracle, with the
//! probe log as the executable form of the no-existence-oracle rule.

use super::*;
use crate::fs::{EntryKind, FsError, LstatFs, OverlayFs};
use crate::workspace::mock::{Probe, Spelling};

/// The RFC's operator tree: an `admin/` subtree the tenant must never
/// reach, a tenant subtree with a planted symlink out of it.
fn rfc_tree() -> MockFs {
    MockFs::new()
        .file("/ws/schemas/demo.package.nml")
        .file("/ws/admin/secret.nml")
        .dir("/ws/admin/secretdir")
        .file("/ws/admin/secretdir/x.nml")
        .file("/ws/tenants/cu/member-lookup.flow.nml")
        .symlink("/ws/tenants/cu/lib", "../../admin/secretdir")
        .symlink("/ws/tenants/cu/leaf.nml", "../../admin/secret.nml")
        .symlink("/ws/tenants/cu/dangling", "../../admin/nope")
        .symlink("/ws/tenants/cu/dangling.nml", "../../admin/nope.nml")
}

#[test]
fn mint_resolves_existing_prefix_and_keeps_leaf_lexical() {
    let fs = rfc_tree();
    let root = root_at(&fs, "/ws");
    let keyed = mint(&fs, &root, "tenants/cu/new.nml", Trust::Closed).unwrap();
    assert_eq!(keyed.key.as_str(), "tenants/cu/new.nml");
    assert_eq!(keyed.via_symlink, SymlinkVerdict::None);
    // The leaf does not exist — and minting did not need to know.
    assert_eq!(keyed.verify(&fs).unwrap(), None);
    // An absent PARENT: the remainder is lexical, the leaf cannot exist.
    let keyed = mint(&fs, &root, "tenants/nobody/deep/x.nml", Trust::Closed).unwrap();
    assert_eq!(keyed.key.as_str(), "tenants/nobody/deep/x.nml");
    assert_eq!(keyed.verify(&fs).unwrap(), None);
    assert!(
        !fs.named("deep"),
        "components after an absent one are never probed: {:?}",
        fs.probes()
    );
}

#[test]
fn mint_never_probes_the_leaf() {
    // Structural leaf-avoidance (E26): the leaf name is never held at an
    // oracle call during minting — in either trust, existing or not.
    for trust in [Trust::Closed, Trust::Open] {
        for leaf in ["member-lookup.flow.nml", "absent.nml"] {
            let fs = rfc_tree();
            let root = root_at(&fs, "/ws");
            fs.clear_probes();
            let keyed = mint(&fs, &root, &format!("tenants/cu/{leaf}"), trust).unwrap();
            assert_eq!(keyed.key.as_str(), format!("tenants/cu/{leaf}"));
            let probes = fs.probes();
            assert!(
                !fs.named(leaf),
                "{trust:?}/{leaf}: the leaf was probed: {probes:?}"
            );
            assert_eq!(
                probes,
                [
                    Probe::Child("/ws".into(), "tenants".into()),
                    Probe::Child("/ws/tenants".into(), "cu".into()),
                ],
                "exactly the parent chain, once each"
            );
        }
    }
}

#[test]
fn symlink_target_existence_is_not_observable() {
    // E26 (1): a tenant-planted `lib → ../../admin/secretdir` under a
    // closed binding. Whether the target exists or not, the walk halts at
    // `lib` with the SAME typed error and the SAME lexical key, and the
    // target is never resolved.
    let with_target = rfc_tree();
    let without_target = MockFs::new()
        .file("/ws/schemas/demo.package.nml")
        .file("/ws/tenants/cu/member-lookup.flow.nml")
        .symlink("/ws/tenants/cu/lib", "../../admin/secretdir");
    let mut errors = Vec::new();
    for fs in [&with_target, &without_target] {
        let root = root_at(fs, "/ws");
        fs.clear_probes();
        let err = mint(fs, &root, "tenants/cu/lib/x.nml", Trust::Closed).unwrap_err();
        assert!(
            !fs.probes()
                .iter()
                .any(|p| matches!(p, Probe::ResolveSymlink(..))),
            "closed trust never resolves a symlink: {:?}",
            fs.probes()
        );
        assert!(!fs.named("x.nml"));
        errors.push(err);
    }
    assert_eq!(
        errors[0], errors[1],
        "byte-identical across target existence"
    );
    // The key in the error is the LEXICAL spelling — `admin` never appears.
    assert_eq!(
        errors[0],
        PathError::SymlinkComponent {
            component: "lib".into(),
            key: key("tenants/cu/lib/x.nml"),
        }
    );
}

#[test]
fn dangling_symlink_leaf_is_2083_not_absent() {
    // A symlink LEAF is invisible to minting (leaf-avoidance) and caught
    // by `verify` — as a symlink, dangling or not, never as "absent".
    let fs = rfc_tree();
    let root = root_at(&fs, "/ws");
    for leaf in ["leaf.nml", "dangling.nml"] {
        let keyed = mint(&fs, &root, &format!("tenants/cu/{leaf}"), Trust::Closed).unwrap();
        assert_eq!(
            keyed.via_symlink,
            SymlinkVerdict::None,
            "minting saw no symlink"
        );
        fs.clear_probes();
        let err = keyed.verify(&fs).unwrap_err();
        assert_eq!(
            err,
            PathError::SymlinkComponent {
                component: leaf.into(),
                key: keyed.key.clone()
            },
            "{leaf}"
        );
        assert!(
            !fs.probes()
                .iter()
                .any(|p| matches!(p, Probe::ResolveSymlink(..))),
            "{leaf}: the target is never resolved"
        );
    }
    // Open trust: the leaf verifies AS a symlink, target still unresolved
    // (reading through it is the open context's own business).
    let keyed = mint(&fs, &root, "tenants/cu/dangling.nml", Trust::Open).unwrap();
    let v = keyed.verify(&fs).unwrap().expect("exists as a link");
    assert_eq!(v.kind, EntryKind::Symlink);
    assert_eq!(v.via_symlink, SymlinkVerdict::Through(2));
}

#[test]
fn dotdot_through_symlink_is_caught_after_realpath() {
    // The RFC's three narratives. (1) `admin/../admin/secret.nml`: `..`
    // is the lexical parent of the CANONICAL prefix — the key names
    // `admin/secret.nml` and a deny of `admin/**` catches it.
    let fs = rfc_tree();
    let root = root_at(&fs, "/ws");
    let keyed = mint(&fs, &root, "admin/../admin/secret.nml", Trust::Closed).unwrap();
    assert_eq!(keyed.key.as_str(), "admin/secret.nml");
    assert!(crate::glob::glob_match("admin/**", keyed.key.as_str()));
    // (2) `tenants/cu/lib → ../../admin`: closed trust halts at the link
    // (E26); OPEN trust follows it and the key names the TARGET, so a
    // deny matched against the resolved spelling says `admin`.
    let err = mint(&fs, &root, "tenants/cu/lib/x.nml", Trust::Closed).unwrap_err();
    assert!(matches!(&err, PathError::SymlinkComponent { component, .. } if component == "lib"));
    let keyed = mint(&fs, &root, "tenants/cu/lib/x.nml", Trust::Open).unwrap();
    assert_eq!(keyed.key.as_str(), "admin/secretdir/x.nml");
    assert_eq!(keyed.via_symlink, SymlinkVerdict::Through(2));
    assert!(crate::glob::glob_match("admin/**", keyed.key.as_str()));
    // `..` AFTER a followed link pops the target's component, not the
    // link's authored one (the r48 probe's case (e)).
    let keyed = mint(&fs, &root, "tenants/cu/lib/../secret.nml", Trust::Open).unwrap();
    assert_eq!(keyed.key.as_str(), "admin/secret.nml");
    // (3) tenant re-rooting is a ROOT property: the root is fixed once
    // per invocation, and a key is always relative to it — a tenant
    // `nml-project.nml` cannot re-root a key (`derive_fence_matrix`
    // pins the once-per-invocation walk).
    let keyed = mint(
        &fs,
        &root,
        "tenants/cu/member-lookup.flow.nml",
        Trust::Closed,
    )
    .unwrap();
    assert!(crate::glob::glob_match(
        "tenants/**/*.flow.nml",
        keyed.key.as_str()
    ));
    assert!(!crate::glob::glob_match("*.flow.nml", keyed.key.as_str()));
}

#[test]
fn escapes_never_names_resolved_target() {
    let fs = rfc_tree().file("/etc/passwd").symlink("/ws/out", "/etc");
    let root = root_at(&fs, "/ws");
    for authored in ["../etc/passwd", "tenants/../../x.nml", "out/passwd"] {
        let err = mint(&fs, &root, authored, Trust::Open).unwrap_err();
        match err {
            PathError::Escapes { authored: a } => {
                assert_eq!(a, authored);
                assert!(!a.contains("etc") || authored.contains("etc"));
            }
            other => panic!("{authored}: {other:?}"),
        }
    }
    // Closed trust halts at the link BEFORE it could escape.
    assert!(matches!(
        &mint(&fs, &root, "out/passwd", Trust::Closed).unwrap_err(),
        PathError::SymlinkComponent { component, .. } if component == "out"
    ));
    // An absolute path that never enters the root.
    let err = mint(&fs, &root, "/etc/passwd", Trust::Open).unwrap_err();
    assert_eq!(
        err,
        PathError::Escapes {
            authored: "/etc/passwd".into()
        }
    );
}

#[test]
fn absolute_path_enters_root_through_operator_prefix() {
    // `/tmp → /private/tmp` (macOS): the prefix OUTSIDE the root is the
    // operator's, followed; the components UNDER it are walked by policy.
    let fs = MockFs::new()
        .symlink("/tmp", "/private/tmp")
        .file("/private/tmp/ws/a/x.nml")
        .symlink("/private/tmp/ws/lnk", "a");
    let root = root_at(&fs, "/tmp/ws");
    assert_eq!(root.path(), Path::new("/private/tmp/ws"));
    let keyed = mint(&fs, &root, "/tmp/ws/a/x.nml", Trust::Closed).unwrap();
    assert_eq!(keyed.key.as_str(), "a/x.nml");
    assert_eq!(keyed.verify(&fs).unwrap().unwrap().kind, EntryKind::File);
    // Under the root, closed trust still halts on a symlink.
    assert!(matches!(
        &mint(&fs, &root, "/tmp/ws/lnk/x.nml", Trust::Closed).unwrap_err(),
        PathError::SymlinkComponent { component, .. } if component == "lnk"
    ));
    assert_eq!(
        root.path_of(&keyed.key),
        Path::new("/private/tmp/ws/a/x.nml")
    );
}

#[test]
fn eacces_on_ancestor_fails_closed() {
    let fs = rfc_tree().denied("/ws/locked");
    let root = root_at(&fs, "/ws");
    // Identical whether or not the leaf exists: EACCES is existence-blind.
    for leaf in ["in.nml", "nope.nml"] {
        let err = mint(&fs, &root, &format!("locked/sub/{leaf}"), Trust::Closed).unwrap_err();
        assert_eq!(err, PathError::Fs(FsError::Denied));
    }
    // Even for the leaf itself: the parent probes fine, `verify` is denied.
    let keyed = mint(&fs, &root, "locked/x.nml", Trust::Closed).unwrap();
    assert_eq!(
        keyed.verify(&fs).unwrap_err(),
        PathError::Fs(FsError::Denied)
    );
}

#[test]
fn eloop_fails_closed() {
    let fs = rfc_tree()
        .symlink("/ws/loop1", "loop2")
        .symlink("/ws/loop2", "loop1");
    let root = root_at(&fs, "/ws");
    // Closed: halts at the link (a loop is still a symlink component).
    assert!(matches!(
        &mint(&fs, &root, "loop1/x.nml", Trust::Closed).unwrap_err(),
        PathError::SymlinkComponent { component, .. } if component == "loop1"
    ));
    // Open: resolving it is ELOOP, typed.
    assert_eq!(
        mint(&fs, &root, "loop1/x.nml", Trust::Open).unwrap_err(),
        PathError::Fs(FsError::SymlinkLoop)
    );
}

#[test]
fn norealpath_is_unverifiable_not_lexical_escape() {
    // The wasi regime: spelling proven by byte-exact listing membership.
    // An exact lookup verifies; a respelled lookup on an insensitive
    // filesystem is UNVERIFIABLE — closed trust fails closed (form 2),
    // open trust proceeds lexically and says so. Never a silent escape.
    let fs = rfc_tree()
        .dir("/ws/Admin")
        .file("/ws/Admin/s.nml")
        .insensitive()
        .spelling(Spelling::Membership);
    let root = root_at(&fs, "/ws");
    let keyed = mint(&fs, &root, "Admin/s.nml", Trust::Closed).unwrap();
    assert_eq!(keyed.via_symlink, SymlinkVerdict::None);
    assert_eq!(
        keyed.verify(&fs).unwrap().unwrap().key.as_str(),
        "Admin/s.nml"
    );
    let err = mint(&fs, &root, "ADMIN/s.nml", Trust::Closed).unwrap_err();
    assert_eq!(
        err,
        PathError::Unverifiable {
            key: key("ADMIN/s.nml")
        }
    );
    let keyed = mint(&fs, &root, "ADMIN/s.nml", Trust::Open).unwrap();
    assert_eq!(keyed.key.as_str(), "ADMIN/s.nml", "open: lexical");
    assert_eq!(keyed.via_symlink, SymlinkVerdict::Unverifiable);
    // A symlink on the wasi backend: closed halts (lstat works there);
    // open cannot resolve it and marks the key unverifiable.
    assert!(matches!(
        &mint(&fs, &root, "tenants/cu/lib/x.nml", Trust::Closed).unwrap_err(),
        PathError::SymlinkComponent { component, .. } if component == "lib"
    ));
    let keyed = mint(&fs, &root, "tenants/cu/lib/x.nml", Trust::Open).unwrap();
    assert_eq!(keyed.key.as_str(), "tenants/cu/lib/x.nml");
    assert_eq!(keyed.via_symlink, SymlinkVerdict::Through(2));
}

#[test]
fn verify_reports_respelled_leaf() {
    // A11: the parent chain is respelled at mint (`ADMIN` → `Admin`); the
    // leaf stays authored until `verify`, which reports the on-disk
    // spelling and flags the respelling for the deny-first re-judge.
    let fs = MockFs::new()
        .file("/ws/Admin/s.nml")
        .file("/ws/x.package.nml")
        .insensitive();
    let root = root_at(&fs, "/ws");
    let keyed = mint(&fs, &root, "ADMIN/S.NML", Trust::Closed).unwrap();
    assert_eq!(
        keyed.key.as_str(),
        "Admin/S.NML",
        "parent respelled, leaf authored"
    );
    let v = keyed.verify(&fs).unwrap().unwrap();
    assert_eq!(v.key.as_str(), "Admin/s.nml");
    assert!(v.respelled);
    assert_eq!(v.kind, EntryKind::File);
    let keyed = mint(&fs, &root, "Admin/s.nml", Trust::Closed).unwrap();
    assert!(!keyed.verify(&fs).unwrap().unwrap().respelled);
}

#[test]
fn a_trailing_dotdot_names_no_file() {
    let fs = rfc_tree();
    let root = root_at(&fs, "/ws");
    assert_eq!(
        mint(&fs, &root, "tenants/..", Trust::Open).unwrap_err(),
        PathError::NotRelative
    );
}

#[test]
fn an_alias_into_the_interior_cannot_leave_the_root_by_dotdot_and_re_enter() {
    // r80-cov (mutant P7 survived): the fold consumes `.`/`..` only AT
    // the root. Through an operator alias INTO the root's interior the
    // walk starts strictly inside, so a `..` chain that pops past the
    // root is the policy walk's `Escapes` — never a physical pop that
    // re-enters the root through the operator's territory (E29(2), E30).
    let fs = rfc_tree()
        .file("/ws/x.nml")
        .symlink("/srv/alias-cu", "/ws/tenants/cu");
    let root = root_at(&fs, "/ws");
    fs.clear_probes();
    assert!(matches!(
        mint(&fs, &root, "/srv/alias-cu/../../../ws/x.nml", Trust::Closed).unwrap_err(),
        PathError::Escapes { .. }
    ));
    assert_eq!(
        fs.probes()
            .iter()
            .filter(|p| matches!(p, Probe::ResolveSymlink(..)))
            .count(),
        1,
        "only the operator alias is resolved: {:?}",
        fs.probes()
    );
    // A `..` that stays inside pops lexically and keys the interior.
    let keyed = mint(
        &fs,
        &root,
        "/srv/alias-cu/../cu/member-lookup.flow.nml",
        Trust::Closed,
    )
    .unwrap();
    assert_eq!(keyed.key.as_str(), "tenants/cu/member-lookup.flow.nml");
}

#[test]
fn depth_is_bounded() {
    let fs = MockFs::new().file("/ws/x.package.nml");
    let root = root_at(&fs, "/ws");
    let deep = (0..64).map(|_| "d").collect::<Vec<_>>().join("/") + "/x.nml";
    assert_eq!(
        mint(&fs, &root, &deep, Trust::Closed).unwrap_err(),
        PathError::Depth
    );
    let ok = (0..63).map(|_| "d").collect::<Vec<_>>().join("/") + "/x.nml";
    assert_eq!(
        mint(&fs, &root, &ok, Trust::Closed).unwrap().key.depth(),
        64
    );
}

#[test]
fn key_invariants_and_navigation() {
    let fs = MockFs::new().file("/ws/x.package.nml");
    let root = root_at(&fs, "/ws");
    let keyed = mint(&fs, &root, "./a/./b/../c/x.nml", Trust::Open).unwrap();
    assert_eq!(keyed.key.as_str(), "a/c/x.nml");
    let key = keyed.key;
    assert_eq!(key.dir().as_str(), "a/c");
    assert_eq!(key.file_name(), "x.nml");
    assert_eq!(key.depth(), 3);
    assert!(SourceKey::root().is_strict_ancestor_of(&key));
    assert!(key.dir().is_strict_ancestor_of(&key));
    assert!(!key.is_strict_ancestor_of(&key));
    assert!(key.dir().contains(&key.dir()));
    assert!(!super::key("a/cx").is_strict_ancestor_of(&key));
    assert_eq!(key.relative_to(&super::key("a")), Some("c/x.nml"));
    assert_eq!(key.relative_to(&SourceKey::root()), Some("a/c/x.nml"));
    assert_eq!(key.relative_to(&super::key("b")), None);
    assert_eq!(root.path_of(&key), Path::new("/ws/a/c/x.nml"));
    assert_eq!(root.path_of(&SourceKey::root()), Path::new("/ws"));
    // Idempotent: minting the key's own path yields the key.
    let again = mint(
        &fs,
        &root,
        &root.path_of(&key).display().to_string(),
        Trust::Open,
    )
    .unwrap();
    assert_eq!(again.key, key);
}

#[test]
fn derive_fence_matrix() {
    // E21. A `.git` entry of ANY kind is the fence: directory, file
    // (worktree/submodule), symlink. The operator's manifest sits at the
    // repo root — the only layout in which it governs `tenants/**` at all
    // (R5′: a manifest governs its own subtree), and the one the tenant's
    // `nml-project.nml` must not re-root.
    // A manifest ABOVE the fence (`/srv/evil.package.nml`, E21's hostile
    // manifest in a shared parent) never becomes the universe. Above a
    // DIRECTORY fence — a real checkout — E21's fence keeps it out and
    // the shadow check (r80-sec F6) DISCLOSES it; above a fence that is
    // no directory (a `.git` file or link: the shapes git writes for a
    // submodule or a tenant plants) it REFUSES derivation: the operator
    // names the root.
    for git in ["dir", "file", "symlink"] {
        let mut fs = MockFs::new()
            .file("/srv/evil.package.nml")
            .file("/srv/app/demo.package.nml")
            .file("/srv/app/tenants/cu/nml-project.nml")
            .file("/srv/app/tenants/cu/x.nml");
        fs = match git {
            "dir" => fs.dir("/srv/app/.git"),
            "file" => fs.file("/srv/app/.git"),
            _ => fs.symlink("/srv/app/.git", "/elsewhere"),
        };
        let derived = WorkspaceRoot::derive(Path::new("/srv/app/tenants/cu/x.nml"), &fs);
        if git == "dir" {
            let root = derived.unwrap();
            assert_eq!(root.path(), Path::new("/srv/app"), "{git}: the fence holds");
            assert_eq!(
                root.origin,
                RootOrigin::Derived {
                    fence: Fence::Vcs {
                        kind: EntryKind::Dir
                    },
                    shadowed: Some(Shadow::Marker(PathBuf::from("/srv/evil.package.nml"))),
                },
                "{git}: a manifest above a directory fence is disclosed, never governs"
            );
        } else {
            assert_eq!(
                derived.unwrap_err(),
                RootError::Shadowed {
                    marker: PathBuf::from("/srv/evil.package.nml"),
                    fence: PathBuf::from("/srv/app/.git"),
                },
                "{git}: a manifest above a non-directory fence refuses, never governs"
            );
        }
        let fs = MockFs::new()
            .file("/srv/app/demo.package.nml")
            .file("/srv/app/tenants/cu/nml-project.nml")
            .file("/srv/app/tenants/cu/x.nml");
        let fs = match git {
            "dir" => fs.dir("/srv/app/.git"),
            "file" => fs.file("/srv/app/.git"),
            _ => fs.symlink("/srv/app/.git", "/elsewhere"),
        };
        let root = WorkspaceRoot::derive(Path::new("/srv/app/tenants/cu/x.nml"), &fs).unwrap();
        assert_eq!(
            root.path(),
            Path::new("/srv/app"),
            "{git}: the outermost manifest within the fence"
        );
        let kind = match git {
            "dir" => EntryKind::Dir,
            "file" => EntryKind::File,
            _ => EntryKind::Symlink,
        };
        assert_eq!(
            root.origin,
            RootOrigin::Derived {
                fence: Fence::Vcs { kind },
                shadowed: None
            },
            "{git}: the fence entry's kind rides the origin"
        );
        // A tenant `nml-project.nml` between the target and the root does
        // not re-root: outermost wins.
        assert_ne!(root.path(), Path::new("/srv/app/tenants/cu"));
    }
    // An operator manifest in a SIBLING subtree is not on the target's
    // ancestor chain: it cannot govern the target (R5′), and the derived
    // root is the tenant's own directory — an open universe the tenant
    // cannot escape (keys are root-relative), not a re-rooted claim.
    let fs = MockFs::new()
        .dir("/srv/app/.git")
        .file("/srv/app/schemas/demo.package.nml")
        .file("/srv/app/tenants/cu/nml-project.nml")
        .file("/srv/app/tenants/cu/x.nml");
    let root = WorkspaceRoot::derive(Path::new("/srv/app/tenants/cu/x.nml"), &fs).unwrap();
    assert_eq!(root.path(), Path::new("/srv/app/tenants/cu"));
    // Outermost of two manifests within the fence.
    let fs = MockFs::new()
        .dir("/srv/app/.git")
        .file("/srv/app/demo.package.nml")
        .file("/srv/app/apps/core/core.package.nml")
        .file("/srv/app/apps/core/x.nml");
    let root = WorkspaceRoot::derive(Path::new("/srv/app/apps/core/x.nml"), &fs).unwrap();
    assert_eq!(root.path(), Path::new("/srv/app"));
    // No VCS root anywhere ⇒ the target's own directory fences the walk:
    // a manifest in a shared parent (`/tmp`) is never the universe.
    let fs = MockFs::new()
        .file("/tmp/evil.package.nml")
        .file("/tmp/build/x.nml");
    let root = WorkspaceRoot::derive(Path::new("/tmp/build/x.nml"), &fs).unwrap();
    assert_eq!(root.path(), Path::new("/tmp/build"));
    assert_eq!(
        root.origin,
        RootOrigin::Derived {
            fence: Fence::TargetDir,
            shadowed: None
        }
    );
    // A manifest IN the target dir with no VCS root: that dir is the root.
    let fs = MockFs::new()
        .file("/tmp/build/mine.package.nml")
        .file("/tmp/build/x.nml");
    let root = WorkspaceRoot::derive(Path::new("/tmp/build/x.nml"), &fs).unwrap();
    assert_eq!(root.path(), Path::new("/tmp/build"));
    // No manifest anywhere ⇒ the target dir, an open universe.
    let fs = MockFs::new().dir("/srv/app/.git").file("/srv/app/a/x.nml");
    let root = WorkspaceRoot::derive(Path::new("/srv/app/a/x.nml"), &fs).unwrap();
    assert_eq!(root.path(), Path::new("/srv/app/a"));
    assert_eq!(
        root.origin,
        RootOrigin::Derived {
            fence: Fence::Vcs {
                kind: EntryKind::Dir
            },
            shadowed: None
        }
    );
    // The 64-component work bound: a `.git` 70 levels up is never
    // reached, and the walk derives NOTHING — closed-denied, naming the
    // last directory it visited — never the smaller OPEN universe the
    // target's depth would choose (r80-sec F9: a file 70 directories
    // below the operator's manifest validated under no binding, `ok`).
    let deep: String = (0..70).map(|i| format!("/d{i}")).collect();
    let fs = MockFs::new()
        .dir("/.git")
        .file("/evil.package.nml")
        .file(&format!("{deep}/x.nml"));
    let err = WorkspaceRoot::derive(Path::new(&format!("{deep}/x.nml")), &fs).unwrap_err();
    let RootError::ComponentCap { last } = &err else {
        panic!("{err:?}");
    };
    assert!(last.starts_with("/d0/d1/d2/d3/d4/d5/d6"), "{err}");
    assert_ne!(last, Path::new("/"));
    assert!(
        err.to_string()
            .contains("no VCS root within 64 directories above"),
        "{err}"
    );
    // A directory target (`nml fix <dir>`) fences from itself.
    let fs = MockFs::new().dir("/srv/app/.git").file("/srv/app/a/x.nml");
    let root = WorkspaceRoot::derive(Path::new("/srv/app/a"), &fs).unwrap();
    assert_eq!(root.path(), Path::new("/srv/app/a"));
    // Relative targets are the caller's to absolutize.
    assert_eq!(
        WorkspaceRoot::derive(Path::new("a/x.nml"), &fs).unwrap_err(),
        RootError::NotAbsolute
    );
}

#[test]
fn explicit_root_rejects_outside_target() {
    let fs = MockFs::new()
        .file("/srv/app/x.nml")
        .file("/srv/other/y.nml")
        .symlink("/srv/link", "app");
    let root = WorkspaceRoot::explicit(Path::new("/srv/app"), &fs).unwrap();
    assert_eq!(root.origin, RootOrigin::Explicit);
    assert_eq!(
        mint(&fs, &root, "/srv/other/y.nml", Trust::Closed).unwrap_err(),
        PathError::Escapes {
            authored: "/srv/other/y.nml".into()
        }
    );
    assert_eq!(
        mint(&fs, &root, "/srv/app/x.nml", Trust::Closed)
            .unwrap()
            .key
            .as_str(),
        "x.nml"
    );
    // An operator-side symlink INTO the root is followed (it is not
    // author-writable content).
    let via = WorkspaceRoot::explicit(Path::new("/srv/link"), &fs).unwrap();
    assert_eq!(via.path(), Path::new("/srv/app"));
    assert_eq!(
        mint(&fs, &root, "/srv/link/x.nml", Trust::Closed)
            .unwrap()
            .key
            .as_str(),
        "x.nml"
    );
    // Not a directory / absent / relative.
    assert_eq!(
        WorkspaceRoot::explicit(Path::new("/srv/app/x.nml"), &fs).unwrap_err(),
        RootError::NotADirectory
    );
    assert_eq!(
        WorkspaceRoot::explicit(Path::new("/srv/nope"), &fs).unwrap_err(),
        RootError::NotADirectory
    );
    assert_eq!(
        WorkspaceRoot::explicit(Path::new("srv/app"), &fs).unwrap_err(),
        RootError::NotAbsolute
    );
}

#[test]
fn overlay_buffers_exist_as_files() {
    // The editor's unsaved buffer at a path whose directory does not exist
    // on disk: the directories the buffer implies exist as `Dir` for the
    // overlay (E28), so the walk reaches the buffer and `verify` finds it.
    let disk = MockFs::new().file("/ws/x.package.nml");
    let buffers = [std::path::PathBuf::from("/ws/new/dir/f.nml")];
    let fs = OverlayFs {
        disk: &disk,
        buffers: &buffers,
    };
    let root = WorkspaceRoot::explicit(Path::new("/ws"), &fs).unwrap();
    let keyed = SourceKey::mint(&root, Path::new("new/dir/f.nml"), &fs, Trust::Closed).unwrap();
    assert_eq!(keyed.key.as_str(), "new/dir/f.nml");
    let v = keyed.verify(&fs).unwrap().expect("the buffer exists");
    assert_eq!((v.kind, v.respelled), (EntryKind::File, false));
    assert_eq!(
        fs.child(Path::new("/ws"), std::ffi::OsStr::new("new"))
            .unwrap()
            .map(|s| s.kind),
        Some(EntryKind::Dir),
        "a buffer-only directory is a directory"
    );
    assert_eq!(
        fs.list_dir(Path::new("/ws/new")).unwrap(),
        [(std::ffi::OsString::from("dir"), EntryKind::Dir)]
    );
    assert_eq!(
        fs.list_dir(Path::new("/ws/new/dir")).unwrap(),
        [(std::ffi::OsString::from("f.nml"), EntryKind::File)]
    );
    // A buffer under an EXISTING directory verifies as a file.
    let buffers = [std::path::PathBuf::from("/ws/g.nml")];
    let fs = OverlayFs {
        disk: &disk,
        buffers: &buffers,
    };
    let keyed = SourceKey::mint(&root, Path::new("g.nml"), &fs, Trust::Closed).unwrap();
    let v = keyed.verify(&fs).unwrap().unwrap();
    assert_eq!((v.kind, v.respelled), (EntryKind::File, false));
}

#[test]
fn overlay_never_swallows_a_denied_listing() {
    // E28 (probe F): a buffer under a directory the disk REFUSES to list
    // does not turn the refusal into "just buffers" — a truncation would
    // go unseen at 0e. Only a directory absent on disk lists buffers
    // alone; an entry the disk has keeps the disk's kind.
    let disk = MockFs::new()
        .file("/ws/x.package.nml")
        .denied("/ws/locked")
        .symlink("/ws/lnk.nml", "x.package.nml");
    let buffers = [
        std::path::PathBuf::from("/ws/locked/new.nml"),
        std::path::PathBuf::from("/ws/lnk.nml"),
    ];
    let fs = OverlayFs {
        disk: &disk,
        buffers: &buffers,
    };
    assert_eq!(
        fs.list_dir(Path::new("/ws/locked")).unwrap_err(),
        FsError::Denied
    );
    assert_eq!(
        fs.child(Path::new("/ws"), std::ffi::OsStr::new("lnk.nml"))
            .unwrap()
            .map(|s| s.kind),
        Some(EntryKind::Symlink),
        "a buffer opened through a link is still a link (E26)"
    );
}

#[test]
fn dotdot_after_absent_component_resumes_probing() {
    // E28 (3): `..` popping back to the resolved prefix RESUMES the walk,
    // so a symlink reached as `tenants/nobody/../cu/lib` halts exactly as
    // `tenants/cu/lib` does — same error, same key, `lib` probed. The
    // pre-fix walk keyed `tenants/cu/lib/x.nml` straight through the link.
    let fs = rfc_tree();
    let root = root_at(&fs, "/ws");
    let direct = mint(&fs, &root, "tenants/cu/lib/x.nml", Trust::Closed).unwrap_err();
    for spelling in [
        "tenants/nobody/../cu/lib/x.nml",
        "tenants/cu/member-lookup.flow.nml/../lib/x.nml",
        "tenants/cu/../cu/lib/x.nml",
        "tenants/nobody/deep/../../cu/lib/x.nml",
    ] {
        fs.clear_probes();
        let err = mint(&fs, &root, spelling, Trust::Closed).unwrap_err();
        assert_eq!(err, direct, "{spelling}");
        assert!(
            fs.named("lib"),
            "{spelling}: `lib` was never probed: {:?}",
            fs.probes()
        );
        assert!(!fs.named("x.nml"));
    }
    // A symlink LEAF reached the same way is caught by `verify`.
    let keyed = mint(&fs, &root, "tenants/nobody/../cu/leaf.nml", Trust::Closed).unwrap();
    assert_eq!(keyed.key.as_str(), "tenants/cu/leaf.nml");
    assert!(matches!(
        &keyed.verify(&fs).unwrap_err(),
        PathError::SymlinkComponent { component, .. } if component == "leaf.nml"
    ));
    // And a plain file reached that way verifies — the resumed prefix is
    // the real one.
    let keyed = mint(
        &fs,
        &root,
        "tenants/nobody/../cu/member-lookup.flow.nml",
        Trust::Closed,
    )
    .unwrap();
    assert_eq!(keyed.verify(&fs).unwrap().unwrap().kind, EntryKind::File);
    // r69b (cov r68 M2): a `..` that pops a RESOLVED component must move
    // the probed prefix back too. `cu/../cu` returns to the directory the
    // walk was in, so a stale prefix is coincidentally right there; these
    // rows leave it — `admin/..` pops to the root, and `tenants` is then
    // probed under the ROOT, not under `admin` (where it is absent, so
    // the pre-fix walk went lexical and keyed straight through `lib`
    // without ever probing it).
    for spelling in [
        "admin/../tenants/cu/lib/x.nml",
        "admin/secretdir/../../tenants/cu/lib/x.nml",
        "schemas/../tenants/cu/lib/x.nml",
    ] {
        fs.clear_probes();
        let err = mint(&fs, &root, spelling, Trust::Closed).unwrap_err();
        assert_eq!(err, direct, "{spelling}");
        assert!(
            fs.probes()
                .contains(&Probe::Child(PathBuf::from("/ws/tenants/cu"), "lib".into())),
            "{spelling}: `lib` was not probed under the resumed prefix: {:?}",
            fs.probes()
        );
    }
    // And a plain file reached that way VERIFIES under the resumed
    // prefix (a stale one reports it absent).
    let keyed = mint(
        &fs,
        &root,
        "admin/../tenants/cu/member-lookup.flow.nml",
        Trust::Closed,
    )
    .unwrap();
    assert_eq!(keyed.key.as_str(), "tenants/cu/member-lookup.flow.nml");
    assert_eq!(keyed.verify(&fs).unwrap().unwrap().kind, EntryKind::File);
    // The invariant the fuzz target states: under closed trust, no
    // EXISTING component of a minted key's parent chain is a symlink.
    for spelling in [
        "tenants/nobody/../cu/new.nml",
        "tenants/cu/../cu/../cu/x.nml",
    ] {
        let keyed = mint(&fs, &root, spelling, Trust::Closed).unwrap();
        let mut prefix = root.path().to_path_buf();
        for component in keyed.key.dir().components() {
            prefix.push(component);
            assert_ne!(fs.kind_at(&prefix), Some(EntryKind::Symlink), "{spelling}");
        }
    }
}

#[test]
fn halt_names_the_component_and_spells_the_tail_lexically() {
    // E28 (6): the halted component travels by NAME, not by an index into
    // the authored path read against the key (`./tenants/cu/lib/x.nml`
    // named `x.nml`; `tenants/cu/../cu/lib/x.nml` named `?`), and the
    // tail's `..` pops lexically through the halting component
    // (`lib/../lib/x.nml` spells `lib/x.nml`, never `lib/lib/x.nml`).
    let fs = rfc_tree();
    let root = root_at(&fs, "/ws");
    for spelling in [
        "./tenants/cu/lib/x.nml",
        "tenants/cu/../cu/lib/x.nml",
        "tenants/./cu/lib/../lib/x.nml",
        "tenants/cu/lib/./x.nml",
    ] {
        assert_eq!(
            mint(&fs, &root, spelling, Trust::Closed).unwrap_err(),
            PathError::SymlinkComponent {
                component: "lib".into(),
                key: key("tenants/cu/lib/x.nml"),
            },
            "{spelling}"
        );
    }
    let d = crate::workspace::diag::path_finding(
        &mint(&fs, &root, "./tenants/cu/lib/x.nml", Trust::Closed).unwrap_err(),
    )
    .unwrap();
    assert!(
        d.message.contains("path component `lib` is a symlink"),
        "{}",
        d.message
    );
}

#[test]
fn fold_stops_on_entering_the_root() {
    // E28 (5): an OPERATOR link into the root's interior (`/srv/deep →
    // /ws/tenants/cu`) plus an AUTHOR link back to the root (`toroot →
    // /ws`): the fold stops the moment it lands inside the root and hands
    // the components under it to the policy walk, which halts at the
    // author's link under closed trust — `admin/secret.nml` is never
    // keyed, and nothing is resolved once inside the root.
    let fs = rfc_tree()
        .symlink("/srv/deep", "/ws/tenants/cu")
        .symlink("/ws/tenants/cu/toroot", "/ws");
    let root = root_at(&fs, "/ws");
    fs.clear_probes();
    let err = mint(
        &fs,
        &root,
        "/srv/deep/toroot/admin/secret.nml",
        Trust::Closed,
    )
    .unwrap_err();
    assert_eq!(
        err,
        PathError::SymlinkComponent {
            component: "toroot".into(),
            key: key("tenants/cu/toroot/admin/secret.nml"),
        }
    );
    let resolves: Vec<Probe> = fs
        .probes()
        .into_iter()
        .filter(|p| matches!(p, Probe::ResolveSymlink(..)))
        .collect();
    assert_eq!(
        resolves,
        [Probe::ResolveSymlink("/srv".into(), "deep".into())],
        "exactly the operator link outside the root is resolved"
    );
    assert!(!fs.named("secret.nml"));
    // The same entry keys a plain file under the interior directory.
    let keyed = mint(
        &fs,
        &root,
        "/srv/deep/member-lookup.flow.nml",
        Trust::Closed,
    )
    .unwrap();
    assert_eq!(keyed.key.as_str(), "tenants/cu/member-lookup.flow.nml");
    assert!(matches!(
        &mint(&fs, &root, "/srv/deep/lib/x.nml", Trust::Closed).unwrap_err(),
        PathError::SymlinkComponent { component, .. } if component == "lib"
    ));
}

#[test]
fn dotdot_popping_the_root_itself_is_not_an_escape() {
    // r51 #4: an absolute target that leaves the root by `..` and
    // re-enters it through the canonical parent names a file INSIDE the
    // root — `cwd=/ws; nml check ../ws/x.nml` is this spelling. Pre-fix
    // the fold stopped AT the root before consuming the `..`, the policy
    // walk popped past its empty prefix, and the kernel said `Escapes`.
    // The fold consumes `.`/`..` while at the root (a physical pop of a
    // canonical prefix) and the policy walk starts at the first plain
    // component under it; a genuine escape stays one; nothing is
    // resolved once inside (E28 (5)).
    let fs = rfc_tree()
        .file("/ws/tenants/cu/real/y.nml")
        .file("/other/x.nml")
        .symlink("/srv/toroot", "/ws");
    let root = root_at(&fs, "/ws");
    fs.clear_probes();
    let keyed = mint(&fs, &root, "/ws/../ws/tenants/cu/real/y.nml", Trust::Closed).unwrap();
    assert_eq!(keyed.key.as_str(), "tenants/cu/real/y.nml");
    assert_eq!(keyed.verify(&fs).unwrap().unwrap().kind, EntryKind::File);
    assert!(
        !fs.probes()
            .iter()
            .any(|p| matches!(p, Probe::ResolveSymlink(..))),
        "{:?}",
        fs.probes()
    );
    // Through an operator link to the root, out by `..`, and back in.
    fs.clear_probes();
    let keyed = mint(&fs, &root, "/srv/toroot/../ws/x.nml", Trust::Closed).unwrap();
    assert_eq!(keyed.key.as_str(), "x.nml");
    let resolves: Vec<Probe> = fs
        .probes()
        .into_iter()
        .filter(|p| matches!(p, Probe::ResolveSymlink(..)))
        .collect();
    assert_eq!(
        resolves,
        [Probe::ResolveSymlink("/srv".into(), "toroot".into())],
        "only the operator link outside the root"
    );
    // `.` at the root, and a `..` chain that runs past the filesystem root.
    for authored in [
        "/ws/./tenants/cu/real/y.nml",
        "/ws/../../ws/tenants/cu/real/y.nml",
        "/ws/../ws/./tenants/cu/real/y.nml",
    ] {
        assert_eq!(
            mint(&fs, &root, authored, Trust::Closed)
                .unwrap()
                .key
                .as_str(),
            "tenants/cu/real/y.nml",
            "{authored}"
        );
    }
    // A genuine escape through the parent is still `Escapes` — the
    // authored spelling only, existing sibling or not.
    for authored in ["/ws/../other/x.nml", "/ws/../nope/x.nml", "/ws/.."] {
        assert_eq!(
            mint(&fs, &root, authored, Trust::Closed).unwrap_err(),
            PathError::Escapes {
                authored: authored.into()
            },
            "{authored}"
        );
    }
    // Strictly INSIDE the root a `..` is the policy walk's — lexical on
    // the canonical prefix, an escape past the root before any probe —
    // so the fold never re-enters the operator's territory from inside.
    assert!(matches!(
        mint(&fs, &root, "/ws/tenants/../../ws/x.nml", Trust::Closed).unwrap_err(),
        PathError::Escapes { .. }
    ));
    // And under the root the closed walk still halts at an author link
    // reached this way.
    assert!(matches!(
        &mint(&fs, &root, "/ws/../ws/tenants/cu/lib/x.nml", Trust::Closed).unwrap_err(),
        PathError::SymlinkComponent { component, .. } if component == "lib"
    ));
}

#[test]
fn root_marker_must_be_a_regular_file() {
    // r51 #13: a symlink named `*.package.nml` is not a root marker —
    // discovery never loads a link, so it must not anchor a universe
    // either. Pre-fix `holds_root_marker` accepted any non-directory
    // entry, and the derived root landed on the link's directory.
    let fs = MockFs::new()
        .dir("/ws/.git")
        .symlink("/ws/sub/lnk.package.nml", "/elsewhere/real.package.nml")
        .file("/elsewhere/real.package.nml")
        .file("/ws/sub/app/x.nml");
    let root = WorkspaceRoot::derive(Path::new("/ws/sub/app/x.nml"), &fs).unwrap();
    assert_eq!(
        root.path(),
        Path::new("/ws/sub/app"),
        "no marker on the chain: the target's own directory"
    );
    assert!(!fs.named("real.package.nml"));
    // The same name as a regular file IS a marker.
    let fs = MockFs::new()
        .dir("/ws/.git")
        .file("/ws/sub/mine.package.nml")
        .file("/ws/sub/app/x.nml");
    let root = WorkspaceRoot::derive(Path::new("/ws/sub/app/x.nml"), &fs).unwrap();
    assert_eq!(root.path(), Path::new("/ws/sub"));
}

/// The unfenced-link refusal holds for every SPELLING of the target's
/// own directory: `link/sub/../x.nml` names `link` as that directory as
/// surely as `link/x.nml` does, and used to derive the universe through
/// the link (root = the link's target) where the plain spelling
/// refused; a tail that climbs above the link (`link/../x.nml`) keeps
/// its verdict, and a genuinely deeper target (`link/sub/x.nml`) is
/// still the operator's followed prefix.
#[test]
fn derive_refuses_the_unfenced_link_however_the_own_directory_is_spelled() {
    let fs = MockFs::new()
        .file("/srv/ws/demo.package.nml")
        .dir("/srv/ws/vendor/sub")
        .symlink("/srv/ws/tenants/cu/lib", "../../vendor");
    let refused = RootError::UnfencedSymlink {
        component: "lib".into(),
    };
    for spelling in [
        "/srv/ws/tenants/cu/lib/x.nml",
        "/srv/ws/tenants/cu/lib/sub/../x.nml",
        "/srv/ws/tenants/cu/lib/sub/./../x.nml",
        "/srv/ws/tenants/cu/lib/a/b/../../x.nml",
        "/srv/ws/tenants/cu/lib/../plain.nml",
    ] {
        assert_eq!(
            WorkspaceRoot::derive(Path::new(spelling), &fs).unwrap_err(),
            refused,
            "{spelling}"
        );
    }
    let deeper = WorkspaceRoot::derive(Path::new("/srv/ws/tenants/cu/lib/sub/x.nml"), &fs).unwrap();
    assert_eq!(deeper.path(), Path::new("/srv/ws/vendor/sub"));
}

#[test]
fn derive_never_resolves_an_author_link() {
    // E28 (2): under a `.git` fence the derivation is lstat-only below
    // the fence — it stops BEFORE `tenants/cu/lib` whether the link's
    // target exists or not, derives the same root, and never calls
    // `resolve_symlink` (the pre-fix derivation canonicalized through
    // the link: a different root per target existence, and `ok` for the
    // existing one with no VCS).
    let mut roots = Vec::new();
    for with_target in [true, false] {
        let mut fs = MockFs::new()
            .dir("/ws/.git")
            .file("/ws/demo.package.nml")
            .symlink("/ws/tenants/cu/lib", "../../vendor");
        if with_target {
            fs = fs.file("/ws/vendor/base.flow.nml");
        }
        fs.clear_probes();
        let root =
            WorkspaceRoot::derive(Path::new("/ws/tenants/cu/lib/base.flow.nml"), &fs).unwrap();
        assert!(
            !fs.probes()
                .iter()
                .any(|p| matches!(p, Probe::ResolveSymlink(..))),
            "derive resolved a link below the fence: {:?}",
            fs.probes()
        );
        roots.push(root);
    }
    assert_eq!(roots[0], roots[1]);
    assert_eq!(roots[0].path(), Path::new("/ws"));
    // A leaf symlink out of the repo never becomes the root (D2): the
    // leaf is the trust-aware walk's, not the derivation's.
    let fs = MockFs::new()
        .dir("/ws/.git")
        .file("/ws/demo.package.nml")
        .file("/etc/passwd")
        .symlink("/ws/tenants/cu/leaf.nml", "/etc/passwd");
    let root = WorkspaceRoot::derive(Path::new("/ws/tenants/cu/leaf.nml"), &fs).unwrap();
    assert_eq!(root.path(), Path::new("/ws"));
    assert!(!fs.named("passwd"));
    // No VCS root at all: E21 fences at the target's own directory, and
    // that directory being a link is REFUSED — identically whether the
    // link dangles or not — rather than derived through (the pre-fix
    // derivation rooted the universe at the link's target and passed).
    let mut errors = Vec::new();
    for with_target in [true, false] {
        let mut fs = MockFs::new()
            .file("/srv/ws/demo.package.nml")
            .symlink("/srv/ws/tenants/cu/lib", "../../vendor");
        if with_target {
            fs = fs.file("/srv/ws/vendor/base.flow.nml");
        }
        fs.clear_probes();
        let err = WorkspaceRoot::derive(Path::new("/srv/ws/tenants/cu/lib/base.flow.nml"), &fs)
            .unwrap_err();
        assert!(
            !fs.probes()
                .iter()
                .any(|p| matches!(p, Probe::ResolveSymlink(..)))
        );
        errors.push(err);
    }
    assert_eq!(errors[0], errors[1]);
    assert_eq!(
        errors[0],
        RootError::UnfencedSymlink {
            component: "lib".into()
        }
    );
    assert!(errors[0].to_string().contains("`lib`"));
    // The operator's prefix above the target's own directory is still
    // followed without a fence (`/tmp → /private/tmp`), endpoint
    // kind-checked; and below a fence an operator link is author
    // territory: the root is derived from the directory before it.
    let fs = MockFs::new()
        .symlink("/tmp", "/private/tmp")
        .file("/private/tmp/build/mine.package.nml")
        .file("/private/tmp/build/x.nml");
    let root = WorkspaceRoot::derive(Path::new("/tmp/build/x.nml"), &fs).unwrap();
    assert_eq!(root.path(), Path::new("/private/tmp/build"));
    let fs = MockFs::new()
        .symlink("/tmp", "/etc/passwd")
        .file("/etc/passwd")
        .file("/private/tmp/build/x.nml");
    assert_eq!(
        WorkspaceRoot::derive(Path::new("/tmp/build/x.nml"), &fs).unwrap_err(),
        RootError::NotADirectory
    );
    let fs = MockFs::new()
        .dir("/ws/.git")
        .file("/ws/demo.package.nml")
        .symlink("/ws/apps/link", "/elsewhere")
        .file("/elsewhere/deep/x.nml");
    let root = WorkspaceRoot::derive(Path::new("/ws/apps/link/deep/x.nml"), &fs).unwrap();
    assert_eq!(root.path(), Path::new("/ws"));
    assert!(!fs.named("deep"));
}

#[test]
fn explicit_root_endpoint_is_kind_checked() {
    // E28 (2, D3): `--root <link-to-file>` is not a directory — the
    // resolved endpoint of an operator link is kind-checked, never
    // accepted on the strength of the link existing.
    let fs = MockFs::new()
        .file("/ws/x.nml")
        .symlink("/srv/rootlink", "/ws/x.nml")
        .symlink("/srv/dirlink", "/ws");
    assert_eq!(
        WorkspaceRoot::explicit(Path::new("/srv/rootlink"), &fs).unwrap_err(),
        RootError::NotADirectory
    );
    assert_eq!(
        WorkspaceRoot::explicit(Path::new("/srv/dirlink"), &fs)
            .unwrap()
            .path(),
        Path::new("/ws")
    );
}

#[test]
fn authored_component_count_is_bounded_before_the_walk() {
    // E28 (10): 20,000 × `admin/../` is refused as `Depth` before the
    // first probe (the pre-fix walk made 20,000 probes for it).
    let fs = rfc_tree();
    let root = root_at(&fs, "/ws");
    let mut authored = String::new();
    for _ in 0..20_000 {
        authored.push_str("admin/../");
    }
    authored.push_str("x.nml");
    fs.clear_probes();
    assert_eq!(
        mint(&fs, &root, &authored, Trust::Closed).unwrap_err(),
        PathError::Depth
    );
    assert!(
        fs.probes().len() <= crate::workspace::paths::MAX_COMPONENTS,
        "{} probes",
        fs.probes().len()
    );
    // Exactly 64 authored components still mint (the key bound).
    let ok = (0..63).map(|_| "d").collect::<Vec<_>>().join("/") + "/x.nml";
    assert_eq!(
        mint(&fs, &root, &ok, Trust::Closed).unwrap().key.depth(),
        64
    );
}

#[test]
fn checked_parses_only_key_spellings() {
    // E28 (9): a key that left the kernel comes back through `checked`;
    // `join` takes plain names only.
    assert_eq!(SourceKey::checked("").unwrap(), SourceKey::root());
    assert_eq!(SourceKey::checked("a/b/c.nml").unwrap().depth(), 3);
    for bad in ["../x", "a//b", "/a", "a/", "a/./b", "a\\b", "."] {
        assert_eq!(SourceKey::checked(bad), None, "{bad:?}");
    }
    let deep = (0..65).map(|_| "d").collect::<Vec<_>>().join("/");
    assert_eq!(SourceKey::checked(&deep), None);
    assert_eq!(key("a").join("b.nml").as_str(), "a/b.nml");
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "join takes a plain name")]
fn join_refuses_a_path() {
    let _ = SourceKey::root().join("../x");
}

#[test]
fn split_absolute_yields_dot_only_from_verbatim_or_drive_relative_spellings() {
    use crate::workspace::paths::split_absolute;
    // r52 #6b: the `.` clauses of the fold and of the derived-root walk
    // looked dead — `Path::components` normalizes `.` away — and they ARE
    // dead for every non-verbatim rooted spelling, on every platform.
    // They are live on Windows: a verbatim path (`\\?\C:\ws\.\x`) is not
    // normalized at all, and a drive-relative one (`C:.\x`: a prefix, no
    // root) is split here as if absolute with its head `.` kept. Kept and
    // pinned rather than deleted. The Windows rows run on the
    // windows-latest lane (they follow `Path::components`' documented
    // contract; on Unix those spellings have no prefix and are plain
    // relative names, so the rows are empty there).
    for spelled in [
        "/ws/./x",
        "/./ws/x",
        "/ws/tenants/./cu/./x",
        "/ws/.",
        "/ws/../ws/./x",
    ] {
        let (prefix, names) =
            split_absolute(Path::new(spelled)).unwrap_or_else(|| panic!("{spelled} is rooted"));
        assert_eq!(prefix, Path::new("/"), "{spelled}");
        assert!(names.iter().all(|n| n != "."), "{spelled}: {names:?}");
    }
    for spelled in ["x", "./x", ".", "../x", "ws/./x"] {
        assert!(split_absolute(Path::new(spelled)).is_none(), "{spelled}");
    }
    let windows_rows: &[(&str, &[&str])] = if cfg!(windows) {
        &[
            (r"\\?\C:\ws\.\x", &["ws", ".", "x"]),
            (r"C:.\x", &[".", "x"]),
            (r"C:\ws\.\x", &["ws", "x"]),
        ]
    } else {
        &[]
    };
    for (spelled, expected) in windows_rows {
        let (_, names) =
            split_absolute(Path::new(spelled)).unwrap_or_else(|| panic!("{spelled} splits"));
        let names: Vec<&str> = names.iter().map(|n| n.to_str().unwrap()).collect();
        assert_eq!(&names[..], *expected, "{spelled}");
    }
}

#[test]
fn classify_is_the_kernels_walk_over_every_component() {
    // E35 (E33's DRY end-state): argv classification IS `mint`'s walk —
    // the same entry through the operator's prefix, the same halts —
    // over EVERY component, the last included. `nml fix` expands a
    // `Root` or `Dir` and hands everything else to `mint` + `verify`.
    let fs = rfc_tree();
    let root = root_at(&fs, "/ws");
    let classify = |p: &str, trust: Trust| SourceKey::classify(&root, Path::new(p), &fs, trust);
    // The root itself, by every spelling that lands on it — a `..` AT
    // the root folds through its canonical parent (E29).
    for spelling in [
        ".",
        "tenants/..",
        "tenants/cu/../..",
        "/ws",
        "/ws/.",
        "/ws/tenants/..",
        "/ws/../ws",
    ] {
        assert_eq!(
            classify(spelling, Trust::Closed).unwrap(),
            Endpoint::Root,
            "{spelling}"
        );
    }
    // A real directory, at its key; a `.`/`..` LAST component is applied
    // like any other (an argv path is the operator's own).
    for (spelling, at) in [
        ("tenants/cu", "tenants/cu"),
        ("tenants/cu/.", "tenants/cu"),
        ("tenants/cu/member-lookup.flow.nml/..", "tenants/cu"),
        ("tenants/nobody/../cu", "tenants/cu"),
        ("/ws/admin/secretdir", "admin/secretdir"),
        ("/ws/../ws/tenants/cu", "tenants/cu"),
    ] {
        assert_eq!(
            classify(spelling, Trust::Closed).unwrap(),
            Endpoint::Dir(key(at)),
            "{spelling}"
        );
    }
    // A file, an absent path, a path under a file: file candidates.
    for spelling in [
        "tenants/cu/member-lookup.flow.nml",
        "tenants/nobody",
        "tenants/cu/absent/deeper",
        "tenants/cu/member-lookup.flow.nml/under",
    ] {
        assert_eq!(
            classify(spelling, Trust::Closed).unwrap(),
            Endpoint::Other,
            "{spelling}"
        );
    }
    // Closed trust: a link — to a directory, to a file, dangling — halts
    // exactly as `mint` halts, wherever the spelling continues past it,
    // the target never resolved.
    for (spelling, at) in [
        ("tenants/cu/lib", "lib"),
        ("tenants/cu/lib/.", "lib"),
        ("tenants/cu/lib/..", "lib"),
        ("tenants/cu/lib/sub", "lib"),
        ("tenants/nobody/../cu/lib/sub", "lib"),
        ("tenants/cu/leaf.nml", "leaf.nml"),
        ("tenants/cu/dangling", "dangling"),
        ("tenants/cu/dangling/x", "dangling"),
    ] {
        fs.clear_probes();
        let err = classify(spelling, Trust::Closed).unwrap_err();
        assert!(
            matches!(&err, PathError::SymlinkComponent { component, .. } if component == at),
            "{spelling}: {err:?}"
        );
        assert!(
            !fs.probes()
                .iter()
                .any(|p| matches!(p, Probe::ResolveSymlink(..))),
            "{spelling}: closed classification resolved a link: {:?}",
            fs.probes()
        );
    }
    // `..` past the root is `Escapes` — lexically, AFTER a walked
    // component too (the r60 row E34 noted: the deleted classifier
    // re-located physically and listed; the kernel pops once inside).
    for spelling in [
        "..",
        "tenants/../..",
        "tenants/cu/../../../ws/tenants/cu",
        "/ws/tenants/../../ws/tenants/cu",
    ] {
        assert!(
            matches!(
                classify(spelling, Trust::Closed),
                Err(PathError::Escapes { .. })
            ),
            "{spelling}"
        );
    }
    // The authored bound holds before the first probe.
    let deep = (0..65).map(|_| "tenants").collect::<Vec<_>>().join("/");
    assert_eq!(classify(&deep, Trust::Closed), Err(PathError::Depth));
    // Open trust (E31 option (b), the one-token switch's blast radius):
    // a link to a directory is FOLLOWED and its endpoint kind-checked; a
    // link to a file, or a dangling one, is a file candidate.
    assert_eq!(
        classify("tenants/cu/lib", Trust::Open).unwrap(),
        Endpoint::Dir(key("admin/secretdir"))
    );
    assert_eq!(
        classify("tenants/cu/leaf.nml", Trust::Open).unwrap(),
        Endpoint::Other
    );
    assert_eq!(
        classify("tenants/cu/dangling", Trust::Open).unwrap(),
        Endpoint::Other
    );
}

#[test]
fn root_origin_tag_states_the_fact_without_cli_advice() {
    // E35: Layer A speaks no front end's advice — "pass --root to pin"
    // is the CLI reporter's sentence (`Workspace::origin_tag`).
    for (origin, tag) in [
        (RootOrigin::Explicit, "explicit"),
        (RootOrigin::Editor, "editor"),
        (
            RootOrigin::Derived {
                fence: Fence::Vcs {
                    kind: EntryKind::Dir,
                },
                shadowed: None,
            },
            "derivedVcsFence",
        ),
        (
            RootOrigin::Derived {
                fence: Fence::Vcs {
                    kind: EntryKind::File,
                },
                shadowed: Some(Shadow::Git(PathBuf::from("/srv/.git"))),
            },
            "derivedVcsFence",
        ),
        (
            RootOrigin::Derived {
                fence: Fence::TargetDir,
                shadowed: None,
            },
            "derivedTargetDir",
        ),
    ] {
        assert_eq!(origin.tag(), tag);
        assert!(!tag.contains("pass"), "{tag}");
        // One enum value of the `--json` vocabulary: lowerCamel, no
        // space, no punctuation for a consumer to strip.
        assert!(
            tag.bytes().all(|b| b.is_ascii_alphanumeric())
                && tag.starts_with(|c: char| c.is_ascii_lowercase()),
            "{tag}"
        );
    }
}

/// r69b (cov r68 M25): the halt key's own component bound. Unreachable
/// through `mint`/`classify` since the authored bound (E28) refuses a
/// 65-component spelling before the walk, so it is pinned DIRECTLY —
/// defence in depth stays defended: a resolved prefix plus the halting
/// component plus a lexical tail past 64 is `Depth`, never a key.
#[test]
fn halt_key_is_bounded_at_sixty_four_components() {
    use std::ffi::OsString;
    let comps: Vec<String> = (0..60).map(|i| format!("d{i}")).collect();
    let tail: Vec<OsString> = (0..10).map(|i| OsString::from(format!("t{i}"))).collect();
    assert_eq!(
        crate::workspace::paths::halt_key(comps.clone(), "link", &tail),
        Err(PathError::Depth)
    );
    // Exactly 64 is a key; `.` is dropped and `..` pops, so a tail that
    // nets out under the bound is fine.
    let three: Vec<OsString> = ["t0", "t1", "t2"].map(OsString::from).to_vec();
    let key = crate::workspace::paths::halt_key(comps.clone(), "link", &three).unwrap();
    assert_eq!(key.depth(), 64);
    let popping: Vec<OsString> = (0..10)
        .flat_map(|i| [OsString::from(format!("t{i}")), OsString::from("..")])
        .chain([OsString::from("."), OsString::from("x.nml")])
        .collect();
    let key = crate::workspace::paths::halt_key(comps, "link", &popping).unwrap();
    assert_eq!(key.depth(), 62);
    assert!(key.as_str().ends_with("/link/x.nml"));
}

/// r80-sec F6: the shadow check at derivation. A `.git` entry BELOW the
/// operator's manifest — git's own submodule `.git` FILE, or a planted
/// entry of any kind — used to re-fence the operator's `--root`-less
/// check at the tenant's directory, silently, so a file crafted valid
/// under the tenant's manifest and invalid under the operator's read
/// `ok`. Now the bounded walk continues above the fence: a root marker
/// there REFUSES derivation, naming both; another `.git` above it, with
/// no marker between, is the outer repository's and the derived root is
/// reported as shadowed by its directory; nothing above leaves the
/// origin unshadowed.
#[test]
fn derive_refuses_a_fence_below_a_root_marker_and_reports_a_shadowing_git() {
    // The fence kinds git can be made to write below an operator's
    // manifest (a submodule's or linked worktree's `.git` FILE) or a
    // tenant can plant (a link, a special entry) REFUSE under a marker;
    // a `.git` DIRECTORY — a nested checkout, which no commit produces —
    // derives at its own universe and DISCLOSES the marker.
    for kind in ["dir", "file", "symlink", "other"] {
        let mut fs = MockFs::new()
            .dir("/srv/app/.git")
            .file("/srv/app/demo.package.nml")
            .file("/srv/app/tenants/cu/sub/evil.package.nml")
            .file("/srv/app/tenants/cu/sub/x.nml");
        fs = match kind {
            "dir" => fs.dir("/srv/app/tenants/cu/sub/.git"),
            "file" => fs.file("/srv/app/tenants/cu/sub/.git"),
            "symlink" => fs.symlink("/srv/app/tenants/cu/sub/.git", "/nonexistent"),
            _ => fs.other("/srv/app/tenants/cu/sub/.git"),
        };
        let derived = WorkspaceRoot::derive(Path::new("/srv/app/tenants/cu/sub/x.nml"), &fs);
        if kind == "dir" {
            let root = derived.unwrap();
            assert_eq!(root.path(), Path::new("/srv/app/tenants/cu/sub"));
            assert_eq!(
                root.origin,
                RootOrigin::Derived {
                    fence: Fence::Vcs {
                        kind: EntryKind::Dir
                    },
                    shadowed: Some(Shadow::Marker(PathBuf::from("/srv/app/demo.package.nml"))),
                },
                "{kind}"
            );
            continue;
        }
        let err = derived.unwrap_err();
        assert_eq!(
            err,
            RootError::Shadowed {
                marker: PathBuf::from("/srv/app/demo.package.nml"),
                fence: PathBuf::from("/srv/app/tenants/cu/sub/.git"),
            },
            "{kind}"
        );
        assert_eq!(
            err.to_string(),
            "the root marker `/srv/app/demo.package.nml` sits above the fence at \
             `/srv/app/tenants/cu/sub/.git`, and that fence is no directory — a submodule's or \
             linked worktree's .git file, or a planted entry, below a workspace manifest cannot \
             shrink its universe"
        );
    }
    // A project config above the fence is a root marker too.
    let fs = MockFs::new()
        .dir("/srv/app/.git")
        .file("/srv/app/nml-project.nml")
        .file("/srv/app/tenants/cu/sub/.git")
        .file("/srv/app/tenants/cu/sub/x.nml");
    assert!(matches!(
        WorkspaceRoot::derive(Path::new("/srv/app/tenants/cu/sub/x.nml"), &fs),
        Err(RootError::Shadowed { .. })
    ));
    // No marker above, an outer `.git`: derived at the submodule's own
    // manifest, SHADOWED by the outer repository's directory, the fence
    // entry's kind kept.
    let fs = MockFs::new()
        .dir("/srv/app/.git")
        .file("/srv/app/tenants/cu/sub/.git")
        .file("/srv/app/tenants/cu/sub/mine.package.nml")
        .file("/srv/app/tenants/cu/sub/x.nml");
    let root = WorkspaceRoot::derive(Path::new("/srv/app/tenants/cu/sub/x.nml"), &fs).unwrap();
    assert_eq!(root.path(), Path::new("/srv/app/tenants/cu/sub"));
    assert_eq!(
        root.origin,
        RootOrigin::Derived {
            fence: Fence::Vcs {
                kind: EntryKind::File
            },
            shadowed: Some(Shadow::Git(PathBuf::from("/srv/app/.git"))),
        }
    );
    assert_eq!(root.origin.tag(), "derivedVcsFence");
    assert_eq!(
        root.origin,
        RootOrigin::Derived {
            fence: Fence::Vcs {
                kind: EntryKind::File
            },
            shadowed: Some(Shadow::Git(PathBuf::from("/srv/app/.git"))),
        }
    );
    // A marker ABOVE the outer `.git` is the outer repository's business
    // — E21's `/tmp` manifest above a checkout — and does not refuse:
    // the shadow walk stops at the first `.git` above the fence.
    let fs = MockFs::new()
        .file("/srv/evil.package.nml")
        .dir("/srv/app/.git")
        .file("/srv/app/tenants/cu/sub/.git")
        .file("/srv/app/tenants/cu/sub/x.nml");
    let root = WorkspaceRoot::derive(Path::new("/srv/app/tenants/cu/sub/x.nml"), &fs).unwrap();
    assert_eq!(
        root.origin,
        RootOrigin::Derived {
            fence: Fence::Vcs {
                kind: EntryKind::File
            },
            shadowed: Some(Shadow::Git(PathBuf::from("/srv/app/.git"))),
        }
    );
    // Nothing above the fence: unshadowed.
    let fs = MockFs::new()
        .dir("/srv/app/.git")
        .file("/srv/app/demo.package.nml")
        .file("/srv/app/tenants/cu/x.nml");
    let root = WorkspaceRoot::derive(Path::new("/srv/app/tenants/cu/x.nml"), &fs).unwrap();
    assert_eq!(
        root.origin,
        RootOrigin::Derived {
            fence: Fence::Vcs {
                kind: EntryKind::Dir
            },
            shadowed: None,
        }
    );
    // The shadow walk shares the 64-directory bound: a marker 63
    // directories above a fence met at once is seen (disclosed above a
    // directory fence, refused above a file fence); one directory
    // further the check is CUT SHORT — and refuses rather than deriving
    // an unchecked universe (the bound is a work bound, never a fence:
    // a tenant's content 61 directories below a submodule-shaped fence
    // used to derive at the tenant's manifest with `shadowed: None`).
    let deep: String = (0..70).map(|i| format!("/d{i}")).collect();
    let within = MockFs::new()
        .file("/d0/d1/d2/d3/d4/d5/d6/demo.package.nml")
        .dir(&format!("{deep}/.git"))
        .file(&format!("{deep}/x.nml"));
    let root = WorkspaceRoot::derive(Path::new(&format!("{deep}/x.nml")), &within).unwrap();
    assert_eq!(
        root.origin,
        RootOrigin::Derived {
            fence: Fence::Vcs {
                kind: EntryKind::Dir
            },
            shadowed: Some(Shadow::Marker(PathBuf::from(
                "/d0/d1/d2/d3/d4/d5/d6/demo.package.nml"
            ))),
        }
    );
    let within_file = MockFs::new()
        .file("/d0/d1/d2/d3/d4/d5/d6/demo.package.nml")
        .file(&format!("{deep}/.git"))
        .file(&format!("{deep}/x.nml"));
    assert!(matches!(
        WorkspaceRoot::derive(Path::new(&format!("{deep}/x.nml")), &within_file),
        Err(RootError::Shadowed { .. })
    ));
    for fence in ["dir", "file"] {
        let mut beyond = MockFs::new()
            .file("/d0/d1/d2/d3/d4/d5/demo.package.nml")
            .file(&format!("{deep}/x.nml"));
        beyond = match fence {
            "dir" => beyond.dir(&format!("{deep}/.git")),
            _ => beyond.file(&format!("{deep}/.git")),
        };
        let err = WorkspaceRoot::derive(Path::new(&format!("{deep}/x.nml")), &beyond).unwrap_err();
        assert_eq!(
            err,
            RootError::ShadowUnchecked {
                fence: PathBuf::from(format!("{deep}/.git")),
                last: PathBuf::from("/d0/d1/d2/d3/d4/d5/d6"),
            },
            "{fence}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "the shadow check above the fence at `{deep}/.git` reached the walk bound of 64 \
                 directories at `/d0/d1/d2/d3/d4/d5/d6` — a universe is denied rather than \
                 derived unchecked"
            )
        );
    }
}

/// The fence entry's kind on the wire: `dir`, `file`, `symlink`,
/// `other`; none without a VCS fence.
#[test]
fn fence_entry_tag_spells_the_kind() {
    for (kind, tag) in [
        (EntryKind::Dir, "dir"),
        (EntryKind::File, "file"),
        (EntryKind::Symlink, "symlink"),
        (EntryKind::Other, "other"),
    ] {
        assert_eq!(Fence::Vcs { kind }.entry_tag(), Some(tag));
    }
    assert_eq!(Fence::TargetDir.entry_tag(), None);
}

/// The fence facts for the two fence kinds no front-end pin exercised:
/// a `.git` SYMLINK and a `.git` special entry are planted entries,
/// spelled as such, and always disclosed; a directory fence with nothing
/// above it is the plain checkout (spelled, not disclosed); a shadow
/// rides the sentence through the front end's `spell`; an explicit,
/// editor or target-directory root has no facts at all.
#[test]
fn fence_facts_spell_a_symlink_and_a_special_entry_fence() {
    let spell = |p: &Path| p.display().to_string();
    for (kind, entry) in [
        (EntryKind::Symlink, "a .git SYMLINK fence — a planted entry"),
        (
            EntryKind::Other,
            "a .git special-entry fence — a planted entry",
        ),
    ] {
        let origin = RootOrigin::Derived {
            fence: Fence::Vcs { kind },
            shadowed: None,
        };
        assert_eq!(
            origin.fence_facts(&spell).as_deref(),
            Some(format!("within {entry}").as_str())
        );
        assert!(origin.needs_disclosure(), "{kind:?}");
        assert_eq!(origin.tag(), "derivedVcsFence");
    }
    let plain = RootOrigin::Derived {
        fence: Fence::Vcs {
            kind: EntryKind::Dir,
        },
        shadowed: None,
    };
    assert_eq!(
        plain.fence_facts(&spell).as_deref(),
        Some("within the .git fence")
    );
    assert!(!plain.needs_disclosure());
    let shadowed = RootOrigin::Derived {
        fence: Fence::Vcs {
            kind: EntryKind::Symlink,
        },
        shadowed: Some(Shadow::Git(PathBuf::from("/r/.git"))),
    };
    assert_eq!(
        shadowed.fence_facts(&spell).as_deref(),
        Some(
            "within a .git SYMLINK fence — a planted entry; SHADOWED by another .git entry at \
             `/r/.git` above it"
        )
    );
    for origin in [RootOrigin::Explicit, RootOrigin::Editor] {
        assert_eq!(origin.fence_facts(&spell), None, "{origin:?}");
        assert!(!origin.needs_disclosure(), "{origin:?}");
    }
    // No fence at all: no fence FACTS to spell (the tag says it), but a
    // root an operator could not infer — disclosed.
    let no_fence = RootOrigin::Derived {
        fence: Fence::TargetDir,
        shadowed: None,
    };
    assert_eq!(no_fence.fence_facts(&spell), None);
    assert!(no_fence.needs_disclosure());
    assert_eq!(no_fence.tag(), "derivedTargetDir");
}

/// A component bearing a separator (`ev\il` on unix — a legal name git
/// tracks, one the walk reports as `unkeyableName`) is refused where the
/// key is MINTED, under either trust, as the parent, the leaf or a lexical
/// tail component — `PathError::NotPlain` naming it, the read's own
/// sentence one step earlier. A key with a `\` in it used to be minted,
/// judged, and only then refused at the open. `SourceKey::under` (the one
/// lexical keying, the editor's included) answers `None` for it and still
/// pops `..` for a plain spelling.
#[cfg(unix)]
#[test]
fn a_separator_bearing_component_is_refused_at_mint() {
    let fs = MockFs::new()
        .dir("/ws")
        .dir("/ws/tenants")
        .dir("/ws/tenants/ev\\il")
        .file("/ws/tenants/ev\\il/x.nml")
        .file("/ws/tenants/a\\b.nml")
        .dir("/ws/tenants/cu");
    let root = WorkspaceRoot::explicit(Path::new("/ws"), &fs).unwrap();
    let not_plain = |component: &str| PathError::NotPlain {
        component: component.to_string(),
    };
    for trust in [Trust::Closed, Trust::Open] {
        // The parent directory's name.
        assert_eq!(
            SourceKey::mint(&root, Path::new("tenants/ev\\il/x.nml"), &fs, trust).unwrap_err(),
            not_plain("ev\\il"),
            "{trust:?}"
        );
        // The leaf (never probed — refused lexically).
        assert_eq!(
            SourceKey::mint(&root, Path::new("tenants/a\\b.nml"), &fs, trust).unwrap_err(),
            not_plain("a\\b.nml"),
            "{trust:?}"
        );
        // A lexical tail past the resolved prefix.
        assert_eq!(
            SourceKey::mint(&root, Path::new("tenants/absent/ev\\il/x.nml"), &fs, trust)
                .unwrap_err(),
            not_plain("ev\\il"),
            "{trust:?}"
        );
        assert_eq!(
            SourceKey::classify(&root, Path::new("tenants/ev\\il"), &fs, trust).unwrap_err(),
            not_plain("ev\\il"),
            "{trust:?}"
        );
    }
    assert_eq!(
        SourceKey::under(&root, Path::new("/ws/tenants/ev\\il/x.nml"), &fs),
        None
    );
    assert_eq!(
        SourceKey::under(&root, Path::new("/ws/tenants/nobody/../cu/x.nml"), &fs),
        Some(SourceKey::checked("tenants/cu/x.nml").unwrap())
    );
    assert_eq!(
        SourceKey::under(&root, Path::new("/ws/../x.nml"), &fs),
        None,
        "escapes the root"
    );
    assert!(
        not_plain("ev\\il")
            .to_string()
            .contains("`ev\\il` is not a plain path component"),
        "the read's sentence"
    );
}

/// RFC 0026 B-8: `SourceKey::under` is the ONE lexical keying, and it
/// answers as `mint` would settle — the truth table: `.` dropped, `..`
/// popped (past the root: escapes, `None`; to the root exactly: the
/// root's own key), a relative path root-relative, a component that
/// bears a separator or is not UTF-8 `None`, the component bound
/// inclusive (64 components key, 65 do not — counted before any pop,
/// as `mint` counts them), a path outside the root `None`, and nothing
/// below the root probed (a directory that does not exist keys).
#[test]
fn under_pops_drops_and_refuses_like_mint() {
    let fs = MockFs::new()
        .dir("/ws")
        .dir("/ws/tenants")
        .dir("/ws/tenants/cu");
    let root = WorkspaceRoot::explicit(Path::new("/ws"), &fs).unwrap();
    let under =
        |p: &str| SourceKey::under(&root, Path::new(p), &fs).map(|k| k.as_str().to_string());
    assert_eq!(
        under("/ws/tenants/./cu/x.nml").as_deref(),
        Some("tenants/cu/x.nml"),
        "`.` dropped"
    );
    assert_eq!(
        under("./tenants/cu/x.nml").as_deref(),
        Some("tenants/cu/x.nml"),
        "a leading `.` dropped (an interior one `Path` already folds)"
    );
    assert_eq!(
        under("tenants/cu/../du/x.nml").as_deref(),
        Some("tenants/du/x.nml"),
        "relative is root-relative; `..` pops"
    );
    assert_eq!(
        under("/ws/tenants/cu/../../x.nml").as_deref(),
        Some("x.nml"),
        "pops to the root exactly"
    );
    assert_eq!(
        under("/ws/tenants/../../x.nml"),
        None,
        "a pop past the root escapes"
    );
    assert_eq!(under("/elsewhere/x.nml"), None, "outside the root");
    assert_eq!(
        under("/ws/tenants/nobody/x.nml").as_deref(),
        Some("tenants/nobody/x.nml"),
        "nothing below the root is probed"
    );
    let deep = |dirs: usize| {
        format!(
            "/ws/{}/x.nml",
            (0..dirs).map(|_| "d").collect::<Vec<_>>().join("/")
        )
    };
    let bound = crate::workspace::paths::MAX_COMPONENTS;
    assert!(under(&deep(bound - 1)).is_some(), "64 components key");
    assert_eq!(under(&deep(bound)), None, "65 do not");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bad = Path::new("/ws/tenants").join(std::ffi::OsStr::from_bytes(b"\xff.nml"));
        assert_eq!(SourceKey::under(&root, &bad, &fs), None, "not UTF-8");
    }
}

/// The manifest-name rule, once, and the one name it must REFUSE: a bare
/// `package.nml` is content, never a root marker and never a manifest.
/// The rule used to spell that as `&& name != "package.nml"` — a conjunct
/// that can never fire (`"package.nml"` is 11 bytes, the suffix
/// `".package.nml"` is 12, so a name equal to the former cannot end with
/// the latter), and a redundant guard reads as a claim that the first
/// test is wrong. The intent is pinned HERE instead.
#[test]
fn a_bare_package_nml_is_not_a_manifest_name() {
    use crate::file_names::is_manifest_name;
    for yes in ["demo.package.nml", ".package.nml", "a.b.package.nml"] {
        assert!(is_manifest_name(yes), "{yes}");
    }
    for no in [
        "package.nml",
        "packagenml",
        "xpackage.nml",
        "demo.package.nm",
        "",
    ] {
        assert!(!is_manifest_name(no), "{no}");
    }
}

/// r103-cov: the manifest-NAME vocabulary, pinned. `is_manifest_name`
/// decides which listed file the walk settles as a live resolution
/// input, and nothing pinned it: deleting its second clause
/// (`name != "package.nml"`) left every test in the workspace green,
/// because that clause is UNREACHABLE — `"package.nml"` is eleven bytes
/// and `.ends_with(".package.nml")` needs twelve, so the `ends_with`
/// has already answered `false`. The spelling the clause was reaching
/// for is the DEGENERATE one, `".package.nml"` (an empty stem), and
/// that one IS admitted: it becomes a live manifest whose stem can
/// never equal its declared `package` name (a package name is
/// `[a-z][a-z0-9-]*`, which cannot start with `.`), so any
/// dot-prefixed manifest closes the universe with NML2088 wherever a
/// live claim does not already cover it — while the walk's own dot-file
/// policy says a hidden file is "never checked, fixed or indexed
/// unasked". Two rules disagreeing is an owner call (see the r103-cov
/// report); this table makes either answer a VISIBLE change.
#[test]
fn the_manifest_name_vocabulary_is_closed_and_pins_the_degenerate_spellings() {
    use crate::file_names::is_manifest_name;
    for (name, want) in [
        ("demo.package.nml", true),
        ("a.package.nml", true),
        // No stem at all: a bare `package.nml` is NOT a manifest…
        ("package.nml", false),
        // …but the hidden spelling is, and it can never load.
        (".package.nml", true),
        (".demo.package.nml", true),
        ("demo.package.nml.bak", false),
        ("demo.package", false),
        ("nml-project.nml", false),
        ("core.model.nml", false),
        ("", false),
    ] {
        assert_eq!(is_manifest_name(name), want, "{name:?}");
    }
}

/// r103-cov: the four KEY-CONTAINMENT relations agree, and none of them
/// confuses a name for a path boundary.
///
/// `is_strict_ancestor_of` compares `other` against `self` and then
/// demands the next byte be `/`; `dir_is_strict_ancestor_of` is the same
/// rule against `dir(self)` without minting the directory key — and its
/// `/` test had no pin at all: deleting it left every test green, while
/// `tenants` became a strict ancestor of `tenants-evil/x.flow.nml`.
/// Through `dir_contains` that is R5′ itself: `ManifestClaim::anchor_for`
/// and `is_budget_unit` ask exactly this question, so a manifest at
/// `tenants/m.package.nml` would have claimed — and minted budget units
/// inside — a SIBLING subtree it does not hold.
#[test]
fn the_key_containment_relations_never_confuse_a_name_prefix_for_a_boundary() {
    let k = |s: &str| SourceKey::checked(s).expect(s);
    // `-` (0x2D) sorts below `/` (0x2F), so a sibling whose name extends
    // another's is the shape a bare `starts_with` gets wrong.
    let cases: [(&str, &str, bool); 8] = [
        ("tenants", "tenants/cu/x.flow.nml", true),
        ("tenants", "tenants-evil/x.flow.nml", false),
        ("tenants", "tenantsevil", false),
        ("tenants", "tenants", false),
        ("tenants/cu", "tenants/cu-2/x.nml", false),
        ("tenants/cu", "tenants/cu/x.nml", true),
        ("", "anything/x.nml", true),
        ("", "", false),
    ];
    for (dir, other, want) in cases {
        assert_eq!(
            k(dir).is_strict_ancestor_of(&k(other)),
            want,
            "is_strict_ancestor_of({dir:?}, {other:?})"
        );
        assert_eq!(
            k(dir).contains(&k(other)),
            want || dir == other,
            "contains({dir:?}, {other:?})"
        );
        // The borrowed twins answer for `dir(self)`: ask them with a key
        // one level deeper whose directory is `dir`.
        let file = if dir.is_empty() {
            k("m.package.nml")
        } else {
            k(dir).join("m.package.nml")
        };
        assert_eq!(
            file.dir_is_strict_ancestor_of(&k(other)),
            want,
            "dir_is_strict_ancestor_of({file}, {other:?})"
        );
        assert_eq!(
            file.dir_contains(&k(other)),
            want || dir == other,
            "dir_contains({file}, {other:?})"
        );
    }
    // `relative_to` is the same boundary, and answers `None` where the
    // relations answer `false`.
    assert_eq!(
        k("tenants-evil/x.flow.nml").relative_to(&k("tenants")),
        None
    );
    assert_eq!(
        k("tenants/cu/x.flow.nml").relative_to(&k("tenants")),
        Some("cu/x.flow.nml")
    );
}

/// r103-cov: `mint`'s OWN depth refusal — the one that runs after the
/// walk, on the parents it came back with — fires.
///
/// The authored bound refuses 65 components before the first probe and
/// the walk refuses a respelling past 64, so the only way to reach
/// `mint` with a full 64 parent components is an OPEN-trust symlink
/// whose target is exactly at the bound: the leaf is then the 65th, and
/// `mint` refuses it. Nothing pinned that boundary — relaxing the test
/// by one (`>=` to `>`) left every test green while `mint` returned a
/// 65-component key, a key `SourceKey::checked` (and `under`, and
/// `child_dir`) refuses: a key that leaves the kernel could not come
/// back through its own front door.
#[test]
fn mint_refuses_a_leaf_that_would_be_the_65th_component_after_a_respell() {
    let bound = crate::workspace::paths::MAX_COMPONENTS;
    let deep: Vec<String> = (0..bound).map(|i| format!("d{i}")).collect();
    let mut fs = MockFs::new();
    let mut at = String::from("/ws");
    for name in &deep {
        at.push('/');
        at.push_str(name);
        fs = fs.dir(&at);
    }
    let fs = fs.symlink("/ws/link", &at).file(&format!("{at}/x.nml"));
    let root = root_at(&fs, "/ws");
    // The target itself keys: exactly 64 directory components.
    assert_eq!(
        SourceKey::checked(&deep.join("/")).map(|k| k.depth()),
        Some(bound)
    );
    // Through the link, under OPEN trust, the respelled prefix is those
    // 64 — and the leaf would make 65.
    assert_eq!(
        mint(&fs, &root, "link/x.nml", Trust::Open).unwrap_err(),
        PathError::Depth
    );
    // One shallower target and the same spelling mints, at the bound.
    let shallower = deep[..bound - 1].join("/");
    let fs = {
        let mut fs = MockFs::new();
        let mut at = String::from("/ws");
        for name in &deep[..bound - 1] {
            at.push('/');
            at.push_str(name);
            fs = fs.dir(&at);
        }
        fs.symlink("/ws/link", &at).file(&format!("{at}/x.nml"))
    };
    let root = root_at(&fs, "/ws");
    let keyed = mint(&fs, &root, "link/x.nml", Trust::Open).expect("at the bound");
    assert_eq!(keyed.key.depth(), bound);
    assert_eq!(keyed.key.as_str(), format!("{shallower}/x.nml"));
    assert!(
        SourceKey::checked(keyed.key.as_str()).is_some(),
        "round-trips"
    );
}

/// r105-cov: the fence rule for a BLINDED listing above the fence,
/// scripted (`MockFs::unlistable`: a lookup answers, a listing refuses —
/// mode `0111`), so the pin no longer rests on real modes and is not
/// skipped when the suite runs as root. Above a DIRECTORY fence a
/// blinded listing costs the disclosure alone: the universe derives at
/// the same root, the marker above is simply not reported, and the check
/// walks ON (a `.git` higher up is still disclosed — a lookup, which
/// `0111` answers). Above a fence that is NO directory an unseen marker
/// would have been the `Shadowed` refusal, so the blinded listing refuses
/// too. And the ROOT'S OWN listing blinded is never "no marker": the
/// derivation refuses rather than shrink the universe to an open context
/// (the r103 fail-open). The three mutants — the carve-out made
/// unconditional, removed, and inverted — are each RED here.
#[test]
fn a_blinded_listing_above_the_fence_costs_what_that_fence_can_deny() {
    let tree = |git_dir: bool| {
        let fs = MockFs::new()
            .file("/srv/evil.package.nml")
            .file("/srv/app/demo.package.nml")
            .file("/srv/app/tenants/cu/x.nml");
        if git_dir {
            fs.dir("/srv/app/.git")
        } else {
            fs.file("/srv/app/.git")
        }
    };
    let target = Path::new("/srv/app/tenants/cu/x.nml");
    // Listable: the marker above a DIRECTORY fence is disclosed.
    let root = WorkspaceRoot::derive(target, &tree(true)).unwrap();
    assert_eq!(root.path(), Path::new("/srv/app"));
    assert_eq!(
        root.origin(),
        &RootOrigin::Derived {
            fence: Fence::Vcs {
                kind: EntryKind::Dir,
            },
            shadowed: Some(Shadow::Marker(PathBuf::from("/srv/evil.package.nml"))),
        }
    );
    // Blinded: the same root, the disclosure lost, nothing else.
    let root = WorkspaceRoot::derive(target, &tree(true).unlistable("/srv")).unwrap();
    assert_eq!(root.path(), Path::new("/srv/app"));
    assert_eq!(
        root.origin(),
        &RootOrigin::Derived {
            fence: Fence::Vcs {
                kind: EntryKind::Dir,
            },
            shadowed: None,
        },
        "a blinded listing above a directory fence loses the disclosure, never the universe"
    );
    // The check walks ON above the blinded ancestor: a `.git` higher up
    // is still disclosed, by lookup.
    let root = WorkspaceRoot::derive(target, &tree(true).unlistable("/srv").dir("/.git")).unwrap();
    assert_eq!(
        root.origin(),
        &RootOrigin::Derived {
            fence: Fence::Vcs {
                kind: EntryKind::Dir,
            },
            shadowed: Some(Shadow::Git(PathBuf::from("/.git"))),
        }
    );
    // Above a FILE fence the marker refuses by name...
    assert_eq!(
        WorkspaceRoot::derive(target, &tree(false)).unwrap_err(),
        RootError::Shadowed {
            marker: PathBuf::from("/srv/evil.package.nml"),
            fence: PathBuf::from("/srv/app/.git"),
        }
    );
    // ...and blinded, the same refusal is unreachable BY NAME — and the
    // universe is denied all the same, never derived on the unknown.
    assert_eq!(
        WorkspaceRoot::derive(target, &tree(false).unlistable("/srv")).unwrap_err(),
        RootError::Fs(FsError::Denied),
        "above a fence that is no directory a blinded listing refuses"
    );
    // The root's OWN listing blinded: refused, never derived at the
    // target's directory with the operator's manifest silently gone.
    assert_eq!(
        WorkspaceRoot::derive(target, &tree(true).unlistable("/srv/app")).unwrap_err(),
        RootError::Fs(FsError::Denied),
        "an unlistable directory on the chain is an unknown marker, not no marker"
    );
}
