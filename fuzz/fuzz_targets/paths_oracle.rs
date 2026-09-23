//! The existence-oracle invariant as a fuzz PROPERTY (RFC 0019 item 0,
//! E35; r62-sec finding 2). The same scripted tree is built TWICE —
//! every symlink target as scripted, and every symlink target replaced
//! by a name nothing in the tree carries — and under closed trust
//! `mint`, `verify`, their probe logs and the derived root under a
//! `/ws/.git` fence must be IDENTICAL between the two: no output of the
//! closed pipeline may depend on what any link points at, or whether it
//! points anywhere at all. Open trust is excluded by design (its targets
//! legitimately matter). The type-level half of the same guarantee is
//! the `LstatFs`/`PathFs` split: the closed walk cannot name
//! `resolve_symlink` at all.

#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;
use nml_validate::workspace::{MockFs, Spelling};
use nml_validate::workspace::{SourceKey, Trust, WorkspaceRoot};
use nml_validate::fs::EntryKind;

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

enum Op {
    Dir(String),
    File(String),
    Symlink(String, &'static str),
    Denied(String),
    Loop(String),
}

fn build(ops: &[Op], flags: u8, dangle_all: bool) -> MockFs {
    let mut fs = MockFs::new().file("/ws/x.package.nml").file("/ws/.git");
    if flags & 1 != 0 {
        fs = fs.insensitive();
    }
    if flags & 2 != 0 {
        fs = fs.spelling(Spelling::Membership);
    }
    for op in ops {
        fs = match op {
            Op::Dir(p) => fs.dir(p),
            Op::File(p) => fs.file(p),
            Op::Symlink(p, t) => fs.symlink(p, if dangle_all { "absent-everywhere" } else { t }),
            Op::Denied(p) => fs.denied(p),
            Op::Loop(p) => fs.symlink(p, if dangle_all { "absent-everywhere" } else { "loop" }),
        };
    }
    fs
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let flags = data[0];
    let mut ops = Vec::new();
    let mut i = 1usize;
    while i + 1 < data.len() && data[i] != 0xff {
        let kind = data[i];
        let n = (data[i + 1] as usize) % 7;
        let path_bytes = &data[i + 2..(i + 2 + n).min(data.len())];
        let path = dir_path(path_bytes);
        ops.push(match kind % 5 {
            0 => Op::Dir(path),
            1 => Op::File(format!("{path}/{}", LEAVES[(kind as usize / 5) % LEAVES.len()])),
            2 => Op::Symlink(path, TARGETS[(kind as usize / 5) % TARGETS.len()]),
            3 => Op::Denied(path),
            _ => Op::Loop(format!("{path}/loop")),
        });
        i += 2 + n;
        if kind == 0xfe {
            break;
        }
    }
    let Some(rest) = data.get(i + 1..) else {
        return;
    };
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

    let live = build(&ops, flags, false);
    let dead = build(&ops, flags, true);
    if live.kind_at(Path::new("/ws")) == Some(EntryKind::Symlink) {
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
    let Ok(root_live) = WorkspaceRoot::explicit(Path::new("/ws"), &live) else {
        return;
    };
    let root_dead = WorkspaceRoot::explicit(Path::new("/ws"), &dead).expect("same tree shape");
    assert_eq!(root_live, root_dead, "explicit root depends on link targets");

    // Closed trust: mint + verify + probes identical.
    live.clear_probes();
    dead.clear_probes();
    let m_live = SourceKey::mint(&root_live, Path::new(&authored), &live, Trust::Closed);
    let m_dead = SourceKey::mint(&root_dead, Path::new(&authored), &dead, Trust::Closed);
    assert_eq!(m_live, m_dead, "closed mint is an existence oracle for {authored:?}");
    assert_eq!(live.probes(), dead.probes(), "closed mint probed differently for {authored:?}");
    if let (Ok(kl), Ok(kd)) = (&m_live, &m_dead) {
        live.clear_probes();
        dead.clear_probes();
        assert_eq!(kl.verify(&live), kd.verify(&dead), "closed verify is an oracle for {authored:?}");
        assert_eq!(live.probes(), dead.probes(), "closed verify probed differently for {authored:?}");
    }

    // The derived root under a `.git` fence: identical too (E28).
    let target = format!("/ws/{authored}");
    live.clear_probes();
    dead.clear_probes();
    let d_live = WorkspaceRoot::derive(Path::new(&target), &live);
    let d_dead = WorkspaceRoot::derive(Path::new(&target), &dead);
    assert_eq!(d_live, d_dead, "derive is an existence oracle for {target:?}");
    assert_eq!(live.probes(), dead.probes(), "derive probed differently for {target:?}");
});
