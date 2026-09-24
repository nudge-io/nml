//! The composition golden — RFC 0025 Phase 5 as amended by RFC 0019
//! item 0 round 88: the two-binary oracle's observable, pinned in-tree.
//!
//! Every battery composition (`compose_with`, the funnel every test
//! composes through — so the corpus is the live battery and cannot rot)
//! and every layer fixture (`tests/fixtures/layers/**`) is composed
//! through [`compose_file`] — the one orchestration the CLI runs — and
//! its OBSERVABLE is compared with `compose.golden` beside this file:
//! for each composing declaration its composed body and its provenance
//! table, and the rendered diagnostics in the sink's order — the
//! Phase-1 dump's shape, unchanged. One block per composition:
//!
//! ```text
//! <name> <input hash> <output hash>
//!   ! <rendered diagnostic>
//! ```
//!
//! `name` is the test's path with its composition index
//! (`layers::tests::items::foo#0`) or `fixture:<path under
//! tests/fixtures/layers>`. The input hash is blake3 over the composed
//! text, so a changed input reads as one and is never mistaken for
//! drift. The output hash is blake3 over the compact JSON of the
//! observable — exact over bodies and origins, which no golden could
//! show reviewably (the five generated stacks dump to 22 MB) — and the
//! diagnostics ride beside it, so an intended change is READ in the
//! golden's diff, not only counted. The oracle's allow-list exempted a
//! case whole; a golden line is exact for every case, always.
//!
//! Runs on every `cargo test`: a composition with no line, a changed
//! input or a drifted output fails the test that composed it, naming the
//! case. `NML_UPDATE_GOLDEN=1 cargo test -p nml-core --lib layers`
//! re-records the compositions that ran (replacing their lines and
//! dropping a test's lines past its count, keeping every other); review the diff — that IS the change, and the golden's
//! history is the changelog of intended differences. `NML_COMPOSE_DUMP=
//! <dir>` writes every observable (and its input) under `<dir>`, so two
//! commits compare with `diff -r` — the two-binary oracle's capability
//! with no tagged binary, no harvest and no hidden verb. A line whose
//! composition no longer exists is stale: the liveness test names it;
//! delete it.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use super::index_from;
use crate::ast::DeclarationKind;
use crate::layers::{LayerGrantProvider, OpenContext, compose_file};

const GOLDEN_FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/layers/tests/compose.golden"
);
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/layers");
const UPDATE: &str = "NML_UPDATE_GOLDEN=1 cargo test -p nml-core --lib layers";
const HEADER: &str = "# The composition golden (crates/nml-core/src/layers/tests/golden.rs): \
                      `<name> <input blake3> <output blake3>`, then `  ! <diagnostic>` per \
                      rendered finding.\n\
                      # Regenerate with NML_UPDATE_GOLDEN=1 cargo test -p nml-core --lib \
                      layers; review the diff — that IS the change.\n";

/// One composition's recorded observable.
#[derive(Clone, PartialEq, Eq)]
struct Entry {
    input: String,
    output: String,
    diagnostics: Vec<String>,
}

struct Golden {
    entries: BTreeMap<String, Entry>,
    update: bool,
}

fn golden() -> &'static Mutex<Golden> {
    static GOLDEN: OnceLock<Mutex<Golden>> = OnceLock::new();
    GOLDEN.get_or_init(|| {
        let update = std::env::var_os("NML_UPDATE_GOLDEN").is_some();
        let entries = match std::fs::read_to_string(GOLDEN_FILE) {
            Ok(text) => parse(&text),
            Err(_) if update => BTreeMap::new(),
            Err(e) => panic!("compose golden unreadable ({e}) — {UPDATE} creates it"),
        };
        Mutex::new(Golden { entries, update })
    })
}

