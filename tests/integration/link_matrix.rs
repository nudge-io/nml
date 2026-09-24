//! The link matrix, in-repo (RFC 0019 item 0, r69b item 8; arch r68
//! finding 6): the r56 `fix` matrix (200 rows over the closed link tree,
//! dry-run and real, derived and `--root .`), the open-tree rows, and the
//! r60 probe rows (operator aliases outside the root, `..` after an
//! absent, a file, a followed or a halted component, loops, EACCES,
//! case and Unicode spellings, paths past `PATH_MAX`, the `/tmp` alias)
//! — 318 rows, table-driven from `tests/fixtures/link-matrix/rows.txt`.
//!
//! Two properties per row, over two halves of one tree that differ ONLY
//! in whether every author link's target exists (`a`) or dangles (`b`):
//!
//! 1. **a/b byte identity** (E26): exit code, stdout and stderr are
//!    identical — the kernel never observes a link's target under a
//!    closed binding, so nothing it prints can depend on one.
//! 2. **a golden first line** per row (`tests/fixtures/link-matrix/
//!    expected.txt`, `NML_UPDATE_GOLDEN=1` rewrites): the exit code and
//!    the first stdout and stderr lines, with the tree's paths spelled
//!    `<dir>`, the 240-char component `<L240>` and an OS errno number
//!    `N` — and every golden line must match one of the documented
//!    [`SHAPES`], so a new sentence cannot land unreviewed.
//!
//! Rows tagged `{ci}` need a lookup-insensitive filesystem, rows
//! tagged `{locked}` need `chmod 0` to bite (not root), `{pathmax}`
//! needs listing the five 240-char components under `tenants/cu` to hit
//! `ENAMETOOLONG` (typical when `PATH_MAX` is ~1024 — not Linux CI),
//! and `{tmp-link}` needs `/tmp` to be a symlink (macOS — not Linux CI);
//! on a platform where the tag is unmet the row is skipped — its golden line stays
//! (generated where it ran), never rewritten to "skipped". Rows tagged
//! `{ab-free}` are the ones whose a/b identity E26 does NOT promise:
//! the spelling names the link's target itself (`vendor` exists only in
//! half `a`), walks the whole root, or reaches the author link through
//! an operator alias outside the root (followed by design, E21) — their
//! golden records both halves (`… || b: …`), so the difference is
//! reviewed, not hidden.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The documented shapes a golden line's `out:`/`err:` halves may take —
/// the review, as code (r69b: every line of the generated golden was
/// read and sorted into exactly these). A half is `-` (an empty stream)
/// or CONTAINS one of these fragments, matched after the `<dir>`/
/// `<L240>`/errno normalization.
const SHAPES: &[&str] = &[
    // `fix`'s summary line (dry-run / applied), the only stdout of a fix.
    " edit(s) would apply across ",
    " edit(s) applied across ",
    // `check`'s success line — the workspace key after link resolution (not
    // the operator's typed spelling), sanitized (A1).
    ": ok (",
    // `binding`'s first row.
    "file      ",
    // NML2083, locationless against the key — the whole point of the
    // matrix: identical whether or not the link's target exists; the
    // `(key …)` form for a `..`-spelled argument (r68 UX F8).
    ": error[NML2083]: closed binding rejects `",
    // NML2089: the `p/` tree's locked directory closes the universe for
    // every row under it (A16; the kernel walk cannot list it).
    ": error[NML2089]: cannot enumerate manifests: the walk stopped at `",
    // P1: a spelling with no file name (`lib/..`, `lib/../`, `.`-final
    // after a link) is not a file candidate.
    ": not a relative path (no scheme, no leading separator, no drive prefix; must name a file)",
    // r85 (r84-cov F15): the EMPTY argument is the invocation's mistake in
    // every verb — a usage error, exit 2 — never a file candidate.
    "error: an empty argument is not a path; usage: nml ",
    // E35 sec 1c: the FIFO named as a target, refused before any open —
    // the sentence names what it is and the remedy.
    "is not a regular file — a FIFO, socket or device is never opened; replace it with a \
     regular file",
    // The open tree reads by path (its links are the developer's own): an
    // absent target, a `..` after a file — the OS's own errno, number
    // normalized; and `fix`'s directory expansion reads the typed path,
    // so `proj/nope/../vendor` is the OS's ENOENT (kernel lexical vs OS
    // resolution — recorded, owner's item).
    "(os error N)",
    // A typed link to a DIRECTORY in the open tree: the resolved leaf is
    // a directory, refused in the walk's own words at the open (it was
    // the OS's `Is a directory`).
    " is a directory the universe walk did not enter (",
    // `binding`'s block notes feed the run's explain hint as every verb's
    // rows do: a row whose finding sits in the block (stdout) has the
    // hint as its only stderr line.
    "for more information, run: nml explain ",
    // P2: an argument that leaves the root (an alias beside it, `..`
    // past it, a case-variant root spelling, the `/tmp/..` prefix).
    " is outside the workspace root ",
    // ELOOP through an operator alias outside the root (`oloop`).
    ": symlink loop",
    // A `--root` spelled from a physical cwd (through an alias) that is
    // not a directory.
    "error: --root ",
    // `-` as a target, and — in half `b` of an `{ab-free}` row — the
    // link's target named directly, absent.
    ": no such file or directory",
    // The open tree's dangling leaf link, in the resolver's own words
    // (r77: the open universe's read resolves the leaf once and opens
    // beneath it, as `fmt` and `parse` have since r75).
    "is a symlink whose target cannot be resolved",
    // EACCES on the operator's own prefix (`olocked/link/…`): the fold
    // fails closed, typed, never "absent".
    ": permission denied on a path component",
    // A many-target `fix` whose one target failed.
    "error: 1 path(s) could not be fixed",
];

