//! The read-through's race harness (E35): a flipper thread swaps link/file/dir shapes under
//! `open_beneath` / `write_beneath` while the main thread hammers them.
//! Invariants: an open never yields bytes from outside the root; a write
//! never lands outside the root; every error is a typed refusal.
#![cfg(unix)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use nml_validate::workspace::{OpenError, ReadError, open_beneath, read_beneath, write_beneath};

/// The perf tier's iteration count (700 opens/writes per shape,
/// 2,100 for the hard-link shape) — `#[ignore]`d, run with `cargo test -p
/// nml-validate --release --test beneath_race -- --ignored perf_`.
const ITERS: usize = 700;
/// The default lane's count: enough for every shape to see both sides of
/// its flip on a loaded CI runner (measured: ≥ 19 refusals per 120
/// iterations here), a fraction of the perf tier's wall time — and
/// [`attempts`] gives it twice as many again while it has not, so
/// [`proved`] fires only where the flip really never lands.
const FAST: usize = 120;

/// How many attempts a shape makes: its lane's count, and — in the
/// DEFAULT lane only — up to twice as many again while the flip has not
/// been observed yet, so a loaded or single-core runner still gets to
/// see both sides before [`proved`] judges it. The perf tier runs
/// exactly its own count (it measures throughput, not luck).
fn attempts(iters: usize) -> usize {
    if iters < ITERS { iters * 3 } else { iters }
}

/// Every shape here pins a REFUSAL the guard makes only while the
/// flipper holds the hostile shape. A run in which the flipper never won
/// asserts nothing about the guard: the `Ok` arm's bytes are the inside
/// file's whatever the guard does. Shape 6 has always said so; shapes
/// 1-5 printed the counter and left the judgement to a human reading the
/// log — so a scheduler that never yielded to the flipper (a single-core
/// CI runner, a container under load, a flipper whose renames all failed)
/// was a GREEN run that proved nothing, and the mutation that removes
/// `O_NOFOLLOW` would have survived it.
fn proved(seen: usize, shape: &str, flips: usize) {
    assert!(
        seen > 0,
        "{shape}: the guard refused nothing in any attempt (flips={flips}) — the flipper never \
         won the race, so this run proves nothing about it; re-run, and if it never lands here \
         the harness needs a stronger flip, not a weaker assertion"
    );
}

/// The harness's own judge and retry, pinned: a `proved` that always
/// passed would turn every shape back into the green-that-proves-nothing
/// it was, and an `attempts` that never retried would hand a loaded
/// runner exactly its lane's count and no more.
#[test]
#[should_panic(expected = "the flipper never won the race")]
fn proved_refuses_a_run_the_flipper_never_won() {
    proved(0, "shape0 self-test", 12);
}

