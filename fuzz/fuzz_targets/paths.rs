//! Fuzz the path kernel (RFC 0019 item 0, step 0b): P1–P4 over a
//! scripted filesystem with a probe log, under both trusts.
//!
//! The input is a tree descriptor plus an authored path, both drawn from
//! a small alphabet so that every byte lands on an interesting shape:
//! nested directories, case variants, symlink edges (relative targets
//! with `..`, loops, dangling), EACCES nodes, lookup insensitivity, the
//! wasi spelling regime.
//!
//! Invariants: never panic; a minted key is never absolute, never carries
//! `\`, an empty, `.` or `..` component, and never exceeds 64 components;
//! re-minting a fully resolved open-trust key's own path is a fixed point;
//! the LEAF name is never held at an oracle call during minting (the
//! no-existence-oracle rule — leaf names come from a disjoint alphabet so
//! the check is exact); closed trust never resolves a symlink, and no
//! EXISTING component of a key it mints is a symlink (checked against the
//! mock's node table — the `..`-after-an-absent-component hole, E28);
//! EACCES and ELOOP surface typed, never as "absent".

#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;
use nml_validate::workspace::{MockFs, Probe, Spelling};
use nml_validate::workspace::{PathError, SourceKey, SymlinkVerdict, Trust, WorkspaceRoot};
use nml_validate::fs::{EntryKind, FsError};

const DIRS: [&str; 6] = ["a", "A", "b", "lnk", "caf\u{e9}", "cafe\u{301}"];
const LEAVES: [&str; 3] = ["x.nml", "y.nml", "z.nml"];
const TARGETS: [&str; 6] = ["a", "../a", "../../b", "lnk", "/ws/A", "nowhere"];