fn hash(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

fn parse(text: &str) -> BTreeMap<String, Entry> {
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    let mut current: Option<String> = None;
    for (i, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(diagnostic) = line.strip_prefix("  ! ") {
            let name = current.as_ref().unwrap_or_else(|| {
                panic!("compose.golden:{}: a diagnostic before any entry", i + 1)
            });
            entries
                .get_mut(name)
                .expect("the current entry exists")
                .diagnostics
                .push(diagnostic.to_string());
            continue;
        }
        let mut words = line.split(' ');
        let (Some(name), Some(input), Some(output), None) =
            (words.next(), words.next(), words.next(), words.next())
        else {
            panic!(
                "compose.golden:{}: not `<name> <input> <output>`: {line}",
                i + 1
            );
        };
        entries.insert(
            name.to_string(),
            Entry {
                input: input.to_string(),
                output: output.to_string(),
                diagnostics: Vec::new(),
            },
        );
        current = Some(name.to_string());
    }
    entries
}

fn render(entries: &BTreeMap<String, Entry>) -> String {
    let mut text = String::from(HEADER);
    for (name, e) in entries {
        text.push_str(&format!("{name} {} {}\n", e.input, e.output));
        for d in &e.diagnostics {
            text.push_str(&format!("  ! {d}\n"));
        }
    }
    text
}

/// The battery's hook: one composition of the funnel, keyed by the
/// test's thread name and a per-thread composition index (libtest names
/// every test's thread after the test). The text composed is the
/// schema and the source as ONE file, exactly as `nml check` composes a
/// single file; `main.nml` is its name, as the funnel's is. The
/// composition runs on its OWN thread: the engine's test seams
/// (`JUDGMENT_MISSES`, `FOLD_LOG`, `FOLD_TAMPER`, `DISCARDS`) are
/// thread-local, so the golden never sees a test's armed tamper and a
/// test's counters never see the golden's composition — exactly the
/// fresh process the CLI oracle composed in.
pub(super) fn observe_battery(schema: &str, src: &str, grants: &(dyn LayerGrantProvider + Sync)) {
    thread_local! {
        static CALLS: Cell<u32> = const { Cell::new(0) };
    }
    let n = CALLS.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    });
    let thread = std::thread::current();
    let test = thread
        .name()
        .filter(|name| *name != "main")
        .expect("the composition golden keys on the test's thread name (libtest names it)");
    let name = format!("{test}#{n}");
    let text = format!("{schema}\n{src}");
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name(format!("{name} (golden)"))
            .spawn_scoped(s, || observe(&name, "main.nml", &text, grants))
            .expect("spawn the golden's thread")
            .join()
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
    });
}