#[test]
fn attempts_retries_in_the_default_lane_only() {
    assert_eq!(attempts(FAST), 3 * FAST);
    assert_eq!(attempts(ITERS - 1), 3 * (ITERS - 1));
    assert_eq!(attempts(ITERS), ITERS);
    assert_eq!(attempts(ITERS + 1), ITERS + 1);
    proved(1, "shape0 self-test", 0);
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn scratch(tag: &str) -> Scratch {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("r80-race-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Scratch(std::fs::canonicalize(&d).unwrap())
}

fn read_all(mut f: std::fs::File) -> String {
    let mut s = String::new();
    f.read_to_string(&mut s).unwrap();
    s
}

/// Shape 1: the PARENT directory `tenants/cu` is flipped between a real
/// directory and a symlink to `outside/` (which holds a file of the same
/// name with OUTSIDE bytes).
fn parent_dir_flipped_to_link_during_open_never_reads_outside_for(iters: usize) {
    let d = scratch("parent-open");
    let root = d.0.join("root");
    std::fs::create_dir_all(root.join("tenants/cu.real")).unwrap();
    std::fs::create_dir_all(d.0.join("outside")).unwrap();
    std::fs::write(root.join("tenants/cu.real/f.nml"), "INSIDE").unwrap();
    std::fs::write(d.0.join("outside/f.nml"), "OUTSIDE").unwrap();
    std::fs::rename(root.join("tenants/cu.real"), root.join("tenants/cu")).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flips = Arc::new(AtomicUsize::new(0));
    let (r2, s2, f2) = (root.clone(), Arc::clone(&stop), Arc::clone(&flips));
    let outside = d.0.join("outside");
    let flipper = std::thread::spawn(move || {
        let cu = r2.join("tenants/cu");
        let real = r2.join("tenants/cu.real");
        while !s2.load(Ordering::Relaxed) {
            // dir -> link
            let _ = std::fs::rename(&cu, &real);
            let _ = std::os::unix::fs::symlink(&outside, &cu);
            // link -> dir
            let _ = std::fs::remove_file(&cu);
            let _ = std::fs::rename(&real, &cu);
            f2.fetch_add(1, Ordering::Relaxed);
        }
    });
    let (mut ok, mut refused_link, mut other) = (0, 0, 0);
    let started = Instant::now();
    for i in 0..attempts(iters) {
        if refused_link > 0 && i >= iters {
            break;
        }
        match open_beneath(&root, &["tenants", "cu", "f.nml"]) {
            Ok(f) => {
                let s = read_all(f);
                assert_eq!(s, "INSIDE", "read OUTSIDE bytes through a flipped parent");
                ok += 1;
            }
            Err(OpenError::Symlink { component }) => {
                assert_eq!(component, "cu");
                refused_link += 1;
            }
            Err(OpenError::NotADirectory { .. }) | Err(OpenError::Io(_)) => other += 1,
            Err(e) => panic!("unexpected {e}"),
        }
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    flipper.join().unwrap();
    eprintln!(
        "shape1 parent-open: iters={iters} ok={ok} refused_link={refused_link} other={other} flips={} in {:?}",
        flips.load(Ordering::Relaxed),
        started.elapsed()
    );
    proved(
        refused_link,
        "shape1 parent-open",
        flips.load(Ordering::Relaxed),
    );
}

/// Shape 2: the LEAF `f.nml` is flipped between a regular file and a
/// symlink to an outside file.
fn leaf_flipped_to_link_during_open_never_reads_outside_for(iters: usize) {
    let d = scratch("leaf-open");
    let root = d.0.join("root");
    std::fs::create_dir_all(root.join("t")).unwrap();
    std::fs::write(root.join("t/f.nml"), "INSIDE").unwrap();
    std::fs::write(d.0.join("outside.nml"), "OUTSIDE").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flips = Arc::new(AtomicUsize::new(0));
    let (r2, s2, f2) = (root.clone(), Arc::clone(&stop), Arc::clone(&flips));
    let outside = d.0.join("outside.nml");
    let flipper = std::thread::spawn(move || {
        let leaf = r2.join("t/f.nml");
        let tmp = r2.join("t/f.link");
        while !s2.load(Ordering::Relaxed) {
            let _ = std::os::unix::fs::symlink(&outside, &tmp);
            let _ = std::fs::rename(&leaf, r2.join("t/f.real"));
            let _ = std::fs::rename(&tmp, &leaf); // now a link
            let _ = std::fs::rename(r2.join("t/f.real"), &leaf); // back to the file (replaces the link)
            f2.fetch_add(1, Ordering::Relaxed);
        }
    });
    let (mut ok, mut refused_link, mut other) = (0, 0, 0);
    for i in 0..attempts(iters) {
        if refused_link > 0 && i >= iters {
            break;
        }
        match open_beneath(&root, &["t", "f.nml"]) {
            Ok(f) => {
                assert_eq!(
                    read_all(f),
                    "INSIDE",
                    "read OUTSIDE bytes through a flipped leaf"
                );
                ok += 1;
            }
            Err(OpenError::Symlink { component }) => {
                assert_eq!(component, "f.nml");
                refused_link += 1;
            }
            Err(OpenError::Io(_)) => other += 1,
            Err(e) => panic!("unexpected {e}"),
        }
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    flipper.join().unwrap();
    eprintln!(
        "shape2 leaf-open: iters={iters} ok={ok} refused_link={refused_link} other={other} flips={}",
        flips.load(Ordering::Relaxed)
    );
    proved(
        refused_link,
        "shape2 leaf-open",
        flips.load(Ordering::Relaxed),
    );
}

/// Shape 3: the PARENT is flipped dir<->link-to-outside during
/// `write_beneath`; the outside directory must never receive a file and
/// the inside file must always hold either the old or the new bytes.
fn parent_dir_flipped_to_link_during_write_never_writes_outside_for(iters: usize) {
    let d = scratch("parent-write");
    let root = d.0.join("root");
    std::fs::create_dir_all(root.join("tenants/cu")).unwrap();
    std::fs::create_dir_all(d.0.join("outside")).unwrap();
    std::fs::write(root.join("tenants/cu/f.nml"), "OLD").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flips = Arc::new(AtomicUsize::new(0));
    let (r2, s2, f2) = (root.clone(), Arc::clone(&stop), Arc::clone(&flips));
    let outside = d.0.join("outside");
    let flipper = std::thread::spawn(move || {
        let cu = r2.join("tenants/cu");
        let real = r2.join("tenants/cu.real");
        while !s2.load(Ordering::Relaxed) {
            let _ = std::fs::rename(&cu, &real);
            let _ = std::os::unix::fs::symlink(&outside, &cu);
            let _ = std::fs::remove_file(&cu);
            let _ = std::fs::rename(&real, &cu);
            f2.fetch_add(1, Ordering::Relaxed);
        }
    });
    let (mut ok, mut refused_link, mut other) = (0, 0, 0);
    for i in 0..attempts(iters) {
        if refused_link > 0 && i >= iters {
            break;
        }
        let body = format!("NEW{i}");
        match write_beneath(&root, &["tenants", "cu", "f.nml"], body.as_bytes()) {
            Ok(()) => ok += 1,
            Err(OpenError::Symlink { component }) => {
                assert_eq!(component, "cu");
                refused_link += 1;
            }
            Err(OpenError::NotADirectory { .. }) | Err(OpenError::Io(_)) => other += 1,
            Err(e) => panic!("unexpected {e}"),
        }
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    flipper.join().unwrap();
    let outside_entries: Vec<_> = std::fs::read_dir(d.0.join("outside"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(
        outside_entries.is_empty(),
        "written OUTSIDE the root: {outside_entries:?}"
    );
    // the inside file (wherever the flipper left the dir) holds OLD or NEWi, no partial
    let cu = if root.join("tenants/cu.real").exists() {
        root.join("tenants/cu.real")
    } else {
        root.join("tenants/cu")
    };
    let text = std::fs::read_to_string(cu.join("f.nml")).unwrap();
    assert!(
        text == "OLD" || text.starts_with("NEW"),
        "partial/garbled: {text:?}"
    );
    let leftovers: Vec<_> = std::fs::read_dir(&cu)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp-"))
        .collect();
    eprintln!(
        "shape3 parent-write: iters={iters} ok={ok} refused_link={refused_link} other={other} flips={} leftovers={leftovers:?} final={text}",
        flips.load(Ordering::Relaxed)
    );
    assert!(leftovers.is_empty(), "temp leftovers: {leftovers:?}");
    proved(
        refused_link,
        "shape3 parent-write",
        flips.load(Ordering::Relaxed),
    );
}

/// Shape 4: the LEAF is flipped file<->link-to-outside during
/// `write_beneath`; the outside file must never change, and the link
/// (if it is the loser of a rename) is simply replaced — never followed.
fn leaf_flipped_to_link_during_write_never_writes_outside_for(iters: usize) {
    let d = scratch("leaf-write");
    let root = d.0.join("root");
    std::fs::create_dir_all(root.join("t")).unwrap();
    std::fs::write(root.join("t/f.nml"), "OLD").unwrap();
    let outside = d.0.join("outside.nml");
    std::fs::write(&outside, "OUTSIDE").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flips = Arc::new(AtomicUsize::new(0));
    let (r2, s2, f2, o2) = (
        root.clone(),
        Arc::clone(&stop),
        Arc::clone(&flips),
        outside.clone(),
    );
    let flipper = std::thread::spawn(move || {
        let leaf = r2.join("t/f.nml");
        let tmp = r2.join("t/f.link");
        while !s2.load(Ordering::Relaxed) {
            let _ = std::os::unix::fs::symlink(&o2, &tmp);
            let _ = std::fs::rename(&tmp, &leaf); // leaf is now a link (the file is GONE, replaced)
            let _ = std::fs::remove_file(&leaf);
            let _ = std::fs::write(&leaf, "OLD");
            f2.fetch_add(1, Ordering::Relaxed);
        }
    });
    let (mut ok, mut refused_link, mut other) = (0, 0, 0);
    for i in 0..attempts(iters) {
        if refused_link > 0 && i >= iters {
            break;
        }
        let body = format!("NEW{i}");
        match write_beneath(&root, &["t", "f.nml"], body.as_bytes()) {
            Ok(()) => ok += 1,
            Err(OpenError::Symlink { component }) => {
                assert_eq!(component, "f.nml");
                refused_link += 1;
            }
            Err(OpenError::Io(_)) | Err(OpenError::NotRegular { .. }) => other += 1,
            Err(e) => panic!("unexpected {e}"),
        }
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "OUTSIDE",
            "the outside file was written through the leaf link"
        );
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    flipper.join().unwrap();
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "OUTSIDE");
    let leftovers: Vec<_> = std::fs::read_dir(root.join("t"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp-"))
        .collect();
    eprintln!(
        "shape4 leaf-write: iters={iters} ok={ok} refused_link={refused_link} other={other} flips={} leftovers={leftovers:?}",
        flips.load(Ordering::Relaxed)
    );
    assert!(leftovers.is_empty(), "temp leftovers: {leftovers:?}");
    proved(
        refused_link,
        "shape4 leaf-write",
        flips.load(Ordering::Relaxed),
    );
}

/// Shape 5: a FIFO swapped in for the leaf during open — the open must
/// never block (O_NONBLOCK) and must refuse it as not regular.
fn leaf_flipped_to_fifo_during_open_never_blocks_for(iters: usize) {
    let d = scratch("leaf-fifo");
    let root = d.0.join("root");
    std::fs::create_dir_all(root.join("t")).unwrap();
    std::fs::write(root.join("t/f.nml"), "INSIDE").unwrap();
    let fifo = root.join("t/fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let stop = Arc::new(AtomicBool::new(false));
    let (r2, s2) = (root.clone(), Arc::clone(&stop));
    let flipper = std::thread::spawn(move || {
        let leaf = r2.join("t/f.nml");
        let fifo = r2.join("t/fifo");
        let real = r2.join("t/f.real");
        while !s2.load(Ordering::Relaxed) {
            let _ = std::fs::rename(&leaf, &real);
            let _ = std::fs::rename(&fifo, &leaf);
            let _ = std::fs::rename(&leaf, &fifo);
            let _ = std::fs::rename(&real, &leaf);
        }
    });
    let started = Instant::now();
    let (mut ok, mut not_regular, mut other) = (0, 0, 0);
    for i in 0..attempts(iters) {
        if not_regular > 0 && i >= iters {
            break;
        }
        match open_beneath(&root, &["t", "f.nml"]) {
            Ok(f) => {
                assert_eq!(read_all(f), "INSIDE");
                ok += 1
            }
            Err(OpenError::NotRegular { dir: false, .. }) => not_regular += 1,
            Err(OpenError::Io(_)) => other += 1,
            Err(e) => panic!("unexpected {e}"),
        }
        assert!(started.elapsed().as_secs() < 30, "an open blocked");
    }
    stop.store(true, Ordering::Relaxed);
    flipper.join().unwrap();
    eprintln!(
        "shape5 leaf-fifo: iters={iters} ok={ok} not_regular={not_regular} other={other} in {:?}",
        started.elapsed()
    );
    proved(not_regular, "shape5 leaf-fifo", 0);
}

/// Shape 6 (r80-sec): a racer keeps planting a HARD LINK to a victim file
/// at the fixer's temp name `.f.nml.tmp-<pid>` while `write_beneath`
/// runs. The pre-create unlink removes a planted link, but one planted
/// between that unlink and the `openat(O_CREAT | O_EXCL)` is exactly what
/// `O_EXCL` exists for: without it (mutant m12: `O_TRUNC`) the fixer's
/// bytes land in the VICTIM's inode through the link.
fn hard_link_planted_at_the_temp_name_never_redirects_the_write_for(iters: usize) {
    let d = scratch("tmp-hardlink");
    let root = d.0.join("root");
    std::fs::create_dir_all(root.join("t")).unwrap();
    std::fs::create_dir_all(root.join("victim")).unwrap();
    std::fs::write(root.join("t/f.nml"), "OLD").unwrap();
    let victim = root.join("victim/secret.nml");
    std::fs::write(&victim, "VICTIM").unwrap();
    let tmp_name = format!(".f.nml.tmp-{}", std::process::id());
    let stop = Arc::new(AtomicBool::new(false));
    let plants = Arc::new(AtomicUsize::new(0));
    let (r2, s2, p2, v2, tn) = (
        root.clone(),
        Arc::clone(&stop),
        Arc::clone(&plants),
        victim.clone(),
        tmp_name.clone(),
    );
    let flipper = std::thread::spawn(move || {
        let tmp = r2.join("t").join(&tn);
        while !s2.load(Ordering::Relaxed) {
            if std::fs::hard_link(&v2, &tmp).is_ok() {
                p2.fetch_add(1, Ordering::Relaxed);
            }
            std::thread::yield_now();
        }
    });
    let (mut ok, mut exist, mut other) = (0, 0, 0);
    // The default lane's form stops once the race has landed a plant
    // between the unlink and the create (the fact this shape pins); the
    // perf tier runs its whole count.
    let bounded = iters < ITERS;
    for i in 0..(iters * 3) {
        if bounded && exist > 0 && i >= iters {
            break;
        }
        let body = format!("NEW{i}");
        match write_beneath(&root, &["t", "f.nml"], body.as_bytes()) {
            Ok(()) => ok += 1,
            Err(OpenError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => exist += 1,
            Err(OpenError::Io(_)) => other += 1,
            Err(e) => panic!("unexpected {e}"),
        }
        let v = std::fs::read_to_string(&victim).unwrap();
        assert_eq!(
            v, "VICTIM",
            "the victim inode was written through a planted hard link (iteration {i})"
        );
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    flipper.join().unwrap();
    let _ = std::fs::remove_file(root.join("t").join(&tmp_name));
    eprintln!(
        "shape6 tmp-hardlink: iters={} ok={ok} eexist={exist} other={other} plants={}",
        iters * 3,
        plants.load(Ordering::Relaxed)
    );
    proved(exist, "shape6 tmp-hardlink", plants.load(Ordering::Relaxed));
}

// The default lane runs every shape at `FAST`; the perf tier at `ITERS`.

#[test]
fn parent_dir_flipped_to_link_during_open_never_reads_outside() {
    parent_dir_flipped_to_link_during_open_never_reads_outside_for(FAST);
}

#[test]
#[ignore = "perf tier: run with `cargo test -p nml-validate --release --test beneath_race -- --ignored perf_`"]
fn perf_parent_dir_flipped_to_link_during_open_never_reads_outside() {
    parent_dir_flipped_to_link_during_open_never_reads_outside_for(ITERS);
}

#[test]
fn leaf_flipped_to_link_during_open_never_reads_outside() {
    leaf_flipped_to_link_during_open_never_reads_outside_for(FAST);
}

#[test]
#[ignore = "perf tier: run with `cargo test -p nml-validate --release --test beneath_race -- --ignored perf_`"]
fn perf_leaf_flipped_to_link_during_open_never_reads_outside() {
    leaf_flipped_to_link_during_open_never_reads_outside_for(ITERS);
}

#[test]
fn parent_dir_flipped_to_link_during_write_never_writes_outside() {
    parent_dir_flipped_to_link_during_write_never_writes_outside_for(FAST);
}

#[test]
#[ignore = "perf tier: run with `cargo test -p nml-validate --release --test beneath_race -- --ignored perf_`"]
fn perf_parent_dir_flipped_to_link_during_write_never_writes_outside() {
    parent_dir_flipped_to_link_during_write_never_writes_outside_for(ITERS);
}

#[test]
fn leaf_flipped_to_link_during_write_never_writes_outside() {
    leaf_flipped_to_link_during_write_never_writes_outside_for(FAST);
}

#[test]
#[ignore = "perf tier: run with `cargo test -p nml-validate --release --test beneath_race -- --ignored perf_`"]
fn perf_leaf_flipped_to_link_during_write_never_writes_outside() {
    leaf_flipped_to_link_during_write_never_writes_outside_for(ITERS);
}

#[test]
fn leaf_flipped_to_fifo_during_open_never_blocks() {
    leaf_flipped_to_fifo_during_open_never_blocks_for(FAST);
}

#[test]
#[ignore = "perf tier: run with `cargo test -p nml-validate --release --test beneath_race -- --ignored perf_`"]
fn perf_leaf_flipped_to_fifo_during_open_never_blocks() {
    leaf_flipped_to_fifo_during_open_never_blocks_for(ITERS);
}

#[test]
fn hard_link_planted_at_the_temp_name_never_redirects_the_write() {
    // Three attempts per iteration; the form stops at the first raced
    // plant after `FAST` iterations, and gives up (red) at `ITERS`.
    hard_link_planted_at_the_temp_name_never_redirects_the_write_for(ITERS - 1);
}

#[test]
#[ignore = "perf tier: run with `cargo test -p nml-validate --release --test beneath_race -- --ignored perf_`"]
fn perf_hard_link_planted_at_the_temp_name_never_redirects_the_write() {
    hard_link_planted_at_the_temp_name_never_redirects_the_write_for(ITERS);
}

/// Shape 7 — shape 1 through the READER: the parent directory
/// `tenants/cu` flipped between a real directory and a link to
/// `outside/` while `read_beneath` — the one text read both front ends
/// make — hammers the file. Its `Ok` text is never the outside file's,
/// and the refusal is the chain's typed one; a reader that opened by
/// path (the editor's, once) read OUTSIDE here.
fn parent_dir_flipped_to_link_during_read_never_reads_outside_for(iters: usize) {
    let d = scratch("parent-read");
    let root = d.0.join("root");
    std::fs::create_dir_all(root.join("tenants/cu.real")).unwrap();
    std::fs::create_dir_all(d.0.join("outside")).unwrap();
    std::fs::write(root.join("tenants/cu.real/f.nml"), "INSIDE").unwrap();
    std::fs::write(d.0.join("outside/f.nml"), "OUTSIDE").unwrap();
    std::fs::rename(root.join("tenants/cu.real"), root.join("tenants/cu")).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flips = Arc::new(AtomicUsize::new(0));
    let (r2, s2, f2) = (root.clone(), Arc::clone(&stop), Arc::clone(&flips));
    let outside = d.0.join("outside");
    let flipper = std::thread::spawn(move || {
        let cu = r2.join("tenants/cu");
        let real = r2.join("tenants/cu.real");
        while !s2.load(Ordering::Relaxed) {
            let _ = std::fs::rename(&cu, &real);
            let _ = std::os::unix::fs::symlink(&outside, &cu);
            let _ = std::fs::remove_file(&cu);
            let _ = std::fs::rename(&real, &cu);
            f2.fetch_add(1, Ordering::Relaxed);
        }
    });
    let (mut ok, mut refused_link, mut other) = (0, 0, 0);
    let started = Instant::now();
    for i in 0..attempts(iters) {
        if refused_link > 0 && i >= iters {
            break;
        }
        match read_beneath(&root, &["tenants", "cu", "f.nml"], 1024, "a test input") {
            Ok(s) => {
                assert_eq!(s, "INSIDE", "read OUTSIDE bytes through a flipped parent");
                ok += 1;
            }
            Err(ReadError::Open(OpenError::Symlink { component })) => {
                assert_eq!(component, "cu");
                refused_link += 1;
            }
            Err(ReadError::Open(OpenError::NotADirectory { .. }))
            | Err(ReadError::Open(OpenError::Io(_))) => other += 1,
            Err(e) => panic!("unexpected {e}"),
        }
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    flipper.join().unwrap();
    eprintln!(
        "shape7 parent-read: iters={iters} ok={ok} refused_link={refused_link} other={other} flips={} in {:?}",
        flips.load(Ordering::Relaxed),
        started.elapsed()
    );
    proved(
        refused_link,
        "shape7 parent-read",
        flips.load(Ordering::Relaxed),
    );
}

#[test]
fn parent_dir_flipped_to_link_during_read_never_reads_outside() {
    parent_dir_flipped_to_link_during_read_never_reads_outside_for(FAST);
}

#[test]
#[ignore = "perf tier: run with `cargo test -p nml-validate --release --test beneath_race -- --ignored perf_`"]
fn perf_parent_dir_flipped_to_link_during_read_never_reads_outside() {
    parent_dir_flipped_to_link_during_read_never_reads_outside_for(ITERS);
}
