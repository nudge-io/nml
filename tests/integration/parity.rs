//! The DIFFERENTIAL PARITY HARNESS: the CLI and the editor, over ONE
//! fixture corpus, asked about the same file, held to one verdict.
//!
//! Every round of this project has found the same shape of defect: the
//! editor answering a question the CLI answers differently, silently —
//! a folder the kernel could not anchor read as "no schema" (r106 F2),
//! a listing whose unreadable entry vanished so a universe read as OPEN
//! (r103 F1), a bound file whose validator failed "falling back to basic
//! validation" where `nml check` refuses. Each was found by hand, one at
//! a time, by someone who thought to look. A ratchet over the SOURCE
//! cannot find them: the question is not how a function is spelled, it
//! is what two programs SAY about one file.
//!
//! So: one corpus, both front ends, one mapping.
//!
//! * the CLI's outcome class is `(exit code, `governing`, `closure`,
//!   `universe`, the GOVERNING BINDING's identity, the set of row
//!   codes)` from `nml binding --json`;
//! * the editor's is `(Resolution, the universe's state, the binding's
//!   identity, the set of note codes)` from `PackageResolver::resolve`
//!   — the real resolver the language server calls, in process, over the
//!   same tree;
//! * the ORACLE column is the kernel over the wasm editor's backend
//!   ([`WasiFs`]: `lstat` through `std`, listings through a shim, no
//!   realpath), which the native resolver compiles OUT (`PackageResolver
//!   ::disk` is `#[cfg(target_os = "wasi")]`) — so until this column
//!   existed the bundled WASM server's filesystem semantics were judged
//!   by nothing this gate runs;
//! * [`expected_editor`] is the MAPPING, written once, and
//!   [`ACCEPTED_DIVERGENCES`] is the only way a row may differ — with a
//!   reason, per fixture. Silence is not available.
//!
//! What is NOT compared, deliberately: spans and message TEXT. Both
//! front ends render the kernel's own `Diagnostic`, and the transcript
//! goldens (`tests/fixtures/workspace/expected/`) pin the CLI's
//! rendering; a row here is about which VERDICT each program reaches.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use nml_lsp::packages::{OpenDocuments, PackageResolver, Resolution, WorkspaceView};
use nml_validate::package::builtin_meta_package;
use nml_validate::workspace::{
    ExternalClaim, ExternalClass, Governing, InputKind, PathFs, StdFs, WorkspaceRoot, discover,
    read_input, resolve_file, wasi_fs_through,
};

// ---------------------------------------------------------------------
// The two outcome classes, and the mapping between them.
// ---------------------------------------------------------------------

/// What the CLI says about one file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CliOutcome {
    exit: i32,
    governing: String,
    closure: String,
    universe: String,
    absent: bool,
    /// The governing binding's identity as the wire spells it —
    /// `<package>::<binding> <class> <step> <contentHash>`; `None` when
    /// nothing binds. Comparing the WORD "bound" proves only that both
    /// front ends bound something: the claim-resolution order (pins
    /// first, the class ladder, first match in declaration order) is a
    /// kernel decision each front end reaches for itself, and nothing
    /// asked them for the same answer.
    binding: Option<String>,
    /// The `layers` object as the wire prints it — the composition
    /// GRANT, which `compose_file` is judged under. Both front ends
    /// carry the kernel's own `Grant`; only the CLI's half was ever
    /// read here. Compared as a VALUE: the two writers order an
    /// object's keys differently and that is not a divergence.
    layers: serde_json::Value,
    codes: Vec<String>,
}

/// What the editor says about the same file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorOutcome {
    resolution: &'static str,
    /// [`UniverseState`] as the wire spells it (`open`/`closed`), the
    /// `binding` row's own word. `None` when no universe was built.
    universe: Option<String>,
    binding: Option<String>,
    layers: serde_json::Value,
    codes: Vec<String>,
}

/// One verdict of the KERNEL under one filesystem oracle — the column
/// that asks whether the wasm editor's backend judges the tree the way
/// the native one does.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OracleOutcome {
    governing: String,
    universe: String,
    closure: String,
    /// The KEY the kernel minted — the name every finding carries
    /// (`Diagnostic.source`), so a backend that keys a file differently
    /// publishes its rows under a different name.
    key: String,
    /// What minting learned about symlinks on the way there.
    via_symlink: String,
    binding: Option<String>,
    codes: Vec<String>,
}

