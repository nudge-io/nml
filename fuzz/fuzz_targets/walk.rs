//! The gate-completeness invariant of the discovery walk (RFC 0019 item 0,
//! NML2090's promise): over a scripted tree under `/ws` — plain and
//! hostile names (a dot-directory, `node_modules`, a `\`-bearing name, a
//! dot-file, `.nml` and non-`.nml` files), links, special entries, denied
//! directories and chains nested past the 64-component bound — EVERY entry
//! the tree holds is accounted for by the `Discovery` it produces: it is in
//! `files`, or it sits under a `skipped` row that names why the walk never
//! judged it (its own key, its holding directory's for a name no key can
//! carry, the depth-64 directory's at the component bound), or the walk was
//! truncated (nothing is enumerated then, by design) or the entry lies under
//! a denied budget unit. Two of this invariant's holes shipped through nine
//! review rounds — a `\`-named entry and a directory at the bound were
//! charged and dropped in silence, and `nml check .` certified the content
//! beneath them — so it is a fuzz PROPERTY now, not a pin per shape. The
//! names include ones that are NOT UTF-8 (unix), and the HIDDEN AUDIT of
//! every skipped dot-directory is held to the same promise: each
//! `.nml`-named entry beneath it is counted exactly, or the audit says
//! where it stopped short.

#![no_main]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use libfuzzer_sys::fuzz_target;
use nml_validate::workspace::{MAX_COMPONENTS, MockFs};
use nml_validate::workspace::{
    AuditBudget, EntryKind, InputKind, Skip, SourceKey, WorkspaceRoot, audit_hidden, discover,
    walk_skips_dir,
};

const NAMES: [&str; 10] = [
    "a",
    "b",
    ".h",
    "node_modules",
    "a\\b",
    "x.nml",
    "y.flow.nml",
    "z\\.nml",
    "n.txt",
    ".d.nml",
];

/// The name vocabulary: the ten above and, on unix, two that are not
/// UTF-8 (a bare one and a `.nml`-named one).
fn names() -> Vec<OsString> {
    let mut out: Vec<OsString> = NAMES.iter().map(OsString::from).collect();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        out.push(std::ffi::OsStr::from_bytes(b"\xff").to_os_string());
        out.push(std::ffi::OsStr::from_bytes(b"n\xff.nml").to_os_string());
    }
    out
}

/// A name no key can carry: not UTF-8, or not a plain component.
fn unkeyable(name: &OsString) -> bool {
    name.to_str().is_none_or(|s| SourceKey::checked(s).is_none())
}

fn is_nml(name: &OsString) -> bool {
    name.as_encoded_bytes().ends_with(b".nml")
}
const MANIFEST: &str = "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    - core:\n        file = \"core.model.nml\"\n\n[]validator validators:\n    - tenantFlows:\n        files:\n            - \"a/**/*.flow.nml\"\n        schemas:\n            - core\n";
const MODEL: &str = "model thing:\n    v: string\n";