struct Row {
    label: String,
    cwd: String,
    root: String,
    verb: String,
    spelling: String,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn rows() -> Vec<Row> {
    let text = std::fs::read_to_string(repo_root().join("tests/fixtures/link-matrix/rows.txt"))
        .expect("rows.txt reads");
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split(" | ").collect();
        assert_eq!(fields.len(), 5, "malformed row: {line}");
        rows.push(Row {
            label: fields[0].to_string(),
            cwd: fields[1].to_string(),
            root: fields[2].to_string(),
            verb: fields[3].to_string(),
            spelling: fields[4].to_string(),
        });
    }
    rows
}

/// The tree: `<typed>/{a,b}/…` — `typed` is `std::env::temp_dir()` as
/// spelled (on macOS a symlink, `/var/…`), `real` its realpath, so the
/// `<dir>`/`<real>` rows exercise an operator alias ABOVE the root; on
/// a platform where the two coincide those rows still run, as twins.
struct Tree {
    typed: PathBuf,
    real: PathBuf,
    locked_bites: bool,
    insensitive: bool,
    /// Listing the fifth 240-char component under `l/proj/tenants/cu` fails
    /// (ENAMETOOLONG on macOS); Linux CI paths are short enough to succeed.
    long_path_bites: bool,
    /// `/tmp` is a symlink (macOS `/private/tmp`); without it, `/tmp/..`
    /// + `<real>` does not refuse C25 before the alias-cu fold enters `proj`.
    tmp_link: bool,
}

impl Drop for Tree {
    fn drop(&mut self) {
        unlock(&self.real);
        let _ = std::fs::remove_dir_all(&self.real);
    }
}

fn unlock(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::read_dir(dir).map(|e| {
            for e in e.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    unlock(&e.path());
                }
            }
        });
        return;
    };
    for e in entries.flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            unlock(&e.path());
        }
    }
}

fn symlink(target: &str, at: &Path) {
    std::os::unix::fs::symlink(target, at).unwrap_or_else(|e| panic!("{}: {e}", at.display()));
}

fn write(at: &Path, text: &str) {
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::fs::write(at, text).unwrap();
}

fn lock(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).unwrap();
}