/// The MAPPING, stated once: the editor verdict the CLI's answer
/// requires. `nml binding` exits 1 for a file nothing governs and 0 for
/// a bound one; the editor has no exit code, so the CLI's `governing`
/// (plus a refusal, which the CLI spells as an NML20xx row on the file)
/// is what carries over.
fn expected_editor(cli: &CliOutcome) -> &'static str {
    // No `binding` row at all: the kernel refused the invocation before
    // a universe existed (a root it will not derive — exit 2). The
    // editor has no exit code, and the document validates under
    // nothing, which is `Refused`.
    if cli.governing == "norow" {
        return "Refused";
    }
    // A universe the CLI refuses to judge under — a truncated walk
    // (NML2089), an input that failed to load (NML2088), an
    // unbuildable validator (NML2091), a path no key carries (NML2083),
    // an ambiguous claim (NML2087) — is the editor's `Refused`.
    const REFUSING: &[&str] = &["NML2083", "NML2087", "NML2088", "NML2089", "NML2091"];
    if cli.codes.iter().any(|c| REFUSING.contains(&c.as_str())) {
        return "Refused";
    }
    match cli.governing.as_str() {
        "bound" => "Bound",
        "ambiguous" => "Refused",
        _ => "Unbound",
    }
}

/// The divergences this project has DECIDED to accept, each with the
/// reason it is not a defect. A fixture named here is compared on
/// everything except the named axis; anything else that diverges is a
/// finding, reported by name. Adding a row is a review decision.
const ACCEPTED_DIVERGENCES: &[(&str, Axis, &str)] = &[
    (
        "open-universe-through-a-link",
        Axis::Oracle,
        "THE ORACLE GAP, measured here: under an OPEN universe the walk FOLLOWS a symlinked \
     directory component, and following it needs realpath. The native backend has one and \
     keys the file at the link's TARGET (`real/a.nml`); the wasm editor's backend has none \
     (`FsError::NoRealpath`) and keeps the authored spelling (`link/a.nml`). The key is the \
     name every finding carries and the name every grant glob is matched against, so in a \
     workspace folder holding a symlinked subdirectory the bundled WASM server can allow \
     or deny a reference the CLI judges the other way, and publishes its rows under \
     another name. Both still call the file unbound here, which is why nothing had \
     noticed. It is an OWNER DECISION, not a settled one: either the wasm host supplies a \
     realpath (a shim the editor already has for listings), or the kernel keys an \
     unverifiable link lexically on BOTH backends and says so. Silence is the one option \
     this row refuses",
    ),
    (
        "store-package",
        Axis::Layers,
        "THE STRUCTURAL GAP, on the axis with teeth: `nml check` sees no universe here and \
     PERMITS composition (the open developer context, `granted: true`); the editor binds \
     the store package, whose binding carries no `layers:` block, and DENIES it \
     (NML2064). So a file that composes in CI is refused in the editor — the same one \
     decision as the rows below, and the reason the decision is not cosmetic",
    ),
    (
        "store-package",
        Axis::Binding,
        "THE STRUCTURAL GAP, on the axis that names it: the editor binds `demo` from the store \
     and the CLI names no binding at all. Same cause and same decision as the row below",
    ),
    (
        "store-package",
        Axis::Resolution,
        "THE STRUCTURAL GAP, measured here: a package in the per-user schema-package store, \
     pinned by a project config, BINDS in the editor and does not exist for the CLI — \
     `nml-cli` names `nml_validate::store` nowhere, so `nml check` reports the same file \
     as ungoverned and exits 1 while the editor validates it under the store package's \
     model. A developer's editor says the file is fine; CI says nothing governs it. Kept \
     here as the one divergence this corpus finds, and it is an OWNER DECISION, not a \
     settled one: either the CLI reads the store too (one verdict, one gate), or the \
     editor SAYS on a store-bound file that the gate will not see this binding. Silence \
     is the one option this row refuses",
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    /// The verdict class itself (`Resolution` vs `governing`).
    Resolution,
    /// The set of codes each side reports.
    Codes,
    /// Whether the universe DECIDES (`open`/`closed`) — the word whose
    /// two remedies differ.
    Universe,
    /// WHICH binding governs, not merely that one does.
    Binding,
    /// The composition grant each front end carries.
    Layers,
    /// The kernel under the wasm editor's oracle vs under the native one.
    Oracle,
}

fn accepted(fixture: &str, axis: Axis) -> Option<&'static str> {
    ACCEPTED_DIVERGENCES
        .iter()
        .find(|(f, a, _)| *f == fixture && *a == axis)
        .map(|(_, _, why)| *why)
}

// ---------------------------------------------------------------------
// The two front ends.
// ---------------------------------------------------------------------

fn nml_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_nml"))
}