fn path_of(comps: &[OsString]) -> PathBuf {
    let mut p = PathBuf::from("/ws");
    for c in comps {
        p.push(c);
    }
    p
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let mut fs = MockFs::new()
        .file("/ws/demo.package.nml")
        .file("/ws/core.model.nml");
    let names = names();
    let mut built: Vec<Vec<OsString>> = Vec::new();
    let mut i = 0usize;
    while i + 1 < data.len() {
        let kind = data[i];
        let n = (data[i + 1] as usize) % 8;
        let bytes = &data[i + 2..(i + 2 + n).min(data.len())];
        let mut comps: Vec<OsString> = bytes
            .iter()
            .map(|b| names[(*b as usize) % names.len()].clone())
            .collect();
        let op = kind % 6;
        if op == 5 {
            // A chain nested around the component bound, a file at the bottom.
            let depth = 60 + (kind as usize / 6) % 8;
            comps = (0..depth).map(|d| OsString::from(format!("c{d}"))).collect();
            comps.push(OsString::from("deep.flow.nml"));
        }
        i += 2 + n;
        if comps.is_empty() {
            continue;
        }
        let p = path_of(&comps);
        fs = match op {
            0 => fs.dir(&p),
            1 | 5 => fs.file(&p),
            2 => fs.symlink(&p, ["a", "../a", "nowhere"][(kind as usize / 6) % 3]),
            3 => fs.other(&p),
            _ => fs.denied(&p),
        };
        built.push(comps);
    }
    let Ok(root) = WorkspaceRoot::explicit(Path::new("/ws"), &fs) else {
        return;
    };
    let read = |kind: InputKind, path: &Path| -> Result<String, String> {
        match (kind, path.to_str()) {
            (InputKind::Manifest, Some("/ws/demo.package.nml")) => Ok(MANIFEST.to_string()),
            (InputKind::Source, Some("/ws/core.model.nml")) => Ok(MODEL.to_string()),
            _ => Err("unreadable".to_string()),
        }
    };
    let d = discover(&root, &fs, &read, vec![], std::sync::Arc::default());
    if d.truncated().is_some() {
        assert!(d.files().is_empty() && d.skipped().is_empty(), "a truncated walk enumerates nothing");
        return;
    }
    let row = |key: &SourceKey, pred: &dyn Fn(&Skip) -> bool| {
        d.skipped().iter().any(|s| &s.key == key && pred(&s.why))
    };
    'entries: for comps in &built {
        let mut prefix = SourceKey::root();
        for (k, name) in comps.iter().enumerate() {
            let last = k + 1 == comps.len();
            let here = path_of(&comps[..=k]);
            // The mock's own truth (a later op may have respelled a node;
            // an orphan under a file or a link is unreachable by any walk).
            let Some(kind) = fs.kind_at(&here) else {
                continue 'entries;
            };
            let here = here.display();
            if unkeyable(name) {
                let content_like =
                    matches!(kind, EntryKind::Dir | EntryKind::Symlink) || is_nml(name);
                if content_like {
                    let lossy = name.to_string_lossy();
                    assert!(
                        row(&prefix, &|why| {
                            matches!(why, Skip::UnkeyableName { kind: k, name: n } if *k == kind && *n == lossy)
                        }),
                        "unreported unkeyable {here} ({kind:?}) under {prefix:?}: {:?}",
                        d.skipped()
                    );
                }
                continue 'entries;
            }
            // Keyable: UTF-8 and plain by the test above.
            let name = name.to_str().expect("keyable names are UTF-8");
            let this = prefix.join(name);
            if d.truncated_units().iter().any(|u| u.unit.contains(&this)) {
                continue 'entries;
            }
            match kind {
                EntryKind::Dir => {
                    if walk_skips_dir(name) {
                        assert!(
                            row(&this, &|why| matches!(why, Skip::DotDirectory | Skip::PolicyDirectory)),
                            "unreported policy skip {here}: {:?}",
                            d.skipped()
                        );
                        continue 'entries;
                    }
                    if this.depth() >= MAX_COMPONENTS {
                        assert!(
                            row(&this, &|why| *why == Skip::ComponentBound),
                            "unreported component-bound directory {here}: {:?}",
                            d.skipped()
                        );
                        continue 'entries;
                    }
                    prefix = this;
                }
                EntryKind::Symlink => {
                    assert!(
                        row(&this, &|why| *why == Skip::Symlink),
                        "unreported link {here}: {:?}",
                        d.skipped()
                    );
                    continue 'entries;
                }
                EntryKind::File => {
                    if !last {
                        continue 'entries;
                    }
                    if name.starts_with('.') && name.ends_with(".nml") {
                        assert!(row(&this, &|why| *why == Skip::DotFile), "unreported dot-file {here}");
                    }
                    assert!(d.files().contains(&this), "file {here} neither enumerated nor reported: {:?}", d.files());
                }
                EntryKind::Other => {
                    if !last {
                        continue 'entries;
                    }
                    if name.ends_with(".nml") {
                        assert!(row(&this, &|why| *why == Skip::Fifo), "unreported special entry {here}");
                    }
                }
            }
        }
    }
    // The hidden audit's promise, per skipped dot-directory: every
    // `.nml`-named entry (file, link, special) beneath it that the audit
    // could reach is counted exactly, and content it could not reach —
    // under a directory whose name no key carries or at the component
    // bound — is announced by `incomplete`. The truth is recomputed from
    // the mock, never from the audit's own classifier.
    for hidden in d.skipped().iter().filter(|s| s.why == Skip::DotDirectory) {
        let audit = audit_hidden(&root, &fs, &hidden.key, &mut AuditBudget::default());
        let under: Vec<&str> = hidden.key.components().collect();
        // Distinct entries: two ops may script the same path.
        let mut expected: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
        let mut unreachable = false;
        'hidden: for comps in &built {
            if comps.len() <= under.len()
                || !under.iter().zip(comps).all(|(u, c)| c.to_str() == Some(u))
            {
                continue;
            }
            let mut depth = under.len();
            for (k, name) in comps.iter().enumerate().skip(under.len()) {
                let last = k + 1 == comps.len();
                let Some(kind) = fs.kind_at(&path_of(&comps[..=k])) else {
                    continue 'hidden;
                };
                match kind {
                    EntryKind::Dir if last => continue 'hidden,
                    EntryKind::Dir => {
                        if unkeyable(name) {
                            unreachable = true;
                            continue 'hidden;
                        }
                        let plain = name.to_str().expect("keyable");
                        // Never audited: `.git` and the policy directories
                        // (a nested dot-directory IS descended).
                        if plain == ".git" || (walk_skips_dir(plain) && !plain.starts_with('.')) {
                            continue 'hidden;
                        }
                        depth += 1;
                        if depth >= MAX_COMPONENTS {
                            unreachable = true;
                            continue 'hidden;
                        }
                    }
                    _ if !last => continue 'hidden,
                    _ => {
                        if is_nml(name) {
                            expected.insert(path_of(comps));
                        }
                    }
                }
            }
        }
        if unreachable {
            assert!(
                audit.incomplete.is_some(),
                "audit of {:?} complete over content it could not reach: {audit:?}",
                hidden.key
            );
        } else if audit.incomplete.is_none() {
            assert_eq!(
                audit.nml,
                expected.len(),
                "audit of {:?} miscounted its `.nml` entries ({expected:?}): {audit:?}",
                hidden.key
            );
        }
    }
});