/// Copy a fixture tree verbatim — symlinks AS symlinks.
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = std::fs::symlink_metadata(&from).unwrap();
        if meta.file_type().is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&from).unwrap(), &to).unwrap();
        } else if meta.is_dir() {
            copy_tree(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

const THING: &str = "thing base:\n    v = \"b\"\n";

/// What a base carries beyond the shared tree: nothing (`<half>/`), a
/// locked directory inside the root (`<half>/p/`), or a directory chain
/// past `PATH_MAX` inside the root (`<half>/l/`). Each of the last two
/// closes the universe for EVERY row under it (A16: the walk cannot list
/// the directory — EACCES, ENAMETOOLONG), which is why they are bases of
/// their own.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    Plain,
    Locked,
    LongPath,
}

/// One base: the closed tree `proj` (the `workspace-link-<half>` fixture
/// plus a `.git` and the r60 links), the operator aliases beside it, the
/// open tree `o`, and the variant's own hazard.
fn build_base(base: &Path, half: &str, variant: Variant) {
    let exists = half == "a";
    let proj = base.join("proj");
    copy_tree(
        &repo_root().join(format!("tests/fixtures/workspace-link-{half}")),
        &proj,
    );
    let cu = proj.join("tenants/cu");
    std::fs::create_dir_all(proj.join(".git")).unwrap();
    if exists {
        std::fs::create_dir_all(base.join("probe")).unwrap();
        symlink("../..", &cu.join("toroot"));
        symlink("../../../probe/../proj", &cu.join("probe"));
        symlink("../../vendor/base.flow.nml", &cu.join("leaf.flow.nml"));
        symlink("../vendor", &proj.join("tenants/lib3"));
    } else {
        symlink("../../nope", &cu.join("toroot"));
        symlink("../../../probe-nope/../proj", &cu.join("probe"));
        symlink("../../nowhere/base.flow.nml", &cu.join("leaf.flow.nml"));
        symlink("../nope", &proj.join("tenants/lib3"));
    }
    // Operator aliases outside the root (followed: not author-writable).
    symlink("proj/tenants/cu", &base.join("alias-cu"));
    symlink("proj/vendor", &base.join("alias-v"));
    symlink("proj", &base.join("alias-root"));
    symlink("proj/tenants/cu/lib", &base.join("alias-lib"));
    symlink("proj/tenants/cu", &base.join("alias-cu2"));
    symlink("alias-cu2", &base.join("alias-chain"));
    symlink("proj/tenants/cu/toroot", &base.join("alias-toroot"));
    symlink("oloop", &base.join("oloop"));
    symlink("loop", &cu.join("loop"));
    symlink("../../vendor", &cu.join("lib2"));
    write(&cu.join("real/r.flow.nml"), THING);
    write(&cu.join("caf\u{e9}/c.flow.nml"), THING);
    write(&base.join("projx/vendor/s.flow.nml"), THING);
    assert!(
        Command::new("mkfifo")
            .arg(cu.join("fifo.flow.nml"))
            .status()
            .expect("mkfifo runs")
            .success()
    );
    // A locked operator directory outside the root holding a link back.
    std::fs::create_dir_all(base.join("olocked")).unwrap();
    symlink("../proj", &base.join("olocked/link"));
    lock(&base.join("olocked"));
    if variant == Variant::Locked {
        write(&cu.join("locked/sub/l.flow.nml"), THING);
        lock(&cu.join("locked"));
    }
    if variant == Variant::LongPath {
        // Five 240-char components: past PATH_MAX as one string, so the
        // chain is built from inside (a shell child's own cwd). The walk
        // meets ENAMETOOLONG listing the fifth — a truncation.
        let l = "L".repeat(240);
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "for i in 1 2 3 4 5; do mkdir {l} && cd {l} || exit 1; done && \
                 printf 'thing z:\\n    v = \"z\"\\n' > z.flow.nml"
            ))
            .current_dir(&cu)
            .status()
            .expect("sh runs");
        assert!(status.success(), "long path built");
    }
    // The open tree: no manifest anywhere, a developer's own links.
    let o = base.join("o");
    write(&o.join("real/x.nml"), "thing t:\n    a = 1\n");
    write(&o.join("other/y.nml"), "thing u:\n    a = 1\n");
    symlink("../other", &o.join("real/inner"));
    symlink("x.nml", &o.join("real/l.nml"));
    symlink("real", &o.join("link"));
    symlink("real/x.nml", &o.join("leaf.nml"));
    symlink("nowhere", &o.join("dangle"));
}