fn cli_outcome(root: &Path, file: &Path, store: Option<&Path>, derived: bool) -> CliOutcome {
    let mut cmd = nml_bin();
    cmd.args(["binding", file.to_str().expect("utf-8"), "--json"]);
    // A DERIVED fixture asks the CLI the question the editor asks for a
    // document outside every folder: fix the universe yourself.
    if !derived {
        cmd.args(["--root", root.to_str().expect("utf-8")]);
    }
    // Both front ends read the SAME store (`NML_SCHEMA_STORE_DIR`, the
    // kernel's one override): a corpus where they read different ones
    // would prove nothing about either.
    match store {
        Some(dir) => cmd.env("NML_SCHEMA_STORE_DIR", dir),
        None => cmd.env_remove("NML_SCHEMA_STORE_DIR"),
    };
    let out = cmd.output().expect("nml runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let rows: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l}: {e}")))
        .collect();
    // A run the kernel REFUSES before a universe exists (a root it will
    // not derive) prints no `binding` row at all: that refusal is an
    // outcome class of its own, not a harness failure.
    let refused = serde_json::json!({});
    let binding = rows
        .iter()
        .find(|r| r["type"] == "binding")
        .unwrap_or(&refused);
    let mut codes: Vec<String> = rows
        .iter()
        .flat_map(|r| {
            r["notes"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(std::iter::once(r))
                .filter_map(|n| n["code"].as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .collect();
    codes.sort();
    codes.dedup();
    let governing_binding = binding["binding"].as_object().map(|b| {
        let field = |k: &str| b.get(k).and_then(|v| v.as_str()).unwrap_or("?");
        format!(
            "{}::{} {} {} {}",
            field("package"),
            field("name"),
            field("class"),
            field("step"),
            field("contentHash"),
        )
    });
    CliOutcome {
        exit: out.status.code().unwrap_or(-1),
        governing: binding["governing"].as_str().unwrap_or("norow").to_string(),
        closure: binding["closure"].as_str().unwrap_or("norow").to_string(),
        universe: binding["universe"].as_str().unwrap_or("norow").to_string(),
        absent: binding["absent"].as_bool().unwrap_or(false),
        binding: governing_binding,
        layers: binding["layers"].clone(),
        codes,
    }
}

/// A document store with nothing open: the parity corpus lives on disk,
/// so the editor reads exactly what the CLI reads. (An OPEN BUFFER is
/// the editor's own axis — a file the CLI cannot see — and belongs to a
/// separate corpus, not to a parity one.)
struct NoBuffers;

impl OpenDocuments for NoBuffers {
    fn text(&self, _path: &Path) -> Option<String> {
        None
    }
    fn stamp(&self, _path: &Path) -> Option<u64> {
        None
    }
}

fn editor_outcome(root: &Path, file: &Path, store: Option<&Path>, derived: bool) -> EditorOutcome {
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let resolver = PackageResolver::new(
        store.map(|dir| nml_validate::store::Store::at(dir.to_path_buf())),
        tx,
    );
    let roots = if derived {
        Vec::new()
    } else {
        vec![root.to_path_buf()]
    };
    let buffers: Vec<PathBuf> = Vec::new();
    let documents = NoBuffers;
    let view = WorkspaceView {
        roots: &roots,
        buffers: &buffers,
        documents: &documents,
    };
    let resolved = resolver.resolve(file, &view);
    let mut codes: Vec<String> = resolved
        .notes
        .iter()
        .filter_map(|n| n.code.map(|c| c.to_string()))
        .collect();
    codes.sort();
    codes.dedup();
    // The wire's spelling of the binding, from the editor's own
    // `Binding`: the same five facts the `binding` row prints, in the
    // kernel's own vocabulary (`ClaimClass::tag`, `BindingStep::tag`),
    // so a difference is a difference in the ANSWER, never in the words.
    let binding = match &resolved.resolution {
        Resolution::Bound(b) => Some(format!(
            "{}::{} {} {} {}",
            b.package_name,
            b.binding_name,
            b.class.tag(),
            b.step.tag(),
            b.content_hash,
        )),
        Resolution::Unbound | Resolution::Refused => None,
    };
    EditorOutcome {
        resolution: match resolved.resolution {
            Resolution::Bound(_) => "Bound",
            Resolution::Unbound => "Unbound",
            Resolution::Refused => "Refused",
        },
        universe: resolved.universe.map(|u| u.label().to_string()),
        binding,
        layers: serde_json::to_value(resolved.grant.wire()).expect("the grant serializes"),
        codes,
    }
}

// ---------------------------------------------------------------------
// The third judge: the kernel under one oracle.
// ---------------------------------------------------------------------

/// The kernel's verdict for one file under `fs` — the same four calls
/// both front ends make (`WorkspaceRoot` → `discover` → `universe` →
/// `resolve_file`), with the CLI's own external claim (the builtin meta
/// package) so the universe is the one a run sees.
fn kernel_outcome(root: &Path, file: &Path, fs: &dyn PathFs, derived: bool) -> OracleOutcome {
    let root = if derived {
        match WorkspaceRoot::derive(file, fs) {
            Ok(root) => root,
            // The kernel refuses to derive: an outcome class of its own,
            // and one both oracles must reach together.
            Err(e) => {
                return OracleOutcome {
                    governing: format!("noroot({e})"),
                    universe: "none".to_string(),
                    closure: "none".to_string(),
                    key: String::new(),
                    via_symlink: String::new(),
                    binding: None,
                    codes: Vec::new(),
                };
            }
        }
    } else {
        WorkspaceRoot::explicit(root, fs).expect("the scratch root anchors")
    };
    let builtin = ExternalClaim::new(Arc::new(builtin_meta_package()), ExternalClass::Builtin);
    let read = |kind: InputKind, path: &Path| read_input(&root, kind, path);
    let discovery = discover(&root, fs, &read, vec![builtin], Arc::default());
    let universe = discovery.universe();
    let (governing, binding, mut codes, key, via) = match resolve_file(&universe, file, fs) {
        Ok(resolved) => {
            let (governing, binding) = match &resolved.governing {
                Governing::Bound { claimant, step } => (
                    "bound",
                    Some(format!(
                        "{}::{} {} {} {}",
                        claimant.claim.name(),
                        claimant.binding.name,
                        claimant.claim.class().tag(),
                        step.tag(),
                        claimant.claim.content_hash(),
                    )),
                ),
                Governing::Ambiguous(_) => ("ambiguous", None),
                Governing::Unbound => ("unbound", None),
            };
            let codes = resolved
                .findings
                .iter()
                .filter_map(|d| d.code.map(|c| c.to_string()))
                .collect::<Vec<_>>();
            (
                governing.to_string(),
                binding,
                codes,
                resolved.key.to_string(),
                format!("{:?}", resolved.via_symlink),
            )
        }
        // A path no key can carry: the kernel's typed refusal, which
        // both front ends render as the file's one row.
        Err(e) => (
            format!("rejected({e})"),
            None,
            Vec::new(),
            String::new(),
            String::new(),
        ),
    };
    codes.sort();
    codes.dedup();
    OracleOutcome {
        governing,
        universe: universe.state().label().to_string(),
        closure: universe.closure.tag().to_string(),
        key,
        via_symlink: via,
        binding,
        codes,
    }
}

// ---------------------------------------------------------------------
// The corpus.
// ---------------------------------------------------------------------

struct Fixture {
    name: &'static str,
    /// Lay the tree out under a fresh scratch root and name the target;
    /// `store` is the per-user schema-package store BOTH front ends
    /// read, when the fixture needs one.
    build: fn(&Path) -> (PathBuf, Option<PathBuf>),
    /// The universe is DERIVED, not given: the CLI runs with no
    /// `--root` and the editor sees no workspace folder, so both take
    /// R1's third rung — the kernel's fenced derivation (`.git`, the
    /// shadow rule, the walk bound). Every fixture was rooted before
    /// this flag, so the rung the two front ends reach for a file
    /// outside every folder was compared by nothing.
    derived: bool,
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

const MANIFEST: &str = "\
package demo:
    version = \"0.1.0\"
    formatVersion = 1

[]schema schemas:
    - core:
        file = \"core.model.nml\"

[]validator validators:
    - core:
        files:
            - \"a.nml\"
        schemas:
            - core
        strict = true
";
/// The same manifest with a `layers:` GRANT on its one binding — the
/// composition verdict both front ends carry as the kernel's `Grant`.
const MANIFEST_GRANTED: &str = "\
package demo:
    version = \"0.1.0\"
    formatVersion = 1

[]schema schemas:
    - core:
        file = \"core.model.nml\"

[]validator validators:
    - core:
        files:
            - \"a.nml\"
        schemas:
            - core
        strict = true
        layers:
            allowRefs:
                - \"lib/**\"
            denyRefs:
                - \"lib/secret/**\"
            maxStackDepth = 4
";
const CORE: &str = "model core:\n    v string\n";
const INSTANCE: &str = "core a:\n    v = \"x\"\n";

fn corpus() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "bound",
            build: |root| {
                write(&root.join("demo.package.nml"), MANIFEST);
                write(&root.join("core.model.nml"), CORE);
                write(&root.join("a.nml"), INSTANCE);
                (root.join("a.nml"), None)
            },
            derived: false,
        },
        Fixture {
            name: "unbound-open",
            build: |root| {
                write(&root.join("demo.package.nml"), MANIFEST);
                write(&root.join("core.model.nml"), CORE);
                write(&root.join("a.nml"), INSTANCE);
                write(&root.join("other.nml"), INSTANCE);
                (root.join("other.nml"), None)
            },
            derived: false,
        },
        Fixture {
            name: "no-manifest",
            build: |root| {
                write(&root.join("a.nml"), INSTANCE);
                (root.join("a.nml"), None)
            },
            derived: false,
        },
        Fixture {
            name: "ambiguous-claim",
            build: |root| {
                write(&root.join("demo.package.nml"), MANIFEST);
                write(&root.join("core.model.nml"), CORE);
                write(
                    &root.join("second.package.nml"),
                    &MANIFEST.replace("package demo:", "package second:"),
                );
                write(&root.join("a.nml"), INSTANCE);
                (root.join("a.nml"), None)
            },
            derived: false,
        },
        Fixture {
            name: "unloadable-manifest",
            build: |root| {
                write(
                    &root.join("demo.package.nml"),
                    "package demo:\n    ( not nml\n",
                );
                write(&root.join("a.nml"), INSTANCE);
                (root.join("a.nml"), None)
            },
            derived: false,
        },
        Fixture {
            name: "declared-source-missing",
            build: |root| {
                write(&root.join("demo.package.nml"), MANIFEST);
                write(&root.join("a.nml"), INSTANCE);
                (root.join("a.nml"), None)
            },
            derived: false,
        },
        Fixture {
            name: "absent-target",
            build: |root| {
                write(&root.join("demo.package.nml"), MANIFEST);
                write(&root.join("core.model.nml"), CORE);
                (root.join("a.nml"), None)
            },
            derived: false,
        },
        Fixture {
            name: "symlinked-file",
            build: |root| {
                write(&root.join("demo.package.nml"), MANIFEST);
                write(&root.join("core.model.nml"), CORE);
                write(&root.join("real.nml"), INSTANCE);
                #[cfg(unix)]
                std::os::unix::fs::symlink("real.nml", root.join("a.nml")).expect("symlink");
                (root.join("a.nml"), None)
            },
            derived: false,
        },
        Fixture {
            name: "unreadable-dir",
            build: |root| {
                write(&root.join("demo.package.nml"), MANIFEST);
                write(&root.join("core.model.nml"), CORE);
                write(&root.join("a.nml"), INSTANCE);
                let shut = root.join("shut");
                std::fs::create_dir_all(&shut).expect("mkdir");
                write(&shut.join("hidden.nml"), INSTANCE);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000))
                        .expect("chmod");
                }
                (root.join("a.nml"), None)
            },
            derived: false,
        },
        Fixture {
            name: "fifo-named-as-nml",
            build: |root| {
                write(&root.join("demo.package.nml"), MANIFEST);
                write(&root.join("core.model.nml"), CORE);
                write(&root.join("a.nml"), INSTANCE);
                #[cfg(unix)]
                {
                    let pipe = root.join("pipe.nml");
                    let made = Command::new("mkfifo").arg(&pipe).status();
                    // A `mkfifo` that is missing or refused leaves a
                    // fixture byte-identical to `bound`: the row would
                    // still be green and would prove nothing about a
                    // non-regular entry. Say so instead.
                    assert!(
                        made.as_ref().is_ok_and(|s| s.success()) && pipe.exists(),
                        "the fifo fixture needs a real FIFO at {} (mkfifo: {made:?})",
                        pipe.display()
                    );
                }
                (root.join("a.nml"), None)
            },
            derived: false,
        },
        Fixture {
            name: "closed-unclaimed",
            build: |root| {
                write(&root.join("demo.package.nml"), MANIFEST);
                write(&root.join("core.model.nml"), CORE);
                write(
                    &root.join("nml-project.nml"),
                    "project demo:\n    formatVersion = 1\n",
                );
                write(&root.join("a.nml"), INSTANCE);
                write(&root.join("other.nml"), INSTANCE);
                (root.join("other.nml"), None)
            },
            derived: false,
        },
        // ── hostile spellings: the NAME vocabulary meets the filesystem ──
        //
        // The kernel's name rules are BYTE-EXACT and the filesystem it
        // opens through need not be. Both fixtures are a claimed-looking
        // file that is NOT claimed, and both front ends must say the same
        // thing about it — the property this harness exists for. They are
        // here because "the gate judged nothing and said nothing" is the
        // one outcome a repository can choose, and a change to either
        // front end's classification has to be a deliberate one.
        Fixture {
            name: "case-variant-directory",
            build: |root| {
                // The manifest claims `tenants/**`; the directory on disk
                // is `TENANTS`. On a case-INSENSITIVE filesystem (APFS,
                // NTFS) the operating system serves `tenants/cu/a.nml` to
                // anything that asks for it by that name, and the key the
                // walk mints is the on-disk spelling, which the glob does
                // not match: the file is judged under no binding, with no
                // skip row and no error.
                write(
                    &root.join("demo.package.nml"),
                    &MANIFEST.replace("\"a.nml\"", "\"tenants/**/*.nml\""),
                );
                write(&root.join("core.model.nml"), CORE);
                write(&root.join("TENANTS/cu/a.nml"), INSTANCE);
                (root.join("TENANTS/cu/a.nml"), None)
            },
            derived: false,
        },
        Fixture {
            name: "nfd-directory",
            build: |root| {
                // The same shape without a case-insensitive filesystem:
                // the manifest claims the NFC spelling `tenants/café` and
                // the directory on disk is the NFD one. The two render
                // IDENTICALLY in every terminal and editor, so the
                // divergence is invisible to the operator — which is why
                // both front ends have to answer it the same way.
                write(
                    &root.join("demo.package.nml"),
                    &MANIFEST.replace("\"a.nml\"", "\"tenants/caf\u{e9}/*.nml\""),
                );
                write(&root.join("core.model.nml"), CORE);
                write(&root.join("tenants/cafe\u{301}/a.nml"), INSTANCE);
                (root.join("tenants/cafe\u{301}/a.nml"), None)
            },
            derived: false,
        },
        Fixture {
            name: "manifest-past-its-bound",
            build: |root| {
                // One byte past the 256 KiB a manifest is read under: the
                // universe has an input that failed to load, which is a
                // refusal on both sides, not a quietly open universe.
                let pad = "\n// ".to_string() + &"p".repeat(256 * 1024);
                write(&root.join("demo.package.nml"), &format!("{MANIFEST}{pad}"));
                write(&root.join("core.model.nml"), CORE);
                write(&root.join("a.nml"), INSTANCE);
                (root.join("a.nml"), None)
            },
            derived: false,
        },
        // The per-user STORE channel: a package published there, bound
        // by AUTO-ASSOCIATION at its marker root — the one claim class
        // that reaches the editor through a channel the CLI's wire has
        // no row for. If the two still agree, there is no store parity
        // gap left to accept, and the allow-list says so by failing.
        Fixture {
            name: "store-package",
            build: |root| {
                // OUTSIDE the workspace root: a store inside it would be
                // walked as workspace content, and the slot's own
                // manifest would claim as a LIVE one.
                let store = root.with_file_name("store-package.store");
                std::fs::create_dir_all(&store).expect("mkdir");
                nml_validate::test_support::publish_demo(&nml_validate::store::Store::at(
                    store.clone(),
                ));
                // The project config PINS the store package by name
                // (R5's first rung), so the file binds through the
                // store channel on both sides.
                write(
                    &root.join("proj/nml-project.nml"),
                    "project p:\n    schemaPackages:\n        - demo\n",
                );
                write(&root.join("proj/demo.nml"), "core d:\n    name = \"d\"\n");
                (root.join("proj/demo.nml"), Some(store))
            },
            derived: false,
        },
        // A manifest GRANT: the composition verdict `compose_file` runs
        // under. Both front ends carry the kernel's own `Grant`, and
        // until this fixture the corpus held no grant at all — the
        // `layers` column compared two "no grant"s on every row.
        Fixture {
            name: "manifest-grant",
            build: |root| {
                write(&root.join("demo.package.nml"), MANIFEST_GRANTED);
                write(&root.join("core.model.nml"), CORE);
                write(&root.join("a.nml"), INSTANCE);
                (root.join("a.nml"), None)
            },
            derived: false,
        },
        // The one shape on which the two ORACLES can disagree: a
        // symlinked DIRECTORY component under an OPEN universe, where
        // the walk follows links. The native backend has realpath and
        // re-keys the file at the link's target; the wasm backend has
        // none (`FsError::NoRealpath`) and keeps the authored spelling,
        // marked unverifiable. Without this fixture the oracle column
        // would compare two backends on trees that cannot tell them
        // apart.
        Fixture {
            name: "open-universe-through-a-link",
            build: |root| {
                write(&root.join("real/a.nml"), INSTANCE);
                #[cfg(unix)]
                std::os::unix::fs::symlink("real", root.join("link")).expect("symlink");
                (root.join("link/a.nml"), None)
            },
            derived: false,
        },
        // R1's THIRD rung: no `--root`, no workspace folder — the
        // kernel's fenced derivation, on both sides. A `.git` DIRECTORY
        // fences the walk at the fixture root, whose manifest is then
        // the outermost marker within the fence.
        Fixture {
            name: "derived-git-fence",
            build: |root| {
                std::fs::create_dir_all(root.join(".git")).expect("mkdir .git");
                write(&root.join("demo.package.nml"), MANIFEST);
                write(&root.join("core.model.nml"), CORE);
                write(&root.join("a.nml"), INSTANCE);
                (root.join("a.nml"), None)
            },
            derived: true,
        },
        // The hostile shape of the same rung: a root marker ABOVE a
        // `.git` entry that is NO directory — what git writes for a
        // submodule or a linked worktree, or what a tenant plants. The
        // kernel REFUSES to derive; neither front end may quietly
        // narrow the universe to the inner directory.
        Fixture {
            name: "derived-marker-above-a-git-file",
            build: |root| {
                write(&root.join("outer/demo.package.nml"), MANIFEST);
                write(&root.join("outer/core.model.nml"), CORE);
                write(&root.join("outer/inner/.git"), "gitdir: ../elsewhere\n");
                write(&root.join("outer/inner/a.nml"), INSTANCE);
                (root.join("outer/inner/a.nml"), None)
            },
            derived: true,
        },
    ]
}