/// Compose `text` as the file `source_path` under `grants`, and check
/// (or record) its observable under `name`.
fn observe(name: &str, source_path: &str, text: &str, grants: &dyn LayerGrantProvider) {
    // The parse's findings over the source — the repeats a merge battery
    // states on purpose (NML2093) — ride the golden ahead of the
    // composition's, stamped as a front end stamps them, so the golden
    // reads exactly what a consumer of this text sees.
    let (file, parse_findings) = crate::cst::parse_to_ast_all(text);
    let index = index_from(text);
    let composed = compose_file(&index, source_path, &file, grants);
    let file_ref = composed.validation_file.as_ref();
    let mut declarations = Vec::new();
    for (idx, origins) in &composed.origins {
        let block = file_ref
            .and_then(|f| f.declarations.get(*idx))
            .and_then(|d| match &d.kind {
                DeclarationKind::Block(b) => Some(b),
                _ => None,
            });
        declarations.push(serde_json::json!({
            "declaration": block.map(|b| b.name.name.clone()).unwrap_or_default(),
            "body": block.map(|b| &b.body),
            "origins": origins,
        }));
    }
    let diagnostics: Vec<String> = parse_findings
        .iter()
        .map(|d| d.clone().with_source(source_path.to_string()).to_string())
        .chain(composed.diagnostics.iter().map(ToString::to_string))
        .collect();
    let dump = serde_json::json!({
        "declarations": declarations,
        "diagnostics": diagnostics,
    });
    let observed = Entry {
        input: hash(text.as_bytes()),
        output: hash(&serde_json::to_vec(&dump).expect("the observable serializes")),
        diagnostics,
    };
    if let Some(dir) = std::env::var_os("NML_COMPOSE_DUMP") {
        let dir = Path::new(&dir);
        std::fs::create_dir_all(dir).expect("NML_COMPOSE_DUMP directory");
        let stem: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        std::fs::write(
            dir.join(format!("{stem}.json")),
            serde_json::to_string_pretty(&dump).expect("the observable serializes"),
        )
        .expect("write the dump");
        std::fs::write(dir.join(format!("{stem}.nml")), text).expect("write the input");
    }
    let mut g = golden().lock().unwrap_or_else(|e| e.into_inner());
    if g.update {
        g.entries.insert(name.to_string(), observed);
        // A test that composes fewer times than it did leaves `<test>#k`
        // lines above its count — live by the liveness test's lights (the
        // function exists), stale in fact: dropped as the test re-records.
        if let Some((test, index)) = name.rsplit_once('#') {
            if let Ok(index) = index.parse::<u32>() {
                let prefix = format!("{test}#");
                g.entries.retain(|key, _| {
                    key.strip_prefix(&prefix)
                        .and_then(|i| i.parse::<u32>().ok())
                        .is_none_or(|i| i <= index)
                });
            }
        }
        std::fs::write(GOLDEN_FILE, render(&g.entries)).expect("write compose.golden");
        return;
    }
    match g.entries.get(name) {
        Some(want) if *want == observed => {}
        Some(want) => {
            let list = |ds: &[String]| -> String {
                if ds.is_empty() {
                    "    (none)".to_string()
                } else {
                    ds.iter()
                        .map(|d| format!("    ! {d}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            };
            panic!(
                "compose golden: `{name}` {} — {UPDATE} accepts it; review the diff\n  \
                 recorded diagnostics:\n{}\n  now:\n{}\n  \
                 (bodies and origins: NML_COMPOSE_DUMP=<dir> at both commits, then diff -r)",
                if want.input != observed.input {
                    "composes a CHANGED input"
                } else {
                    "DRIFTED"
                },
                list(&want.diagnostics),
                list(&observed.diagnostics),
            );
        }
        None => {
            panic!("compose golden: no line for `{name}` — {UPDATE} records it; review the diff")
        }
    }
}

/// The committed layer fixtures — the oracle's second corpus — composed
/// as `nml check` composes each single file, under the open context.
/// The hand-written fixtures run here, on every `cargo test`; the five
/// generated stacks under `perf/` (their dumps are 22 MB) ride the
/// `perf_` tier beside their timing gates (`--release -- --ignored
/// perf_`), where CI runs them explicitly.
#[test]
fn layer_fixtures_match_the_composition_golden() {
    observe_fixtures(|rel| !rel.starts_with("perf/"));
}

#[test]
#[ignore = "the generated stacks' golden — run with --release -- --ignored perf_"]
fn perf_layer_stacks_match_the_composition_golden() {
    observe_fixtures(|rel| rel.starts_with("perf/"));
}

fn observe_fixtures(selected: impl Fn(&str) -> bool) {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let path = entry.expect("a fixture entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|x| x == "nml") {
                out.push(path);
            }
        }
    }
    let mut paths = Vec::new();
    walk(Path::new(FIXTURES), &mut paths);
    paths.sort();
    let mut seen = 0;
    for path in paths {
        let rel: Vec<String> = path
            .strip_prefix(FIXTURES)
            .expect("under the fixtures root")
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let rel = rel.join("/");
        if !selected(&rel) {
            continue;
        }
        seen += 1;
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel}: {e}"));
        observe(&format!("fixture:{rel}"), &rel, &text, &OpenContext);
    }
    assert!(seen > 0, "no selected layer fixtures under {FIXTURES}");
}

/// Every golden line names a composition that still exists: a fixture
/// on disk, or a test function in the battery file its path names. A
/// renamed or deleted test leaves a stale line; this names it.
#[test]
fn every_composition_golden_line_is_live() {
    let Ok(text) = std::fs::read_to_string(GOLDEN_FILE) else {
        assert!(
            std::env::var_os("NML_UPDATE_GOLDEN").is_some(),
            "compose.golden is missing — {UPDATE} creates it"
        );
        return;
    };
    let sources: &[(&str, &str)] = &[
        ("", include_str!("mod.rs")),
        ("grants", include_str!("grants.rs")),
        ("items", include_str!("items.rs")),
        ("linearize", include_str!("linearize.rs")),
        ("merge", include_str!("merge.rs")),
        ("normalize", include_str!("normalize.rs")),
        ("oneof", include_str!("oneof.rs")),
        ("order", include_str!("order.rs")),
        ("perf", include_str!("perf.rs")),
        ("policy", include_str!("policy.rs")),
        ("seal", include_str!("seal.rs")),
        ("union", include_str!("union.rs")),
    ];
    let live = |name: &str| -> bool {
        if let Some(rel) = name.strip_prefix("fixture:") {
            return Path::new(FIXTURES).join(rel).is_file();
        }
        let Some((path, _index)) = name.rsplit_once('#') else {
            return false;
        };
        let Some(rest) = path.strip_prefix("layers::tests::") else {
            return false;
        };
        let (file, func) = match rest.split_once("::") {
            Some((file, tail)) => (file, tail.rsplit("::").next().unwrap_or(tail)),
            None => ("", rest),
        };
        sources
            .iter()
            .any(|(f, text)| *f == file && text.contains(&format!("fn {func}(")))
    };
    let entries = parse(&text);
    let stale: Vec<&String> = entries.keys().filter(|name| !live(name)).collect();
    assert!(
        stale.is_empty(),
        "stale composition golden lines (their tests are gone — delete the lines):\n  {}",
        stale
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// The funnel still OBSERVES. `every_composition_golden_line_is_live`
/// proves each recorded name still has a test; nothing proved that the
/// test still reaches the observer — and the battery is 96 % of the
/// record (281 of 293 entries here; the rest are the fixture corpus,
/// which `observe_fixtures` drives directly). MEASURED: deleting the
/// one `golden::observe_battery(...)` call from `compose_with` leaves
/// the whole `nml-core` suite green, so every battery entry becomes a
/// line nothing compares. This is the ratchet for that line: the
/// funnel's body must call the observer, on a line that is not a
/// comment.
#[test]
fn the_funnel_observes_every_battery_composition() {
    const FUNNEL: &str = include_str!("mod.rs");
    let body = FUNNEL
        .split_once("fn compose_with(")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_once("\n}\n"))
        .map(|(body, _)| body)
        .expect("the battery's composition funnel is `fn compose_with` in mod.rs");
    let observes = body
        .lines()
        .map(str::trim)
        .any(|line| !line.starts_with("//") && line.contains("golden::observe_battery("));
    assert!(
        observes,
        "`compose_with` no longer calls `golden::observe_battery`: every battery line in \
         compose.golden is then compared with nothing, and the whole suite stays green"
    );
}

/// The observer keys on the test's thread name (libtest names every
/// test thread): from an UNNAMED thread it refuses to record — fail
/// closed, never a golden line under a made-up name (a wasm run, or a
/// helper thread, would otherwise mint one).
#[test]
fn the_observer_refuses_an_unnamed_thread() {
    let outcome = std::thread::spawn(|| observe_battery("", "", &OpenContext)).join();
    let payload = outcome.expect_err("an unnamed thread must be refused");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(message.contains("thread name"), "{message}");
}