fn build_tree() -> Tree {
    let mut typed = std::env::temp_dir();
    // A trailing separator would double in the spelled paths.
    if typed.as_os_str().to_string_lossy().ends_with('/') {
        typed = PathBuf::from(typed.to_string_lossy().trim_end_matches('/'));
    }
    let typed = typed.join(format!("nml-link-matrix-{}", std::process::id()));
    unlock(&typed);
    let _ = std::fs::remove_dir_all(&typed);
    std::fs::create_dir_all(&typed).unwrap();
    let real = std::fs::canonicalize(&typed).unwrap();
    for half in ["a", "b"] {
        build_base(&typed.join(half), half, Variant::Plain);
        build_base(&typed.join(half).join("p"), half, Variant::Locked);
        build_base(&typed.join(half).join("l"), half, Variant::LongPath);
    }
    // Self-selection: does `chmod 0` bite (not root), does the
    // filesystem find `VENDOR` for `vendor`, does the long-path base
    // truncate discovery, and does `/tmp` alias like macOS?
    let locked_bites = std::fs::read_dir(typed.join("a/olocked")).is_err();
    let insensitive = std::fs::symlink_metadata(typed.join("a/proj/VENDOR")).is_ok();
    let long_path_bites = long_path_enametoolong(&typed);
    let tmp_link = std::fs::symlink_metadata("/tmp")
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    Tree {
        typed,
        real,
        locked_bites,
        insensitive,
        long_path_bites,
        tmp_link,
    }
}

/// After [`build_base`]'s five 240-char components: can the kernel's walk
/// list into the fifth? When `PATH_MAX` is tight the listing fails — the
/// `{pathmax}` rows' goldens.
fn long_path_enametoolong(typed: &Path) -> bool {
    let l = "L".repeat(240);
    let mut p = typed.join("a/l/proj/tenants/cu");
    for _ in 0..4 {
        p = p.join(&l);
    }
    let Ok(it) = std::fs::read_dir(&p) else {
        return true;
    };
    if let Some(e) = it.flatten().next() {
        let child = p.join(e.file_name());
        return std::fs::read_dir(&child).is_err();
    }
    false
}

/// Run one row in one half with a watchdog (a hang is a failure, and the
/// child is killed first): `(exit, stdout, stderr)`.
fn run_row(tree: &Tree, half: &str, row: &Row) -> (i32, String, String) {
    use std::io::Read;
    let l240 = "L".repeat(240);
    let sub = |s: &str| -> String {
        s.replace("<real>", &tree.real.join(half).to_string_lossy())
            .replace("<dir>", &tree.typed.join(half).to_string_lossy())
            .replace("<L240>", &l240)
            .replace("<empty>", "")
    };
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nml"));
    // The goldens carry the sentences' Unicode spelling (r88 P6).
    cmd.env("NML_UNICODE", "1");
    cmd.current_dir(tree.typed.join(half).join(&row.cwd));
    cmd.args(row.verb.split_whitespace());
    if row.root != "-" {
        cmd.args(["--root", &sub(&row.root)]);
    }
    cmd.arg(sub(&row.spelling));
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("nml runs");
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stdout.read_to_end(&mut b);
        b
    });
    let err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stderr.read_to_end(&mut b);
        b
    });
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > std::time::Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{}: hung (killed after 30 s)", row.label);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let norm = |bytes: Vec<u8>| -> String {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let text = text.replace(&*tree.real.join(half).to_string_lossy(), "<dir>");
        let text = text.replace(&*tree.typed.join(half).to_string_lossy(), "<dir>");
        let text = text.replace(&l240, "<L240>");
        normalize_errno(&text)
    };
    (
        status.code().unwrap_or(-1),
        norm(out.join().unwrap()),
        norm(err.join().unwrap()),
    )
}