fn dir_path(bytes: &[u8]) -> String {
    let mut p = String::from("/ws");
    for b in bytes.iter().take(6) {
        p.push('/');
        p.push_str(DIRS[(*b as usize) % DIRS.len()]);
    }
    p
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let flags = data[0];
    let mut fs = MockFs::new().file("/ws/x.package.nml");
    if flags & 1 != 0 {
        fs = fs.insensitive();
    }
    if flags & 2 != 0 {
        fs = fs.spelling(Spelling::Membership);
    }
    let trust = if flags & 4 != 0 { Trust::Closed } else { Trust::Open };

    // Tree ops: [kind, n, n bytes of path].
    let mut i = 1usize;
    while i + 1 < data.len() && data[i] != 0xff {
        let kind = data[i];
        let n = (data[i + 1] as usize) % 7;
        let path_bytes = &data[i + 2..(i + 2 + n).min(data.len())];
        let path = dir_path(path_bytes);
        fs = match kind % 5 {
            0 => fs.dir(&path),
            1 => fs.file(&format!("{path}/{}", LEAVES[(kind as usize / 5) % LEAVES.len()])),
            2 => fs.symlink(&path, TARGETS[(kind as usize / 5) % TARGETS.len()]),
            3 => fs.denied(&path),
            _ => fs.symlink(&format!("{path}/loop"), "loop"),
        };
        i += 2 + n;
        if kind == 0xfe {
            break;
        }
    }
    let Some(rest) = data.get(i + 1..) else {
        return;
    };

    // The authored path: components from the alphabet, `.`/`..`/`\`
    // sprinkled in, a leaf from the disjoint leaf alphabet.
    let mut authored = String::new();
    for b in rest.iter().take(70) {
        let comp = match b % 10 {
            0 => "..",
            1 => ".",
            2 => "",
            3 => "\\",
            _ => DIRS[(*b as usize / 10) % DIRS.len()],
        };
        authored.push_str(comp);
        authored.push('/');
    }
    let leaf = LEAVES[(rest.first().copied().unwrap_or(0) as usize) % LEAVES.len()];
    authored.push_str(leaf);

    let Ok(root) = WorkspaceRoot::explicit(Path::new("/ws"), &fs) else {
        return;
    };
    if fs.kind_at(root.path()) == Some(EntryKind::Symlink) {
        // `/ws` itself scripted as a link, kept by spelling on the
        // no-realpath backend (an OPERATOR root's documented degraded
        // mode — realpath resolves it everywhere else): every `child`
        // under a link answers absent, so the closed parent-chain
        // invariant below would hold vacuously. Skip the configuration
        // explicitly rather than let the invariant go quiet by accident.
        return;
    }
    // The P1 gate an authored reference passes before minting (RFC 0020's
    // import step owns it in product code): `\` maps to `/`; an empty,
    // NUL-bearing, absolute or scheme-bearing spelling names no key.
    let authored = authored.replace('\\', "/");
    if authored.is_empty()
        || authored.contains('\0')
        || authored.starts_with('/')
        || authored
            .split('/')
            .next()
            .is_some_and(|first| first.contains(':'))
    {
        return;
    }
    fs.clear_probes();
    let minted = SourceKey::mint(&root, Path::new(&authored), &fs, trust);
    let probes = fs.probes();
    assert!(
        !fs.named(leaf),
        "{trust:?} {authored:?}: the leaf was probed: {probes:?}"
    );
    if trust == Trust::Closed {
        assert!(
            !probes.iter().any(|p| matches!(p, Probe::ResolveSymlink(..))),
            "closed trust resolved a symlink: {probes:?}"
        );
    }
    let check_key = |key: &SourceKey| {
        let s = key.as_str();
        assert!(!s.starts_with('/'), "absolute key {s:?}");
        assert!(!s.contains('\\'), "backslash in key {s:?}");
        assert!(key.depth() <= 64, "over-deep key {s:?}");
        for c in s.split('/') {
            assert!(!c.is_empty() && c != "." && c != "..", "bad component in {s:?}");
        }
    };
    match minted {
        Ok(keyed) => {
            check_key(&keyed.key);
            if trust == Trust::Closed {
                // Along the key's parent chain, through EXISTING directories
                // only (the scripted table can hold an orphan node under a
                // file or a link — `a` a file, `a/A/lnk` a link — that no
                // walk can reach): the first component that is not a
                // directory ends what could have been traversed, and none
                // of it is a link. The root is a directory here (a linked
                // root returned above; a denied one never became a root).
                let mut prefix = root.path().to_path_buf();
                assert_eq!(fs.kind_at(&prefix), Some(EntryKind::Dir), "{authored:?}");
                let mut reachable = true;
                for component in keyed.key.dir().components() {
                    if !reachable {
                        break;
                    }
                    prefix.push(component);
                    match fs.kind_at(&prefix) {
                        Some(EntryKind::Symlink) => panic!(
                            "closed trust keyed through a symlink at {}: {authored:?} -> {} \
                             (probes {probes:?})",
                            prefix.display(),
                            keyed.key
                        ),
                        Some(EntryKind::Dir) => {}
                        _ => reachable = false,
                    }
                }
            }
            let verified = keyed.verify(&fs);
            if let Ok(Some(v)) = &verified {
                check_key(&v.key);
            }
            if trust == Trust::Open && keyed.via_symlink != SymlinkVerdict::Unverifiable {
                if let Ok(Some(v)) = &verified {
                    if !v.respelled && v.kind != EntryKind::Symlink {
                        let again =
                            SourceKey::mint(&root, &root.path_of(&keyed.key), &fs, Trust::Open)
                                .expect("re-mint of a verified key");
                        assert_eq!(again.key, keyed.key, "re-mint is a fixed point");
                    }
                }
            }
        }
        Err(PathError::SymlinkComponent { key, .. }) | Err(PathError::Unverifiable { key }) => {
            assert_eq!(trust, Trust::Closed);
            check_key(&key);
        }
        Err(PathError::Escapes { authored: a }) => assert_eq!(a, authored),
        Err(PathError::Fs(FsError::Denied | FsError::SymlinkLoop))
        | Err(PathError::Depth)
        | Err(PathError::NotRelative) => {}
        Err(PathError::Fs(e)) => panic!("untyped fs error {e:?}"),
        Err(PathError::NotUtf8) => panic!("the alphabet is UTF-8"),
        // The P1 gate above maps every `\\` to `/` and the alphabet is
        // plain: a separator can reach `mint` only in a respelled on-disk
        // name, which the mock never produces.
        Err(PathError::NotPlain { component }) => {
            panic!("a plain authored path refused as not plain: {component:?} in {authored:?}")
        }
    }
});