/// Undo anything the fixtures made unreadable, so the scratch tree can
/// be removed whatever the verdict.
fn reopen(root: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let shut = root.join("shut");
        if shut.exists() {
            let _ = std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755));
        }
    }
}

#[test]
fn the_cli_and_the_editor_agree_about_every_file_in_the_corpus() {
    let base =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("parity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("mkdir");
    let mut findings: Vec<String> = Vec::new();
    let mut rows: Vec<String> = Vec::new();
    let mut used: Vec<(&str, Axis)> = Vec::new();
    // What the corpus REACHED. A differential harness agrees trivially
    // on a corpus that stopped producing an outcome — every fixture
    // could collapse to "unbound, closed" and every row would still
    // match. Measured: making the kernel stop REFUSING a shadowed
    // derivation leaves this test green, because both front ends move
    // together. So the corpus states which outcome classes it must
    // still reach, and says so when one goes.
    let mut classes: Vec<String> = Vec::new();

    for fixture in corpus() {
        let root = base.join(fixture.name);
        std::fs::create_dir_all(&root).expect("mkdir");
        let root = std::fs::canonicalize(&root).expect("canonical");
        let (target, store) = (fixture.build)(&root);
        let cli = cli_outcome(&root, &target, store.as_deref(), fixture.derived);
        let editor = editor_outcome(&root, &target, store.as_deref(), fixture.derived);
        // The ORACLE column: the kernel over the native backend and over
        // the wasm editor's, which the native resolver compiles out.
        // A store-pinned fixture has no oracle row — the kernel's own
        // call takes no store (that channel is the front ends').
        let oracles = store.is_none().then(|| {
            let native = kernel_outcome(&root, &target, &StdFs, fixture.derived);
            let wasi = kernel_outcome(
                &root,
                &target,
                &wasi_fs_through(|dir: &Path| std::fs::read_dir(dir)),
                fixture.derived,
            );
            (native, wasi)
        });
        reopen(&root);
        let want = expected_editor(&cli);
        rows.push(format!(
            "{:<24} CLI exit={} governing={} closure={} universe={} binding={:?} codes={:?}  \
             |  EDITOR {} universe={:?} binding={:?} codes={:?}  |  ORACLE {}",
            fixture.name,
            cli.exit,
            cli.governing,
            cli.closure,
            cli.universe,
            cli.binding,
            cli.codes,
            editor.resolution,
            editor.universe,
            editor.binding,
            editor.codes,
            match &oracles {
                Some((native, wasi)) if native == wasi => format!("wasi==native {native:?}"),
                Some((native, wasi)) => format!("native {native:?} WASI {wasi:?}"),
                None => "(store fixture: not asked)".to_string(),
            }
        ));
        classes.push(format!("governing={}", cli.governing));
        classes.push(format!(
            "universe={}",
            editor.universe.as_deref().unwrap_or("none")
        ));
        classes.push(format!("resolution={}", editor.resolution));
        if editor.resolution != want {
            match accepted(fixture.name, Axis::Resolution) {
                Some(_) => used.push((fixture.name, Axis::Resolution)),
                None => findings.push(format!(
                    "{}: the CLI says governing={} (exit {}, codes {:?}) — the editor must say \
                     {want}, and says {}",
                    fixture.name, cli.governing, cli.exit, cli.codes, editor.resolution
                )),
            }
        }
        if cli.codes != editor.codes {
            match accepted(fixture.name, Axis::Codes) {
                Some(_) => used.push((fixture.name, Axis::Codes)),
                None => findings.push(format!(
                    "{}: the two front ends report different codes — CLI {:?}, editor {:?}",
                    fixture.name, cli.codes, editor.codes
                )),
            }
        }
        // The universe's own word: the two UNBOUND states have different
        // remedies, and the status bar once gave the OPEN one for both.
        // `None` is "no universe at all", which a rooted fixture never is.
        // A run with no `binding` row named no universe: the editor must
        // have none either (`None` = no universe was built), which is
        // the same fact in the editor's vocabulary.
        let want_universe = (cli.governing != "norow").then_some(cli.universe.as_str());
        if editor.universe.as_deref() != want_universe {
            match accepted(fixture.name, Axis::Universe) {
                Some(_) => used.push((fixture.name, Axis::Universe)),
                None => findings.push(format!(
                    "{}: the universe must be {want_universe:?} and the editor says {:?}",
                    fixture.name, editor.universe
                )),
            }
        }
        // The composition GRANT: `nml check` and the editor must deny or
        // permit composition identically (NML2064/NML2065).
        // A run with no `binding` row carries no grant to compare: the
        // CLI refused the invocation, so there is no verdict of its own
        // to hold the editor's against.
        if cli.governing != "norow" && cli.layers != editor.layers {
            match accepted(fixture.name, Axis::Layers) {
                Some(_) => used.push((fixture.name, Axis::Layers)),
                None => findings.push(format!(
                    "{}: the two front ends carry different grants — CLI {}, editor {}",
                    fixture.name, cli.layers, editor.layers
                )),
            }
        }
        // WHICH binding, not merely that there is one.
        if cli.binding != editor.binding {
            match accepted(fixture.name, Axis::Binding) {
                Some(_) => used.push((fixture.name, Axis::Binding)),
                None => findings.push(format!(
                    "{}: the two front ends name different bindings — CLI {:?}, editor {:?}",
                    fixture.name, cli.binding, editor.binding
                )),
            }
        }
        if let Some((native, wasi)) = &oracles {
            if native != wasi {
                match accepted(fixture.name, Axis::Oracle) {
                    Some(_) => used.push((fixture.name, Axis::Oracle)),
                    None => findings.push(format!(
                        "{}: the wasm editor's oracle judges this tree differently — native \
                         {native:?}, wasi {wasi:?}",
                        fixture.name
                    )),
                }
            }
        }
    }

    let table = rows.join("\n");
    if std::env::var_os("NML_PARITY_TABLE").is_some() {
        println!("{table}");
    }
    let _ = std::fs::remove_dir_all(&base);
    assert!(
        findings.is_empty(),
        "the CLI and the editor disagree. Each row is a DEFECT in one of them, or a \
         divergence this project decides to accept — which is a row in \
         ACCEPTED_DIVERGENCES with its reason, never a silence:\n{}\n\nthe whole corpus:\n{table}",
        findings.join("\n")
    );
    // Every outcome class the corpus exists to exercise. A row that
    // stops happening is a corpus that stopped asking.
    let reached: std::collections::BTreeSet<&str> = classes.iter().map(String::as_str).collect();
    let required = [
        // The four verdicts `nml binding` can reach, the last being a
        // run refused before a universe existed (R1's derived rung).
        "governing=bound",
        "governing=unbound",
        "governing=ambiguous",
        "governing=norow",
        // Both universes, whose remedies differ, and no universe at all.
        "universe=open",
        "universe=closed",
        "universe=none",
        // The editor's three.
        "resolution=Bound",
        "resolution=Unbound",
        "resolution=Refused",
    ];
    let unreached: Vec<&str> = required
        .iter()
        .copied()
        .filter(|c| !reached.contains(c))
        .collect();
    assert!(
        unreached.is_empty(),
        "the corpus no longer reaches {unreached:?} — a differential harness agrees for \
         free on a corpus that stopped producing an outcome. Restore the fixture that \
         reached it, or drop the requirement with the reason.\n\nthe whole corpus:\n{table}"
    );
    for (fixture, axis, why) in ACCEPTED_DIVERGENCES {
        assert!(
            used.contains(&(fixture, *axis)),
            "the accepted divergence ({fixture}, {axis:?}) no longer happens — drop the row \
             ({why})"
        );
    }
}