/// `(os error 63)` → `(os error N)`: ENAMETOOLONG and friends are numbered
/// per platform; the golden is not.
fn normalize_errno(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("(os error ") {
        let after = &rest[at + "(os error ".len()..];
        let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 && after[digits..].starts_with(')') {
            out.push_str(&rest[..at]);
            out.push_str("(os error N)");
            rest = &after[digits + 1..];
        } else {
            out.push_str(&rest[..at + "(os error ".len()]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("-")
}

fn shape_ok(half: &str) -> bool {
    half == "-" || SHAPES.iter().any(|s| half.contains(s))
}

#[test]
fn link_matrix_rows_are_ab_identical_and_match_their_goldens() {
    let tree = build_tree();
    let rows = rows();
    let golden_path = repo_root().join("tests/fixtures/link-matrix/expected.txt");
    let update = std::env::var_os("NML_UPDATE_GOLDEN").is_some();
    let expected: BTreeMap<String, String> = if update {
        BTreeMap::new()
    } else {
        std::fs::read_to_string(&golden_path)
            .unwrap_or_else(|e| panic!("golden unreadable ({e}) — NML_UPDATE_GOLDEN=1 to create"))
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .map(|l| {
                let (label, rest) = l.split_once(" => ").expect("`label => line`");
                (label.to_string(), rest.to_string())
            })
            .collect()
    };
    let mut lines = Vec::new();
    let mut skipped = Vec::new();
    let mut failures = Vec::new();
    for row in &rows {
        let unmet = (row.label.contains("{ci}") && !tree.insensitive)
            || (row.label.contains("{locked}") && !tree.locked_bites)
            || (row.label.contains("{pathmax}") && !tree.long_path_bites)
            || (row.label.contains("{tmp-link}") && !tree.tmp_link);
        if unmet {
            skipped.push(row.label.clone());
            if let Some(line) = expected.get(&row.label) {
                lines.push(format!("{} => {line}", row.label));
            }
            continue;
        }
        let a = run_row(&tree, "a", row);
        let b = run_row(&tree, "b", row);
        let ab_free = row.label.contains("{ab-free}");
        if a != b && !ab_free {
            failures.push(format!("{}: a/b differ\n  a: {a:?}\n  b: {b:?}", row.label));
        }
        let summary = |r: &(i32, String, String)| {
            format!(
                "exit={} | out: {} | err: {}",
                r.0,
                first_line(&r.1),
                first_line(&r.2)
            )
        };
        let line = if ab_free {
            format!("{} || b: {}", summary(&a), summary(&b))
        } else {
            summary(&a)
        };
        for r in [&a, &b] {
            if !shape_ok(first_line(&r.1)) || !shape_ok(first_line(&r.2)) {
                failures.push(format!(
                    "{}: an undocumented shape (add it to SHAPES after review): {}",
                    row.label,
                    summary(r)
                ));
            }
        }
        if !update {
            match expected.get(&row.label) {
                Some(want) if want == &line => {}
                Some(want) => failures.push(format!(
                    "{}: golden drifted (NML_UPDATE_GOLDEN=1 to accept)\n  want: {want}\n  got:  {line}",
                    row.label
                )),
                None => failures.push(format!("{}: no golden line", row.label)),
            }
        }
        lines.push(format!("{} => {line}", row.label));
    }
    if update {
        let mut text = String::from(
            "# The link matrix goldens (tests/integration/link_matrix.rs): `label => exit | \
             out: first stdout line | err: first stderr line`, paths spelled <dir>.\n\
             # Regenerate with NML_UPDATE_GOLDEN=1; review the diff — that IS the change.\n",
        );
        for l in &lines {
            text.push_str(l);
            text.push('\n');
        }
        std::fs::write(&golden_path, text).unwrap();
    }
    assert!(
        failures.is_empty(),
        "{} row(s) failed (skipped {}: {:?}):\n{}",
        failures.len(),
        skipped.len(),
        skipped,
        failures.join("\n")
    );
    assert!(rows.len() >= 300, "{} rows", rows.len());
}
