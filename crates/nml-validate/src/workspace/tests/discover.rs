//! Discovery pins (step 0c): inertness by strict ancestry (R3′),
//! manifest-derived anchors (R4), loud truncation (A16), and the checked
//! file's own resolution (P4 → NML2083).

use super::*;
use nml_core::diagnostic::codes;

use crate::fs::{EntryKind, FsError};
use crate::workspace::discover::{
    MAX_ENTRIES, MAX_LIVE_INPUT_BYTES, MAX_TOTAL_ENTRIES, MAX_TOTAL_LIVE_INPUT_BYTES,
};
use crate::workspace::mock::Probe;

const TENANTS: &[(&str, &[&str])] = &[("tenantFlows", &["tenants/**/*.flow.nml"])];

/// The layout under which a tenant's inputs are LIVE inside its unit:
/// `tenants/*/flows/*.flow.nml` makes every `tenants/<x>` a unit (the
/// last run of wildcard directory segments starts at `*`) and reaches
/// `tenants/<x>/flows` only, so `tenants/<x>/other/` is unclaimed — a
/// manifest or config there is live, and charged to the tenant's unit.
const FLOWS_ONLY: &[(&str, &[&str])] = &[
    ("tenantFlows", &["tenants/*/flows/*.flow.nml"]),
    ("ops", &["admin/ops.flow.nml"]),
];

/// The operator's glob with a LITERAL AFTER the wildcard — E38's honest
/// residual: under the last-run rule the unit is `tenants/<x>/flows/<y>`,
/// so `tenants/<x>` itself and `tenants/<x>/other/` are the ROOT unit's
/// (reached content, no unit root above it).
const FLOWS_DEEP: &[(&str, &[&str])] = &[
    ("tenantFlows", &["tenants/*/flows/**"]),
    ("ops", &["admin/ops.flow.nml"]),
];

/// A tenant whose manifest sits in the ROOT unit (a gap of `FLOWS_DEEP`)
/// and claims a glob with a literal between two wildcard runs — the
/// r76 F1 shape: its `other/z<i>/deep/w` would be unit roots, and the
/// manifests it plants in `…/w/side/s<j>/` are live (never reached).
fn minting_tenant(ws: Ws) -> Ws {
    ws.manifest(
        "tenants/cu/other/own.package.nml",
        "own",
        &[],
        &[("mint", &["*/deep/*/*.nml"])],
    )
}

/// `n` empty files under `dir`.
fn spam(mut ws: Ws, dir: &str, n: usize) -> Ws {
    for i in 0..n {
        ws.fs = ws.fs.file(&format!("/ws/{dir}/f{i}"));
    }
    ws
}

/// Every directory the walk LISTED, in order (the mock's probe log).
fn listed(ws: &Ws) -> Vec<String> {
    ws.fs
        .probes()
        .into_iter()
        .filter_map(|p| match p {
            Probe::ListDir(d) => Some(d.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// The checked file's kernel findings, as `resolve_file` derives them.
fn findings_of(u: &Universe<'_>, fs: &MockFs, path: &str) -> Vec<Diagnostic> {
    crate::workspace::discover::resolve_file(u, Path::new(path), fs)
        .expect("keyed")
        .findings
}

#[test]
fn inertness_depends_only_on_strictly_shallower_inputs() {
    // A manifest at depth 2 claiming `**` under the root anchor cannot
    // inert the depth-1 operator manifest, nor a depth-2 sibling, nor a
    // depth-1 config: only a STRICTLY SHALLOWER manifest's reach counts.
    let ws = Ws::new()
        .config("nml-project.nml", "")
        .manifest(
            "apps/demo.package.nml",
            "demo",
            &[],
            &[("apps", &["apps/**/*.nml"])],
        )
        .manifest("apps/x/evil.package.nml", "evil", &[], &[("all", &["**"])])
        .manifest(
            "apps/y/other.package.nml",
            "other",
            &[],
            &[("y", &["apps/y/**"])],
        )
        .config("apps/y/nml-project.nml", "    autoAssociate = false\n")
        .file("apps/y/z.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    // The depth-1 `apps/demo.package.nml` reaches `apps/x` and `apps/y`
    // — so the two nested MANIFESTS and the nested config are inert,
    // whatever `evil` claims.
    let mut inert = inert_keys(&d);
    inert.sort();
    assert_eq!(
        inert,
        [
            "apps/x/evil.package.nml",
            "apps/y/nml-project.nml",
            "apps/y/other.package.nml"
        ]
    );
    assert_eq!(manifest_keys(&d), ["apps/demo.package.nml"]);
    assert_eq!(d.configs.len(), 1);
    assert_eq!(d.configs[0].path.as_str(), "nml-project.nml");
    for diag in &d.inert {
        assert_eq!(diag.code, Some(codes::INERT_RESOLUTION_INPUT));
        assert!(
            diag.message
                .contains("binding 'apps' of apps/demo.package.nml"),
            "{}",
            diag.message
        );
        assert!(diag.message.contains("files[0]"), "{}", diag.message);
    }
    // Reach never points upward or sideways: `evil` (live here — the
    // operator's manifest reaches only `apps/y/**`) cannot inert
    // `apps/demo.package.nml` beside-and-above it, nor `apps/y/**`.
    let ws = Ws::new()
        .config("nml-project.nml", "")
        .manifest(
            "apps/demo.package.nml",
            "demo",
            &[],
            &[("apps", &["apps/y/**/*.nml"])],
        )
        .manifest("apps/x/evil.package.nml", "evil", &[], &[("all", &["**"])])
        .config("apps/y/nml-project.nml", "    autoAssociate = false\n")
        .file("apps/x/own.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(inert_keys(&d), ["apps/y/nml-project.nml"]);
    assert_eq!(
        manifest_keys(&d),
        ["apps/demo.package.nml", "apps/x/evil.package.nml"]
    );
    assert_eq!(
        bound(&governing(&d.universe(), &key("apps/x/own.nml"))).0,
        "evil"
    );
}

#[test]
fn tenant_project_config_in_claimed_content_is_inert() {
    // Rule 2: `autoAssociate = false` in a tenant-committed
    // `nml-project.nml` under the operator's `tenants/**` binding is
    // inert — the tenant's file still binds to the operator's binding.
    // Both spellings of the binding: `tenants/**` (matches the config
    // itself) and the RFC's own `tenants/**/*.flow.nml`, which does NOT
    // match the config yet REACHES its directory — claimed content is a
    // subtree, not a file list.
    for glob in ["tenants/**", "tenants/**/*.flow.nml"] {
        let ws = Ws::new()
            .manifest("demo.package.nml", "demo", &[], &[("tenantFlows", &[glob])])
            .config("tenants/cu/nml-project.nml", "    autoAssociate = false\n")
            .file("tenants/cu/member-lookup.flow.nml");
        let root = ws.root();
        let d = ws.discover(&root, vec![]);
        assert_eq!(inert_keys(&d), ["tenants/cu/nml-project.nml"], "{glob}");
        assert!(d.configs.is_empty());
        let u = d.universe();
        let (pkg, binding, idx, anchor) =
            bound(&governing(&u, &key("tenants/cu/member-lookup.flow.nml")));
        assert_eq!(
            (pkg.as_str(), binding.as_str(), idx, anchor.as_str()),
            ("demo", "tenantFlows", 0, "")
        );
        assert!(
            d.inert[0]
                .message
                .contains("project config `tenants/cu/nml-project.nml` is inert"),
            "{}",
            d.inert[0].message
        );
        assert!(d.inert[0].message.contains(&format!("files[0] = {glob:?}")));
    }
    // A config beside the manifest (same directory) is never inerted by
    // it — the operator's own config stays live.
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], &[("all", &["**"])])
        .config("nml-project.nml", "    autoAssociate = false\n")
        .file("x.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(d.inert.is_empty());
    assert_eq!(d.configs.len(), 1);
    assert!(matches!(
        governing(&d.universe(), &key("x.nml")),
        Governing::Unbound
    ));
}

#[test]
fn tenant_marker_in_claimed_content_is_inert() {
    // The r48 walk: root config, root marker `app.nml`, a STORE package
    // `demo` (`rootMarkers: ["app.nml"]`, bindings tenantFlows then all).
    // The tenant commits `tenants/cu/app.nml`: nested under the live root
    // marker of the same package ⇒ inert ⇒ the anchor stays the root ⇒
    // the key is `tenants/cu/…` ⇒ `tenantFlows` first-matches. A live
    // tenant marker would have re-rooted the glob base to `tenants/cu`
    // and handed the file to `all`.
    let ws = Ws::new()
        .config("nml-project.nml", "")
        .file("app.nml")
        .file("tenants/cu/app.nml")
        .file("tenants/cu/member-lookup.flow.nml");
    let root = ws.root();
    let store = external(
        "demo",
        &["app.nml"],
        &[
            ("tenantFlows", &["tenants/**/*.flow.nml"]),
            ("all", &["**/*.flow.nml"]),
        ],
        ExternalClass::Store,
    );
    let d = ws.discover(&root, vec![store]);
    assert_eq!(inert_keys(&d), ["tenants/cu/app.nml"]);
    assert!(
        d.inert[0]
            .message
            .contains("nested under the live `app.nml` marker")
    );
    assert!(
        matches!(&d.claims[0].origin, ClaimOrigin::External { markers, .. } if markers.len() == 1 && markers[0].as_str().is_empty())
    );
    let u = d.universe();
    let (_, binding, _, anchor) = bound(&governing(&u, &key("tenants/cu/member-lookup.flow.nml")));
    assert_eq!((binding.as_str(), anchor.as_str()), ("tenantFlows", ""));
    // Without the root marker, the tenant marker IS the anchor (and
    // `all` binds): the nesting clause, not glob luck, is what protects.
    let ws = Ws::new()
        .config("nml-project.nml", "")
        .file("tenants/cu/app.nml")
        .file("tenants/cu/member-lookup.flow.nml");
    let root = ws.root();
    let store = external(
        "demo",
        &["app.nml"],
        &[
            ("tenantFlows", &["tenants/**/*.flow.nml"]),
            ("all", &["**/*.flow.nml"]),
        ],
        ExternalClass::Store,
    );
    let d = ws.discover(&root, vec![store]);
    let u = d.universe();
    let (_, binding, _, anchor) = bound(&governing(&u, &key("tenants/cu/member-lookup.flow.nml")));
    assert_eq!((binding.as_str(), anchor.as_str()), ("all", "tenants/cu"));
}

#[test]
fn marker_anchor_is_manifest_derived_not_content_derived() {
    // R4: a workspace manifest at `apps/core/` with `rootMarkers:
    // ["app.nml"]` anchors at the nearest live config/own-marker dir at
    // or ABOVE its own directory (the root config here); a content-side
    // `app.nml` deeper than the manifest never moves the anchor — under
    // content-derived anchoring the tenant file's relative path would be
    // `x.flow.nml` and the binding would miss.
    let ws = Ws::new()
        .config("nml-project.nml", "")
        .manifest(
            "apps/core/core.package.nml",
            "core",
            &["app.nml"],
            &[("tenants", &["apps/core/tenants/**/*.flow.nml"])],
        )
        .file("apps/core/tenants/cu/app.nml")
        .file("apps/core/tenants/cu/x.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(
        matches!(&d.claims[0].origin, ClaimOrigin::Workspace { anchor, .. } if anchor.as_str().is_empty())
    );
    let u = d.universe();
    let (_, binding, _, anchor) = bound(&governing(&u, &key("apps/core/tenants/cu/x.flow.nml")));
    assert_eq!((binding.as_str(), anchor.as_str()), ("tenants", ""));
    // With no config above, the manifest's OWN-marker directory at or
    // above dir(M) anchors it — `apps/app.nml` here — never the content
    // marker below.
    let ws = Ws::new()
        .file("apps/app.nml")
        .manifest(
            "apps/core/core.package.nml",
            "core",
            &["app.nml"],
            &[("tenants", &["core/tenants/**/*.flow.nml"])],
        )
        .file("apps/core/tenants/cu/app.nml")
        .file("apps/core/tenants/cu/x.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(
        matches!(&d.claims[0].origin, ClaimOrigin::Workspace { anchor, .. } if anchor.as_str() == "apps")
    );
    let u = d.universe();
    assert_eq!(
        bound(&governing(&u, &key("apps/core/tenants/cu/x.flow.nml"))).3,
        "apps"
    );
    // And with neither: dir(M) itself.
    let ws = Ws::new()
        .manifest(
            "apps/core/core.package.nml",
            "core",
            &["app.nml"],
            &[("tenants", &["tenants/**/*.flow.nml"])],
        )
        .file("apps/core/tenants/cu/x.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(
        matches!(&d.claims[0].origin, ClaimOrigin::Workspace { anchor, .. } if anchor.as_str() == "apps/core")
    );
}

#[test]
fn sibling_app_markers_anchor_independently() {
    // A store package with `rootMarkers: ["app.nml"]`: two sibling apps
    // each carry a live marker; each anchors its own files.
    let ws = Ws::new()
        .file("apps/a/app.nml")
        .file("apps/a/flows/x.flow.nml")
        .file("apps/b/app.nml")
        .file("apps/b/flows/y.flow.nml");
    let root = ws.root();
    let store = external(
        "demo",
        &["app.nml"],
        &[("flows", &["flows/*.flow.nml"])],
        ExternalClass::Store,
    );
    let d = ws.discover(&root, vec![store]);
    assert!(d.inert.is_empty());
    assert!(
        matches!(&d.claims[0].origin, ClaimOrigin::External { markers, .. } if markers.len() == 2)
    );
    let u = d.universe();
    assert_eq!(
        bound(&governing(&u, &key("apps/a/flows/x.flow.nml"))).3,
        "apps/a"
    );
    assert_eq!(
        bound(&governing(&u, &key("apps/b/flows/y.flow.nml"))).3,
        "apps/b"
    );
    assert!(matches!(
        governing(&u, &key("apps/a/app.nml")),
        Governing::Unbound
    ));
}

#[test]
fn nested_manifest_in_claimed_content_is_inert_2080() {
    let ws = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("tenants", &["tenants/**"])],
        )
        .manifest(
            "tenants/cu/mine.package.nml",
            "mine",
            &[],
            &[("all", &["**"])],
        )
        .file("tenants/cu/x.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(inert_keys(&d), ["tenants/cu/mine.package.nml"]);
    assert_eq!(manifest_keys(&d), ["demo.package.nml"]);
    assert!(
        d.inert[0]
            .message
            .starts_with("package manifest `tenants/cu/mine.package.nml` is inert")
    );
    let u = d.universe();
    assert_eq!(bound(&governing(&u, &key("tenants/cu/x.nml"))).0, "demo");
}

/// `Discovery::nml_files_under` is THE enumeration every front end
/// expands a directory to: `.nml` regular files the walk saw, by depth
/// then key — never a dot-file, a policy-skipped subtree, a symlink or a
/// non-`.nml` file — under the root key or under a directory key. The
/// universe's own inputs are files among them: the root's
/// `nml-project.nml` and a live manifest are enumerated like any
/// `.nml` file (the editor's index reads them through this, with no
/// pre-read of its own).
#[test]
fn nml_files_under_is_the_one_enumeration() {
    let ws = Ws::new()
        .config("nml-project.nml", "")
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("all", &["**/*.flow.nml"])],
        )
        .file("x.nml")
        .file(".hidden.nml")
        .file("notes.txt")
        .file("sub/y.nml")
        .file("sub/.also.nml")
        .file("sub/deep/z.nml")
        .file("node_modules/n.nml")
        .file("target/t.nml")
        .file(".git/g.nml")
        .symlink("link.nml", "x.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let under = |dir: &str| -> Vec<String> {
        d.nml_files_under(&key(dir))
            .map(|k| k.as_str().to_string())
            .collect()
    };
    assert_eq!(
        under(""),
        [
            "core.model.nml",
            "demo.package.nml",
            "nml-project.nml",
            "x.nml",
            "sub/y.nml",
            "sub/deep/z.nml"
        ]
    );
    assert_eq!(under("sub"), ["sub/y.nml", "sub/deep/z.nml"]);
    assert_eq!(under("sub/deep"), ["sub/deep/z.nml"]);
    assert!(under("node_modules").is_empty());
}

#[test]
fn truncated_discovery_is_closed_denied() {
    // A16: a walk cut short (an unreadable directory) yields a closed
    // universe that binds NOTHING — never "no claims" — and the
    // truncation is an ERROR naming the directory where the walk
    // stopped (E28: a warning left validation switched off with exit 0).
    // r75: `locked` sits in the ROOT unit — `tenants/**` reaches no
    // top-level directory but `tenants` — so its refusal is the whole
    // universe; under a `**` claim every top-level directory is a unit
    // and an unlistable one is denied alone
    // (`an_unlistable_directory_inside_a_unit_denies_the_unit_alone`).
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("x.nml")
        .file("tenants/cu/x.flow.nml")
        .denied("locked");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated,
        Some(Truncation::Unreadable {
            dir: key("locked"),
            error: FsError::Denied
        })
    );
    let u = d.universe();
    assert!(u.is_closed());
    // Denied in FULL: the file the binding would claim binds nothing.
    assert!(matches!(
        governing(&u, &key("tenants/cu/x.flow.nml")),
        Governing::Unbound
    ));
    assert!(matches!(governing(&u, &key("x.nml")), Governing::Unbound));
    let error = d.truncation_error().expect("loud");
    assert_eq!(error.severity, nml_core::diagnostic::Severity::Error);
    assert_eq!(error.source.as_deref(), Some("locked"));
    // r69b: NML2089 is minted HERE (the A7 table), not stamped by a
    // front end — the editor and the CLI print one code.
    assert_eq!(error.code, Some(codes::UNIVERSE_TRUNCATED));
    assert_eq!(u.closure, Closure::Truncated);
    assert_eq!(shown(&d.universe_errors()), [error.to_string()]);
    assert_eq!(shown(&d.universe_notes()), [error.to_string()]);
    assert!(d.inert_notes_for(&key("x.nml")).is_empty());
    assert!(d.files.is_empty(), "denied in full: no listing survives");
    assert!(
        error
            .message
            .contains("cannot enumerate manifests: the walk stopped at `locked` (unreadable: permission denied on a path component)"),
        "{}",
        error.message
    );
    // E35: Layer B states the fact; the "pass --root" advice is the CLI
    // reporter's (the editor has no flag to offer).
    assert!(!error.message.contains("--root"), "{}", error.message);
    assert!(
        error
            .message
            .ends_with("no binding governs any file; remove what stopped the walk"),
        "{}",
        error.message
    );
    // Denied in full: nothing is loaded — the manifest beside the root
    // is not a claim and not a load error.
    assert!(d.claims.is_empty());
    assert!(d.load_errors.is_empty());
    assert!(d.truncated_units.is_empty());
    // r73 DELTA. The walk is now interleaved with the settlement (the
    // budget units of A16's amendment are a function of the live claims
    // above them, so they must be known while the walk runs), so a
    // whole-universe truncation can follow reads of manifests STRICTLY
    // SHALLOWER than the stop directory. The `Discovery` is still empty
    // — nothing binds, nothing is enumerated — and E28(4) is untouched:
    // what was read was settled LIVE first, under the same byte caps.
    // `locked` is at depth 1, so the depth-1 manifest was read before
    // the round that failed to list it.
    assert_eq!(
        ws.read_keys(),
        ["demo.package.nml", "core.model.nml"],
        "only live inputs strictly above the stop directory"
    );
    // Not truncated: policy skips are not truncation.
    let ws = Ws::new()
        .file("node_modules/evil.package.nml")
        .file(".hidden/evil.package.nml")
        .file("target/evil.package.nml")
        .file("x.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    assert!(d.claims.is_empty());
    assert!(!d.universe().is_closed());
    assert_eq!(d.universe().closure, Closure::Complete);
    assert_eq!(
        files(&d),
        ["x.nml"],
        "policy-skipped subtrees are not listed"
    );
}

#[test]
fn deep_directory_chain_is_skipped_never_truncated() {
    // E28 (1): a tenant-committed 66-deep chain under a strict binding
    // is an EXACT skip — nothing at depth 64 is keyable and no manifest
    // below it can govern a keyable key (R5′) — so the operator's binding
    // keeps governing the tenant's file. Pre-fix this was a truncation:
    // the universe closed, the file resolved Unbound, and validation was
    // switched off with a warning.
    let deep: String = (0..66).map(|i| format!("d{i}/")).collect();
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/bad.flow.nml")
        .file(&format!("tenants/cu/{deep}x.nml"))
        .file(&format!("tenants/cu/{deep}evil.package.nml"));
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    assert!(d.truncation_error().is_none());
    assert_eq!(manifest_keys(&d), ["demo.package.nml"]);
    let u = d.universe();
    assert_eq!(
        bound(&governing(&u, &key("tenants/cu/bad.flow.nml"))).1,
        "tenantFlows"
    );
    // The last keyable depth is still walked: a manifest at key depth 64
    // is seen (inert here, inside claimed content).
    let sixty_three: String = (0..63).map(|i| format!("e{i}/")).collect();
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], &[("all", &["**"])])
        .file(&format!("{sixty_three}deep.package.nml"));
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    assert_eq!(inert_keys(&d), [format!("{sixty_three}deep.package.nml")]);
}

#[test]
fn a_directory_at_the_component_bound_is_never_listed() {
    use crate::workspace::Skip;
    // r80-cov (mutant K4 `<` → `<=` survived): the depth clause is an
    // EXACT skip at the bound — a directory at depth 64 is never listed,
    // so no key deeper than 64 components can enter `files`, while a
    // directory at depth 63 still is (its files are 64-component keys).
    let sixty_three: String = (0..63).map(|i| format!("e{i}/")).collect();
    let sixty_four: String = (0..64).map(|i| format!("f{i}/")).collect();
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], &[("all", &["**"])])
        .file(&format!("{sixty_three}edge.nml"))
        .file(&format!("{sixty_four}below.nml"));
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    let listed = files(&d);
    assert!(
        listed.contains(&format!("{sixty_three}edge.nml")),
        "depth 64 is keyable: {listed:?}"
    );
    assert!(
        !listed.iter().any(|f| f.ends_with("below.nml")),
        "a depth-64 directory is never listed: {listed:?}"
    );
    // …and never SILENTLY: the never-listed directory is a row at its own
    // key (the last keyable depth), so a gate reports what lies beneath.
    let bound: Vec<&SourceKey> = d
        .skipped
        .iter()
        .filter(|s| s.why == Skip::ComponentBound)
        .map(|s| &s.key)
        .collect();
    assert_eq!(
        bound,
        [&key(sixty_four.trim_end_matches('/'))],
        "the never-listed directory is a row: {:?}",
        d.skipped
    );
    assert!(
        listed
            .iter()
            .all(|f| f.split('/').count() <= crate::workspace::paths::MAX_COMPONENTS),
        "{listed:?}"
    );
}

#[test]
fn entry_bound_truncation_names_the_stop_directory() {
    // E28 (1): past the entry bound the walk stops, the universe is
    // denied in full, and the error names the directory being listed.
    //
    // r73, A16 amendment: this is the ROOT UNIT — the spam sits in
    // `admin/`, content NO claiming glob reaches, so it is the
    // operator's own and its exhaustion is still the whole universe.
    // (The same fixture under `tenants/**` is now the unit case:
    // `unit_budget_exhaustion_closes_only_that_unit`.)
    let mut ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/plain.flow.nml");
    for i in 0..=crate::workspace::discover::MAX_ENTRIES {
        ws.fs = ws.fs.file(&format!("/ws/admin/spam/f{i}"));
    }
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated,
        Some(Truncation::Entries {
            dir: key("admin/spam")
        })
    );
    assert!(d.truncated_units.is_empty(), "the root unit is not a unit");
    let error = d.truncation_error().expect("loud");
    assert_eq!(error.severity, nml_core::diagnostic::Severity::Error);
    assert_eq!(error.source.as_deref(), Some("admin/spam"));
    assert!(
        error
            .message
            .contains("the walk stopped at `admin/spam` (the 65536-entry bound was reached)"),
        "{}",
        error.message
    );
    assert!(d.claims.is_empty() && d.load_errors.is_empty());
    assert!(d.files.is_empty());
    let u = d.universe();
    assert!(u.is_closed());
    assert_eq!(u.closure, Closure::Truncated);
    // Denied in FULL: the tenant file the walk did reach binds nothing.
    assert!(matches!(
        governing(&u, &key("tenants/cu/plain.flow.nml")),
        Governing::Unbound
    ));
    assert!(matches!(
        governing(&u, &key("admin/spam/f0")),
        Governing::Unbound
    ));
}

#[test]
fn unit_budget_exhaustion_closes_only_that_unit() {
    // A16 amendment (r73). `tenants/**/*.flow.nml` makes every
    // `tenants/<x>` ONE 65,536-entry budget unit; the glob's literal
    // prefix (`tenants`) is the operator's layout and is charged to the
    // root. A tenant that spends its unit is denied in full — and
    // NOTHING else is: the sibling tenant still binds and validates,
    // the operator's own file elsewhere still binds, `files` excludes
    // the unit, and the universe is still CLOSED, so no verb anywhere
    // degrades to parse-only.
    let mut ws = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[
                ("tenantFlows", &["tenants/**/*.flow.nml"]),
                // Wildcard-free: layout end to end, so it mints no unit.
                ("ops", &["admin/ops.flow.nml"]),
            ],
        )
        .file("tenants/cu/plain.flow.nml")
        .file("tenants/cu/nested/deeper.flow.nml")
        .file("tenants/du/plain.flow.nml")
        .file("admin/ops.flow.nml");
    for i in 0..=crate::workspace::discover::MAX_ENTRIES {
        ws.fs = ws.fs.file(&format!("/ws/tenants/cu/spam/f{i}"));
    }
    let root = ws.root();
    let d = ws.discover(&root, vec![]);

    // The UNIVERSE is intact: not truncated, one live claim, no
    // universe-level error at all.
    assert_eq!(d.truncated, None);
    assert!(d.truncation_error().is_none());
    assert!(d.universe_errors().is_empty());
    assert_eq!(manifest_keys(&d), ["demo.package.nml"]);
    assert!(d.inert.is_empty() && d.load_errors.is_empty());
    assert_eq!(
        d.truncated_units,
        [UnitTruncation {
            unit: key("tenants/cu"),
            stop: key("tenants/cu/spam"),
            why: UnitBound::Entries,
        }]
    );
    let u = d.universe();
    assert!(
        u.is_closed(),
        "a truncated unit never re-opens the universe: the claim that \
         INDUCED the unit is always above it"
    );
    assert_eq!(u.closure, Closure::Complete);

    // DENIED: every key under the unit, at every depth — including the
    // one the walk had already seen before the bound was reached.
    for denied in [
        "tenants/cu/plain.flow.nml",
        "tenants/cu/nested/deeper.flow.nml",
        "tenants/cu/spam/f0",
    ] {
        assert!(
            matches!(governing(&u, &key(denied)), Governing::Unbound),
            "{denied}"
        );
        let r = crate::workspace::discover::resolve_file(
            &u,
            Path::new(&format!("/ws/{denied}")),
            &ws.fs,
        )
        .expect("keyed");
        assert_eq!(r.findings.len(), 1, "{denied}");
        let finding = &r.findings[0];
        assert_eq!(finding.severity, nml_core::diagnostic::Severity::Error);
        assert_eq!(finding.code, Some(codes::UNIVERSE_TRUNCATED));
        assert_eq!(finding.source.as_deref(), Some(denied));
        assert!(
            finding
                .message
                .contains("the discovery budget for `tenants/cu` is exhausted"),
            "{}",
            finding.message
        );
        assert!(
            finding
                .message
                .contains("the walk stopped at `tenants/cu/spam`"),
            "{}",
            finding.message
        );
        assert!(
            finding
                .message
                .contains("files outside `tenants/cu` are unaffected"),
            "{}",
            finding.message
        );
        // E35: the kernel states the fact; `--root` is not even the
        // remedy here (a root inside the unit leaves the operator's
        // manifest outside the universe, which re-opens it).
        assert!(!finding.message.contains("--root"), "{}", finding.message);
    }

    // UNAFFECTED: the sibling tenant, and the operator's own file.
    for (unaffected, binding) in [
        ("tenants/du/plain.flow.nml", "tenantFlows"),
        ("admin/ops.flow.nml", "ops"),
    ] {
        let (pkg, name, ..) = bound(&governing(&u, &key(unaffected)));
        assert_eq!((pkg.as_str(), name.as_str()), ("demo", binding));
        let r = crate::workspace::discover::resolve_file(
            &u,
            Path::new(&format!("/ws/{unaffected}")),
            &ws.fs,
        )
        .expect("keyed");
        assert!(r.findings.is_empty(), "{unaffected}: {:?}", r.findings);
        assert!(d.universe_notes().is_empty());
        assert!(d.inert_notes_for(&key(unaffected)).is_empty());
    }

    // The one enumeration excludes the truncated unit entirely — the
    // file the walk saw before the bound included (a partial listing
    // would read as a whole one).
    assert_eq!(
        files(&d),
        [
            "core.model.nml",
            "demo.package.nml",
            "admin/ops.flow.nml",
            "tenants/du/plain.flow.nml"
        ]
    );
    // Nothing under the unit was ever read.
    assert_eq!(ws.read_keys(), ["demo.package.nml", "core.model.nml"]);
}

#[test]
fn budget_units_do_not_share_a_counter() {
    // A16 amendment (r73): the count is PER LISTING and a directory
    // belongs to exactly one unit, so a tenant cannot straddle two.
    // Two tenants of 40,000 entries each spend 80,000 between them —
    // past the single cumulative bound the walk used before — and
    // NEITHER truncates.
    let mut ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/plain.flow.nml")
        .file("tenants/du/plain.flow.nml");
    for i in 0..40_000u32 {
        ws.fs = ws.fs.file(&format!("/ws/tenants/cu/spam/f{i}"));
        ws.fs = ws.fs.file(&format!("/ws/tenants/du/spam/f{i}"));
    }
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    assert!(d.truncated_units.is_empty());
    let u = d.universe();
    for tenant in ["tenants/cu/plain.flow.nml", "tenants/du/plain.flow.nml"] {
        assert_eq!(bound(&governing(&u, &key(tenant))).1, "tenantFlows");
    }
}

#[test]
fn nested_units_are_charged_to_the_outermost() {
    // A16 amendment (r73): a directory under two unit roots is charged
    // to the OUTERMOST. Deliberate, and the security half of the rule:
    // a tenant whose own live manifest mints unit roots inside its
    // subtree must not thereby multiply its share of the universe-wide
    // backstop.
    //
    // `orgs/*/x.flow.nml` reaches `orgs/a` (unit root) but NOT
    // `orgs/a/sub`, so the manifest there is LIVE, and its `**` would
    // make every `orgs/a/sub/<t>` a unit root of its own.
    let mut ws = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("orgFlows", &["orgs/*/x.flow.nml"])],
        )
        .manifest(
            "orgs/a/sub/own.package.nml",
            "own",
            &[],
            &[("all", &["**"])],
        )
        .file("orgs/a/x.flow.nml")
        .file("orgs/b/x.flow.nml");
    for i in 0..=crate::workspace::discover::MAX_ENTRIES {
        ws.fs = ws.fs.file(&format!("/ws/orgs/a/sub/t1/spam/f{i}"));
    }
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    // The tenant's manifest is live — read, its unit roots computed and
    // ignored — and purged with the spent unit (r75): not a claim.
    assert!(
        ws.read_keys()
            .contains(&"orgs/a/sub/own.package.nml".to_string()),
        "{:?}",
        ws.read_keys()
    );
    assert_eq!(manifest_keys(&d), ["demo.package.nml"]);
    assert_eq!(
        d.truncated_units,
        [UnitTruncation {
            unit: key("orgs/a"),
            stop: key("orgs/a/sub/t1/spam"),
            why: UnitBound::Entries,
        }],
        "the inner unit root `orgs/a/sub/t1` never gets a budget of its own"
    );
    let u = d.universe();
    assert!(matches!(
        governing(&u, &key("orgs/a/x.flow.nml")),
        Governing::Unbound
    ));
    // The sibling org is untouched.
    assert_eq!(
        bound(&governing(&u, &key("orgs/b/x.flow.nml"))).1,
        "orgFlows"
    );
}

#[test]
#[ignore = "perf tier: run with `cargo test -p nml-validate --release --lib -- --ignored perf_` \
            (a 1,048,592-node scripted tree: 7.5 s / 711 MB in release, r73)"]
fn perf_unit_budgets_are_bounded_by_the_universe_backstop() {
    // A16 amendment (r73): per-unit budgets are not a licence to walk
    // forever. MAX_TOTAL_ENTRIES (16 × MAX_ENTRIES) is the sum over
    // every unit and the root; past it the universe truncates exactly
    // as the single cumulative bound did before the amendment, naming
    // the directory being listed.
    //
    // 16 tenants × 65,537 = 1,048,592 > 1,048,576, so the backstop
    // fires inside the sixteenth tenant. Perf tier: the fixture is a
    // million-node scripted tree — the only way to reach the real
    // constant without a second, weaker code path.
    const N: usize = 16;
    let mut ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/t00/plain.flow.nml");
    for t in 0..N {
        for i in 0..=crate::workspace::discover::MAX_ENTRIES {
            ws.fs = ws.fs.file(&format!("/ws/tenants/t{t:02}/spam/f{i}"));
        }
    }
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated,
        Some(Truncation::Entries {
            dir: key("tenants/t15/spam")
        }),
        "the backstop is the whole universe, as it has always been"
    );
    let u = d.universe();
    assert_eq!(u.closure, Closure::Truncated);
    assert!(u.is_closed());
    assert!(d.claims.is_empty() && d.files.is_empty() && d.truncated_units.is_empty());
    assert!(matches!(
        governing(&u, &key("tenants/t00/plain.flow.nml")),
        Governing::Unbound
    ));
}

/// r88 P1: a binding whose declared source fails to LOAD (a parse
/// error) is the kernel's own finding on every key it governs — NML2091,
/// naming the binding, the manifest and the source's first finding where
/// it sits, with that finding as a related note in the source's own file
/// — and the resolution carries NO validator: the file validates under
/// nothing. The manifest's other binding, whose source loads, carries
/// its validator, built ONCE per universe (the same `Arc` on every
/// resolve). Each front end used to judge this for itself: the CLI a
/// usage error (exit 2), the editor "basic validation".
#[test]
fn a_binding_whose_source_fails_to_load_is_denied_per_binding_with_one_build() {
    let manifest = "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema \
                    schemas:\n    - core:\n        file = \"core.model.nml\"\n    - good:\n        \
                    file = \"good.model.nml\"\n\n[]validator validators:\n    - tenantFlows:\n        \
                    files:\n            - \"tenants/**/*.flow.nml\"\n        schemas:\n            - \
                    core\n    - docs:\n        files:\n            - \"docs/**/*.doc.nml\"\n        \
                    schemas:\n            - good\n";
    let ws = Ws::new()
        .text("demo.package.nml", manifest)
        .text("core.model.nml", "model thing:\n    v string\n  x = 1\n")
        .text("good.model.nml", "model note:\n    text string\n")
        .file("tenants/cu/plain.flow.nml")
        .file("docs/readme.doc.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(
        d.load_errors.is_empty(),
        "the manifest itself loads: {:?}",
        d.load_errors
    );
    let u = d.universe();
    let resolve = |path: &str| {
        crate::workspace::discover::resolve_file(&u, Path::new(path), &ws.fs).expect("keyed")
    };
    let tenant = resolve("/ws/tenants/cu/plain.flow.nml");
    assert!(
        matches!(tenant.governing, Governing::Bound { .. }),
        "the claim stands"
    );
    assert!(
        tenant.validator.is_none(),
        "no validator: the file validates under nothing"
    );
    let [finding] = tenant.findings.as_slice() else {
        panic!("one finding, got {:?}", tenant.findings);
    };
    assert_eq!(finding.code, Some(codes::VALIDATOR_UNBUILDABLE));
    assert_eq!(finding.severity, nml_core::diagnostic::Severity::Error);
    assert_eq!(finding.source.as_deref(), Some("tenants/cu/plain.flow.nml"));
    assert!(
        finding.message.starts_with(
            "binding 'tenantFlows' of demo.package.nml cannot build its validator: declared \
             source `core` failed to load at core.model.nml:3:1 (finding 1 of 5): "
        ) && finding.message.contains("(finding 1 of 5)")
            && finding
                .message
                .ends_with("— the file validates under no binding until the source loads"),
        "{}",
        finding.message
    );
    let [related] = finding.related.as_slice() else {
        panic!("one related note, got {:?}", finding.related);
    };
    assert_eq!(related.source.as_deref(), Some("core.model.nml"));
    assert_eq!(
        related.span.start, 26,
        "the first finding's span, in the source's own text"
    );
    // The sibling binding is unaffected, and its validator is built once.
    let docs = resolve("/ws/docs/readme.doc.nml");
    assert!(docs.findings.is_empty(), "{:?}", docs.findings);
    let first = docs.validator.clone().expect("the docs binding composes");
    let again = resolve("/ws/docs/readme.doc.nml")
        .validator
        .expect("still composes");
    assert!(Arc::ptr_eq(&first, &again), "built once per universe");
    assert_eq!(
        u.validators.len(),
        2,
        "one failure and one validator memoized — and nothing else: strictness is the \
         binding's own, never a front end's twin"
    );
}

#[test]
fn declared_source_text_is_read_once_and_shared_across_manifests() {
    // r73 DoS bound (a). Every live manifest is loaded and RETAINS each
    // declared source. N manifests in one directory declaring one source
    // retained N copies: measured 1,927 MB at N = 500 over a 3.99 MiB
    // source, and the entry bound admits ~32k such manifests. The text
    // is now read ONCE per (directory, file) in one discovery and
    // shared — one read, one allocation, N referents.
    const N: usize = 8;
    let mut ws = Ws::new().manifest("demo.package.nml", "demo", &[], TENANTS);
    // `vendor/` is content NO glob reaches, so these manifests are LIVE
    // (under `tenants/**` they would be inert and never read at all).
    ws = ws.text("vendor/core.model.nml", DEMO_CORE);
    for i in 0..N {
        ws = ws.text(
            &format!("vendor/m{i}.package.nml"),
            &manifest_text(&format!("m{i}"), &[], &[("b", &["never/*.q.nml"])]),
        );
    }
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.claims.len(), N + 1, "every vendor manifest is live");
    assert!(d.load_errors.is_empty() && d.truncated.is_none());
    // ONE read of the shared source, whatever N is.
    let reads = ws.read_keys();
    assert_eq!(
        reads
            .iter()
            .filter(|k| *k == "vendor/core.model.nml")
            .count(),
        1,
        "{reads:?}"
    );
    // ONE allocation behind all N packages.
    let shared: Vec<*const u8> = d
        .claims
        .iter()
        .filter(|c| c.manifest().is_some_and(|m| m.depth() == 2))
        .map(|c| c.package.sources[0].1.as_ptr())
        .collect();
    assert_eq!(shared.len(), N);
    assert!(
        shared.windows(2).all(|w| w[0] == w[1]),
        "N manifests, one retained copy"
    );
    // Sharing is per (directory, file): the operator's own
    // `core.model.nml` beside the root manifest is a different key and
    // its own read.
    assert_eq!(
        reads.iter().filter(|k| *k == "core.model.nml").count(),
        1,
        "{reads:?}"
    );
}

#[test]
fn live_input_byte_budget_truncates_closed_denied() {
    // r73 DoS bound (b). Dedup removes the ×N of ONE source; nothing
    // can remove the ×N of DISTINCT sources, so the universe-wide
    // live-input budget does: past MAX_LIVE_INPUT_BYTES the walk
    // truncates, closed-denied and LOUD, naming the input that crossed
    // it. Seventeen 4 MiB sources = 68 MiB > the 64 MiB budget.
    let big: String = "# ".to_string() + &"x".repeat(4 * 1024 * 1024) + "\n";
    let mut ws = Ws::new().manifest("demo.package.nml", "demo", &[], TENANTS);
    for i in 0..17 {
        ws = ws.text(&format!("vendor/v{i}/core.model.nml"), &big).text(
            &format!("vendor/v{i}/m{i}.package.nml"),
            &manifest_text(&format!("m{i}"), &[], &[("b", &["never/*.q.nml"])]),
        );
    }
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let Some(Truncation::LiveInputBytes { key: spent }) = &d.truncated else {
        panic!("not a byte-budget truncation: {:?}", d.truncated);
    };
    assert_eq!(spent.file_name(), "core.model.nml");
    // Closed-denied in full, exactly like the entry bound.
    let u = d.universe();
    assert_eq!(u.closure, Closure::Truncated);
    assert!(u.is_closed());
    assert!(d.claims.is_empty() && d.files.is_empty());
    assert!(matches!(
        governing(&u, &key("tenants/cu/plain.flow.nml")),
        Governing::Unbound
    ));
    let error = d.truncation_error().expect("loud");
    assert_eq!(error.severity, nml_core::diagnostic::Severity::Error);
    assert_eq!(error.code, Some(codes::UNIVERSE_TRUNCATED));
    assert!(
        error
            .message
            .contains("the live-input budget (67108864 bytes) was spent reading `"),
        "{}",
        error.message
    );
    assert_eq!(error.source.as_deref(), Some(spent.as_str()));
    // r75: the sentence ends in the kernel's own remedy and offers no
    // flag — the CLI appends no `--root` advice to this row.
    assert!(
        error.message.ends_with("declared sources under the root"),
        "{}",
        error.message
    );
    assert!(!error.message.contains("--root"), "{}", error.message);
    // Sixteen of them fit: the bound is a bound, not a trip-wire.
    let mut ws = Ws::new().manifest("demo.package.nml", "demo", &[], TENANTS);
    for i in 0..15 {
        ws = ws.text(&format!("vendor/v{i}/core.model.nml"), &big).text(
            &format!("vendor/v{i}/m{i}.package.nml"),
            &manifest_text(&format!("m{i}"), &[], &[("b", &["never/*.q.nml"])]),
        );
    }
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    assert_eq!(d.claims.len(), 16);
}

/// r88 P11 (spike): a manifest's declared `budgetUnits` replace the
/// inference from its globs — the three loud layouts of E38 become
/// per-tenant units by one line — and a declaration is read whole
/// (a declared `tenants/*` makes `tenants/<x>` the unit and nothing
/// below or beside it one).
#[test]
fn declared_budget_units_replace_inference() {
    let unit_root = |globs: &[&str], units: &[&str], dir: &str| {
        let package = SchemaPackage::from_parts(
            &manifest_text_with_units("demo", &[], units, &[("b", globs)]),
            |_| Ok(DEMO_CORE.to_string()),
        )
        .expect("package loads");
        let claim = ManifestClaim::workspace(
            Arc::new(package),
            key("demo.package.nml"),
            SourceKey::root(),
            true,
        );
        let mut probes = 0usize;
        crate::workspace::claims::is_budget_unit(
            std::slice::from_ref(&claim),
            &key(dir),
            &mut probes,
        )
    };
    let table: &[(&[&str], &[&str], &str, bool)] = &[
        // E38's first loud layout: inference put the unit at
        // `tenants/<x>/flows/<y>`; the declaration puts it at the tenant.
        (&["tenants/*/flows/**"], &["tenants/*"], "tenants/cu", true),
        (
            &["tenants/*/flows/**"],
            &["tenants/*"],
            "tenants/cu/flows",
            false,
        ),
        (
            &["tenants/*/flows/**"],
            &["tenants/*"],
            "tenants/cu/flows/x",
            false,
        ),
        (&["tenants/*/flows/**"], &["tenants/*"], "tenants", false),
        (&["tenants/*/flows/**"], &["tenants/*"], "shared", false),
        // The org layout, declared per org (shallower than inference).
        (&["orgs/*/tenants/**"], &["orgs/*"], "orgs/acme", true),
        (
            &["orgs/*/tenants/**"],
            &["orgs/*"],
            "orgs/acme/tenants/cu",
            false,
        ),
        // A catch-all, declared as inference would infer it.
        (&["**"], &["*"], "tenants", true),
        (&["**"], &["*"], "tenants/cu", false),
        // Two globs, two declarations, both read.
        (
            &["tenants/**", "vendor/**"],
            &["tenants/*", "vendor"],
            "tenants/cu",
            true,
        ),
        (
            &["tenants/**", "vendor/**"],
            &["tenants/*", "vendor"],
            "vendor",
            true,
        ),
        (
            &["tenants/**", "vendor/**"],
            &["tenants/*", "vendor"],
            "vendor/sub",
            false,
        ),
        (
            &["tenants/**", "vendor/**"],
            &["tenants/*", "vendor"],
            "docs",
            false,
        ),
        // No declaration: inference, unchanged.
        (&["tenants/*/flows/**"], &[], "tenants/cu", false),
        (&["tenants/*/flows/**"], &[], "tenants/cu/flows/x", true),
    ];
    for (globs, units, dir, want) in table {
        assert_eq!(
            unit_root(globs, units, dir),
            *want,
            "globs {globs:?} units {units:?} dir {dir:?}"
        );
    }
}

/// r88 P11 (spike): under E38's loud layout `tenants/*/flows/**` a
/// tenant's entries OUTSIDE the inferred unit (`tenants/<x>/other/`)
/// denied the whole universe; one declared line isolates the tenant —
/// its unit is denied, the sibling tenant stands, the universe stays
/// closed and complete.
#[test]
fn a_declared_unit_isolates_the_flood_inference_left_in_the_root_unit() {
    let flood = |units: &[&str]| {
        let mut ws = Ws::new()
            .text(
                "demo.package.nml",
                &manifest_text_with_units(
                    "demo",
                    &[],
                    units,
                    &[("flows", &["tenants/*/flows/**"])],
                ),
            )
            .text("core.model.nml", DEMO_CORE)
            .file("tenants/cu/flows/a.flow.nml")
            .file("tenants/du/flows/b.flow.nml");
        for i in 0..12 {
            ws = ws.file(&format!("tenants/cu/other/f{i}.nml"));
        }
        let root = ws.root();
        ws.discover_scaled(&root, vec![], 8, 1024)
    };
    // Inference: the flood is the root unit's — the whole universe is
    // truncated (E38's loud layout).
    let d = flood(&[]);
    assert!(
        d.truncated.is_some(),
        "inference: the universe should be truncated"
    );
    // Declared: the tenant's unit is denied, everything else stands.
    let d = flood(&["tenants/*"]);
    assert!(
        d.truncated.is_none(),
        "declared: the universe stands: {:?}",
        d.truncated
    );
    assert_eq!(
        d.truncated_units
            .iter()
            .map(|t| t.unit.as_str())
            .collect::<Vec<_>>(),
        ["tenants/cu"]
    );
    assert!(
        d.files
            .iter()
            .any(|k| k.as_str() == "tenants/du/flows/b.flow.nml")
    );
    assert!(
        !d.files
            .iter()
            .any(|k| k.as_str().starts_with("tenants/cu/"))
    );
}

/// r89 (P11): the gap lint is a universe note — carried by `notes_for`
/// for every key (the CLI prints it once per run), attributed to the
/// manifest, spanned at the glob — for an OPERATOR-LEVEL manifest only:
/// a tenant's live manifest in a gap of the operator's glob mints no
/// unit, so its layout is never linted; and an explicit `budgetUnits`
/// silences it. `ClaimOrigin::Workspace.operator_level` is the walk's
/// verdict, pinned here.
#[test]
fn the_gap_lint_rides_the_universe_notes_for_operator_level_manifests_only() {
    let ws = Ws::new()
        .text(
            "demo.package.nml",
            &manifest_text("demo", &[], &[("flows", &["tenants/*/flows/**"])]),
        )
        .text("core.model.nml", DEMO_CORE)
        .file("tenants/cu/flows/a.flow.nml")
        // A tenant's manifest in the GAP (`tenants/cu/other/` is root-unit
        // content the glob does not reach): live, not operator-level, and
        // its own loud glob mints nothing.
        .text(
            "tenants/cu/other/evil.package.nml",
            &manifest_text("evil", &[], &[("loud", &["a/*/b/**"])]),
        )
        .text("tenants/cu/other/core.model.nml", DEMO_CORE)
        .file("tenants/cu/other/a/x/b/y.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let levels: Vec<(String, bool)> = d
        .claims
        .iter()
        .filter_map(|c| match &c.origin {
            ClaimOrigin::Workspace {
                manifest,
                operator_level,
                ..
            } => Some((manifest.to_string(), *operator_level)),
            _ => None,
        })
        .collect();
    assert_eq!(
        levels,
        [
            ("demo.package.nml".to_string(), true),
            ("tenants/cu/other/evil.package.nml".to_string(), false)
        ]
    );
    let notes = d.universe_notes();
    let gaps: Vec<&Diagnostic> = notes
        .iter()
        .filter(|n| n.code == Some(codes::BUDGET_UNIT_GAP))
        .collect();
    assert_eq!(gaps.len(), 1, "{notes:?}");
    assert_eq!(gaps[0].source.as_deref(), Some("demo.package.nml"));
    assert!(gaps[0].span.is_some(), "spanned at the glob");
    assert!(
        gaps[0]
            .message
            .starts_with("binding 'flows' files[0] = \"tenants/*/flows/**\": the inferred budget unit is `tenants/*/flows/*`"),
        "{}",
        gaps[0].message
    );
    // A universe note, not a per-key one: no key's own list carries it.
    for k in ["tenants/cu/flows/a.flow.nml", "docs/readme.nml"] {
        assert!(
            d.inert_notes_for(&key(k))
                .iter()
                .all(|n| n.code != Some(codes::BUDGET_UNIT_GAP)),
            "{k}: a universe note, not a per-key one"
        );
    }
    // Declared: silent, and the declaration is the walk's unit shape.
    let ws = Ws::new()
        .text(
            "demo.package.nml",
            &manifest_text_with_units(
                "demo",
                &[],
                &["tenants/*"],
                &[("flows", &["tenants/*/flows/**"])],
            ),
        )
        .text("core.model.nml", DEMO_CORE)
        .file("tenants/cu/flows/a.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(d.layout_notes().is_empty());
}

#[test]
fn budget_units_follow_the_last_wildcard_run() {
    // A16 amendment (r73; boundary refined r74 decision 4), the
    // inference rule as a table: a claiming glob's literal segments are
    // the operator's layout wherever they sit, and the directories
    // matched at the start of its LAST run of wildcard directory
    // segments are the units. Every row is anchored at the root.
    let unit_root = |glob: &str, dir: &str| {
        let package =
            SchemaPackage::from_parts(&manifest_text("demo", &[], &[("b", &[glob])]), |_| {
                Ok(DEMO_CORE.to_string())
            })
            .expect("package loads");
        let claim = ManifestClaim::workspace(
            Arc::new(package),
            key("demo.package.nml"),
            SourceKey::root(),
            true,
        );
        let mut probes = 0usize;
        crate::workspace::claims::is_budget_unit(
            std::slice::from_ref(&claim),
            &key(dir),
            &mut probes,
        )
    };
    let table: &[(&str, &str, bool)] = &[
        // The RFC's own spelling: one unit per tenant.
        ("tenants/**/*.flow.nml", "tenants", false),
        ("tenants/**/*.flow.nml", "tenants/cu", true),
        ("tenants/**/*.flow.nml", "tenants/cu/nested", false),
        // The `*`-then-`**` spelling agrees.
        ("tenants/*/**", "tenants/cu", true),
        // A bare `**`, or a root-relative `**/…`: the top-level dirs.
        ("**", "admin", true),
        ("**/*.flow.nml", "admin", true),
        ("**/*.flow.nml", "admin/deeper", false),
        // A `*` that cannot descend reaches no directory at all.
        ("*", "admin", false),
        ("*.flow.nml", "admin", false),
        // Wildcard-free: layout end to end, no unit anywhere.
        ("admin/ops.flow.nml", "admin", false),
        ("admin/ops.flow.nml", "admin/ops.flow.nml", false),
        // A longer literal prefix pushes the unit down with it.
        ("tenants/prod/**", "tenants/prod", false),
        ("tenants/prod/**", "tenants/prod/cu", true),
        // The r73 surprise, settled: a nested namespace infers
        // per-TENANT units — the literal `tenants` inside every org is
        // the operator's layout, and the unit is where the last run of
        // wildcards begins. (r73's first-wildcard rule stopped at the
        // org, so one tenant's spam denied every sibling tenant in it.)
        ("orgs/*/tenants/**", "orgs/acme", false),
        ("orgs/*/tenants/**", "orgs/acme/tenants", false),
        ("orgs/*/tenants/**", "orgs/acme/tenants/cu", true),
        ("orgs/*/tenants/**", "orgs/acme/tenants/cu/deeper", false),
        // Operator layout INSIDE each tenant keeps the tenant as the unit
        // (the one-liner r73 proposed instead — "the first wildcard with
        // no literal after it" — would infer no unit here at all).
        ("tenants/*/flows/*.flow.nml", "tenants/cu", true),
        ("tenants/*/flows/*.flow.nml", "tenants/cu/flows", false),
        ("orgs/*/tenants/*/flows/*.flow.nml", "orgs/acme", false),
        (
            "orgs/*/tenants/*/flows/*.flow.nml",
            "orgs/acme/tenants/cu",
            true,
        ),
        // The honest residual of the last-run rule (E38): a `**` AFTER
        // the operator's literal starts a run of its own, so
        // `tenants/*/flows/**` delegates at `tenants/<x>/flows/<y>` —
        // not at the tenant — and `tenants/<x>/other/` is the root
        // unit's. An operator whose delegation boundary is shallower
        // than the deepest wildcard needs item 4's explicit spelling.
        ("tenants/*/flows/**", "tenants/cu", false),
        ("tenants/*/flows/**", "tenants/cu/flows", false),
        ("tenants/*/flows/**", "tenants/cu/flows/x", true),
    ];
    for (glob, dir, want) in table {
        assert_eq!(unit_root(glob, dir), *want, "{glob} over {dir}");
    }
}

#[test]
fn ambiguous_claim_is_an_error_finding_naming_both_manifests() {
    // r51 #3: two live manifests claiming one key — `resolve_file` keeps
    // the `Ambiguous` governing AND carries the denial as an ERROR
    // finding naming both claimants (in manifest order, whatever order
    // they were collected in), so no front end can read the ambiguity
    // as "unbound, the flags decide" and validate parse-only. Pre-fix
    // `findings` was empty.
    let ws = Ws::new()
        .manifest(
            "other.package.nml",
            "other",
            &[],
            &[("sharedToo", &["shared/**/*.flow.nml"])],
        )
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("shared", &["shared/**/*.flow.nml"])],
        )
        .file("shared/nouses.flow.nml")
        .file("docs/x.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let r = crate::workspace::discover::resolve_file(
        &u,
        Path::new("/ws/shared/nouses.flow.nml"),
        &ws.fs,
    )
    .unwrap();
    assert!(matches!(&r.governing, Governing::Ambiguous(c) if c.len() == 2));
    let [finding] = r.findings.as_slice() else {
        panic!("one finding, got {:?}", r.findings);
    };
    assert_eq!(finding.severity, nml_core::diagnostic::Severity::Error);
    assert_eq!(
        finding.code,
        Some(codes::AMBIGUOUS_CLAIM),
        "coded (r69a B5)"
    );
    assert_eq!(finding.source.as_deref(), Some("shared/nouses.flow.nml"));
    assert!(
        finding.message.starts_with(
            "2 manifests claim this file: demo.package.nml (shared, files[0] = \
             \"shared/**/*.flow.nml\"), other.package.nml (sharedToo, files[0] = \
             \"shared/**/*.flow.nml\") — an ambiguously-claimed file is denied"
        ),
        "{}",
        finding.message
    );
    // An unbound key in the same universe carries no finding.
    let r =
        crate::workspace::discover::resolve_file(&u, Path::new("/ws/docs/x.nml"), &ws.fs).unwrap();
    assert!(r.findings.is_empty());
    assert!(matches!(r.governing, Governing::Unbound));
}

#[test]
fn ambiguous_claimants_collect_in_name_order_and_render_in_label_order() {
    // r52 #6a, re-pinned in r65 (r64 finding 3): rule 3 collects the
    // claimants in package-NAME order (`governing` looks the names up
    // sorted) and `ambiguous_claim_summary` renders them in manifest-
    // LABEL order — neither depends on the order the universe holds its
    // claims in. Since E35's stem rule a discovered manifest's label IS
    // its name, so a discovered universe cannot tell the sort from the
    // directory listing (the old pin over `zeta.package.nml` +
    // `alpha.package.nml` passed with the sort removed). The universe
    // is therefore built BY HAND: two root-anchored workspace claims
    // held in REVERSE name order, whose labels sort the other way from
    // their names (`z.package.nml` holds `alpha`, `a.package.nml` holds
    // `zeta`).
    let glob = "shared/**/*.flow.nml";
    let claim = |name: &str, binding: &str, label: &str| {
        let package =
            SchemaPackage::from_parts(&manifest_text(name, &[], &[(binding, &[glob])]), |_| {
                Ok(DEMO_CORE.to_string())
            })
            .expect("package loads");
        ManifestClaim::workspace(Arc::new(package), key(label), SourceKey::root(), true)
    };
    let claims = [
        claim("zeta", "zb", "a.package.nml"),
        claim("alpha", "ab", "z.package.nml"),
    ];
    let ws = Ws::new();
    let root = ws.root();
    let validators = ValidatorMemo::default();
    let u = Universe {
        root: &root,
        claims: &claims,
        configs: &[],
        closure: Closure::Complete,
        truncated_units: &[],
        validators: &validators,
    };
    let claimants = match governing(&u, &key("shared/n.flow.nml")) {
        Governing::Ambiguous(claimants) => claimants,
        other => panic!("not ambiguous: {other:?}"),
    };
    let names: Vec<&str> = claimants.iter().map(|c| c.claim.name()).collect();
    assert_eq!(
        names,
        ["alpha", "zeta"],
        "collected in package-name order, not the universe's order — the sort is load-bearing"
    );
    let labels: Vec<&str> = claimants
        .iter()
        .map(|c| c.claim.manifest_label.as_str())
        .collect();
    assert_eq!(
        labels,
        ["z.package.nml", "a.package.nml"],
        "the labels sort the other way from the names"
    );
    let rendered = format!(
        "2 manifests claim this file: a.package.nml (zb, files[0] = {glob:?}), \
         z.package.nml (ab, files[0] = {glob:?})"
    );
    assert_eq!(
        crate::workspace::diag::ambiguous_claim_summary(&claimants),
        rendered,
        "rendered in manifest-label order — its own sort, whatever the collection order"
    );
    let mut reversed = claimants.clone();
    reversed.reverse();
    assert_eq!(
        crate::workspace::diag::ambiguous_claim_summary(&reversed),
        rendered
    );
}

#[test]
fn inert_manifest_is_never_read() {
    // E28 (4a): a malformed tenant manifest inside claimed content is
    // settled inert BEFORE loading — never read, never a load error, so
    // no check in the repository fails on it (pre-fix every `nml check`
    // in the repo failed with "manifest failed to load").
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .text(
            "tenants/cu/evil.package.nml",
            "package evil:\n    this is not a manifest\n",
        )
        .file("tenants/cu/plain.flow.nml")
        .file("vendor/base.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(d.load_errors.is_empty(), "{:?}", d.load_errors);
    assert_eq!(inert_keys(&d), ["tenants/cu/evil.package.nml"]);
    assert!(
        !ws.read_keys()
            .iter()
            .any(|k| k == "tenants/cu/evil.package.nml"),
        "the inert manifest was read: {:?}",
        ws.read_keys()
    );
    let u = d.universe();
    assert_eq!(u.workspace_claims(), 1);
    assert_eq!(
        bound(&governing(&u, &key("tenants/cu/plain.flow.nml"))).0,
        "demo"
    );
    assert!(matches!(
        governing(&u, &key("vendor/base.flow.nml")),
        Governing::Unbound
    ));
}

/// An unloadable manifest's row is LOCATED at its first finding: the row
/// carries the finding's span, the kernel keeps the text it read the
/// manifest from (no claim exists to hold it) and derives the line and
/// column ONCE for every front end — a meta-schema finding and a parse
/// error alike; the sentence names no line. A foreign source, a span
/// past the text or no span locate nothing.
#[test]
fn an_unloadable_manifests_row_is_located_in_the_text_the_kernel_kept() {
    for (tag, needle, text) in [
        (
            "meta",
            "versio",
            manifest_text("demo", &[], TENANTS)
                .replace("version = \"0.1.0\"", "versio = \"0.1.0\""),
        ),
        (
            "parse",
            "  oops",
            format!("{}\n  oops\n", manifest_text("demo", &[], TENANTS)),
        ),
    ] {
        let ws = Ws::new()
            .text("demo.package.nml", &text)
            .text("core.model.nml", DEMO_CORE)
            .file("tenants/cu/plain.flow.nml");
        let root = ws.root();
        let d = ws.discover(&root, vec![]);
        assert!(d.claims.is_empty(), "{tag}");
        assert_eq!(d.load_errors.len(), 1, "{tag}: {:?}", d.load_errors);
        let row = &d.load_errors[0];
        assert_eq!(row.code, Some(codes::RESOLUTION_INPUT_UNLOADABLE), "{tag}");
        assert!(
            row.message.starts_with("manifest failed to load"),
            "{tag}: {}",
            row.message
        );
        assert!(
            !row.message.contains("validation at "),
            "{tag}: the sentence names no line: {}",
            row.message
        );
        let span = row
            .span
            .unwrap_or_else(|| panic!("{tag}: spanned at the finding: {row:?}"));
        assert_eq!(
            d.manifest_text("demo.package.nml"),
            Some(text.as_str()),
            "{tag}"
        );
        let at = d
            .manifest_location(row)
            .unwrap_or_else(|| panic!("{tag}: located"));
        let expect = nml_core::span::SourceMap::new(&text).location(span.start);
        assert_eq!((at.line, at.column), (expect.line, expect.column), "{tag}");
        let site = nml_core::span::SourceMap::new(&text)
            .location(text.find(needle).expect("the finding's site"));
        assert_eq!(
            (at.line, at.column),
            (site.line, site.column),
            "{tag}: at the finding"
        );
        let stale = nml_core::diagnostic::Diagnostic::error("x")
            .with_source("demo.package.nml".to_string())
            .with_span(nml_core::span::Span::new(text.len() + 1, text.len() + 2));
        assert_eq!(d.manifest_location(&stale), None, "{tag}: past the text");
        let foreign = nml_core::diagnostic::Diagnostic::error("x")
            .with_source("tenants/cu/plain.flow.nml".to_string())
            .with_span(nml_core::span::Span::new(0, 1));
        assert_eq!(d.manifest_location(&foreign), None, "{tag}: a content file");
        assert_eq!(d.manifest_text("tenants/cu/plain.flow.nml"), None, "{tag}");
        let spanless = nml_core::diagnostic::Diagnostic::error("x")
            .with_source("demo.package.nml".to_string());
        assert_eq!(d.manifest_location(&spanless), None, "{tag}: no span");
        assert_eq!(d.universe().closure, Closure::Unloadable(1), "{tag}");
    }
}

#[test]
fn declared_source_through_a_symlink_is_refused_unopened() {
    // E28 (4b): a LIVE manifest whose declared source is a symlink (to a
    // FIFO, in the r50 probe — discovery hung) is refused by kind through
    // the oracle: the source is never handed to the reader.
    let ws = Ws::new()
        .text("demo.package.nml", &manifest_text("demo", &[], TENANTS))
        .symlink("core.model.nml", "/dev/pipe")
        .file("tenants/cu/plain.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(d.claims.is_empty());
    assert_eq!(d.load_errors.len(), 1);
    assert_eq!(d.load_errors[0].source.as_deref(), Some("demo.package.nml"));
    // r69b: the sentence names the source, its locator in the manifest
    // (`schemas[i].file`), the manifest, the verdict and the next step —
    // and carries NML2088 from the kernel.
    assert_eq!(
        d.load_errors[0].code,
        Some(codes::RESOLUTION_INPUT_UNLOADABLE)
    );
    assert_eq!(
        d.load_errors[0].message,
        "manifest failed to load: declared source `core.model.nml` (schemas[0].file in \
         `demo.package.nml`) is unavailable: a symlink — a declared source is never read \
         through a link; replace the link with the file itself"
    );
    assert_eq!(ws.read_keys(), ["demo.package.nml"]);
    assert_eq!(d.universe().closure, Closure::Unloadable(1));
    assert!(
        d.universe().is_closed(),
        "fail-closed: counted as discovered"
    );
    assert_eq!(shown(&d.universe_errors()), shown(&d.load_errors));
    // A directory in a source's place is refused the same way; an absent
    // one says what to do.
    let mut ws = Ws::new()
        .text("demo.package.nml", &manifest_text("demo", &[], TENANTS))
        .file("tenants/cu/plain.flow.nml");
    ws.fs = ws.fs.dir("/ws/core.model.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(
        d.load_errors[0]
            .message
            .ends_with("is unavailable: a directory, not a file — name a regular file"),
        "{}",
        d.load_errors[0].message
    );
    let ws = Ws::new()
        .text(
            "tenants/cu/demo.package.nml",
            &manifest_text("demo", &[], TENANTS),
        )
        .file("tenants/cu/plain.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.load_errors[0].message,
        "manifest failed to load: declared source `core.model.nml` (schemas[0].file in \
         `tenants/cu/demo.package.nml`) is unavailable: absent — create it beside the \
         manifest, or fix the `file` path"
    );
    assert_eq!(
        d.load_errors[0].source.as_deref(),
        Some("tenants/cu/demo.package.nml")
    );
}

#[test]
fn manifest_read_refusal_names_the_manifest_not_a_declared_source() {
    // r69b (r69a B4's kernel half): the reader's refusal of the
    // manifest's OWN bytes (a byte cap, an open failure) is the load
    // error's sentence, attributed to the manifest — pre-fold it was
    // wrapped as `declared source 'demo.package.nml' is unavailable:
    // …`, the manifest named as its own declared source.
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/plain.flow.nml");
    let root = ws.root();
    let refusing = |kind: InputKind, p: &Path| -> Result<String, String> {
        if kind == InputKind::Manifest {
            return Err(
                "too large: 300 KiB (307200 bytes) — a package manifest is read only up to \
                 256 KiB (262144 bytes)"
                    .to_string(),
            );
        }
        ws.read()(kind, p)
    };
    let d = crate::workspace::discover::discover(&root, &ws.fs, &refusing, vec![], Arc::default());
    assert_eq!(d.load_errors.len(), 1, "{:?}", d.load_errors);
    assert_eq!(d.load_errors[0].source.as_deref(), Some("demo.package.nml"));
    assert_eq!(
        d.load_errors[0].code,
        Some(codes::RESOLUTION_INPUT_UNLOADABLE)
    );
    assert_eq!(
        d.load_errors[0].message,
        "manifest failed to load: too large: 300 KiB (307200 bytes) — a package manifest is \
         read only up to 256 KiB (262144 bytes)"
    );
    assert!(
        !d.load_errors[0].message.contains("declared source"),
        "{}",
        d.load_errors[0].message
    );
}

#[test]
fn inert_manifest_markers_make_no_noise() {
    // E28 (4c): the marker names an INERT tenant manifest declares are
    // dead — no NML2080 "root marker is inert" note for any README.md in
    // the tree (pre-fix: one per README.md inside claimed content).
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .manifest(
            "tenants/cu/evil.package.nml",
            "evil",
            &["README.md"],
            &[("all", &["**"])],
        )
        .file("tenants/cu/README.md")
        .file("shared/README.md")
        .file("tenants/cu/plain.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(inert_keys(&d), ["tenants/cu/evil.package.nml"]);
    assert!(
        d.inert.iter().all(|n| !n.message.contains("README.md")),
        "{:?}",
        inert_keys(&d)
    );
    // A LIVE package's marker inside claimed content is still noted —
    // that note is real: the marker would have moved the anchor.
    let store = external(
        "kit",
        &["README.md"],
        &[("all", &["**/*.nml"])],
        ExternalClass::Store,
    );
    let d = ws.discover(&root, vec![store]);
    let mut inert = inert_keys(&d);
    inert.sort();
    assert_eq!(
        inert,
        ["tenants/cu/README.md", "tenants/cu/evil.package.nml"]
    );
    assert!(matches!(
        &d.claims.iter().find(|c| c.class() == ClaimClass::Store).unwrap().origin,
        ClaimOrigin::External { markers, .. } if markers == &[key("shared")]
    ));
}

#[test]
fn unreadable_live_config_is_a_load_error() {
    // A live config the reader cannot deliver must not silently read as
    // absent (its pins and autoAssociate are unknown): a load error,
    // closing the universe like an unloadable manifest.
    let mut ws = Ws::new().file("x.nml");
    ws.fs = ws.fs.file("/ws/nml-project.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(d.configs.is_empty());
    assert_eq!(d.load_errors.len(), 1);
    assert!(
        d.load_errors[0]
            .message
            .starts_with("project config failed to load: absent")
    );
    assert_eq!(
        d.load_errors[0].code,
        Some(codes::RESOLUTION_INPUT_UNLOADABLE)
    );
    assert!(d.universe().is_closed());
    assert_eq!(d.universe().closure, Closure::Unloadable(1));
}

#[test]
fn walk_never_loads_through_symlinks() {
    // A symlinked manifest or a symlinked directory is neither a claim
    // nor descended: a link is never a resolution input.
    let ws = Ws::new()
        .manifest(
            "elsewhere/evil.package.nml",
            "evil",
            &[],
            &[("all", &["**"])],
        )
        .symlink("link.package.nml", "elsewhere/evil.package.nml")
        .symlink("linked", "elsewhere")
        .file("x.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    // `elsewhere/` itself is walked (a real dir); the links are not.
    assert_eq!(manifest_keys(&d), ["elsewhere/evil.package.nml"]);
    assert!(!ws.fs.named("link.package.nml"));
    // r69b (cov r68 M31): a symlinked manifest that BECAME an input would
    // surface as a load error (the reader has no text at the link's
    // path) and CLOSE the universe — the stem rule masked exactly that
    // regression as a load error. So: no load error, and the read log
    // never names the link.
    assert!(d.load_errors.is_empty(), "{:?}", d.load_errors);
    assert_eq!(
        ws.read_keys(),
        ["elsewhere/evil.package.nml", "elsewhere/core.model.nml"]
    );
    assert_eq!(d.universe().closure, Closure::Complete);
    // The one enumeration: regular files only, by depth then key — the
    // links and the directory are not in it.
    assert_eq!(
        files(&d),
        [
            "x.nml",
            "elsewhere/core.model.nml",
            "elsewhere/evil.package.nml"
        ]
    );
}

#[test]
fn notes_for_is_the_universe_errors_then_the_inert_chain() {
    // r69b: the per-key notes live in the kernel (moved from the CLI's
    // pipeline) so the editor prints the CLI's sentences — the
    // universe's errors first, then the NML2080 notes on the key's
    // ancestor chain and nothing off it.
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], &[("all", &["**/*.nml"])])
        .config("tenants/cu/nml-project.nml", "    autoAssociate = false\n")
        .config(
            "tenants/other/nml-project.nml",
            "    autoAssociate = false\n",
        )
        .text(
            "broken.package.nml",
            "package broken:\n    not a manifest\n",
        )
        .file("tenants/cu/x.nml")
        .file("vendor/y.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.load_errors.len(), 1);
    let errors = d.universe_errors();
    assert_eq!(shown(&errors), shown(&d.load_errors));
    let notes: Vec<Diagnostic> = d
        .universe_notes()
        .into_iter()
        .chain(d.inert_notes_for(&key("tenants/cu/x.nml")))
        .collect();
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert_eq!(notes[0].to_string(), errors[0].to_string());
    assert_eq!(notes[1].code, Some(codes::INERT_RESOLUTION_INPUT));
    assert_eq!(
        notes[1].source.as_deref(),
        Some("tenants/cu/nml-project.nml")
    );
    assert_eq!(
        shown(&d.inert_notes_for(&key("tenants/cu/x.nml"))),
        [notes[1].to_string()]
    );
    // Off the chain: the universe's errors only.
    assert_eq!(shown(&d.universe_notes()), shown(&errors));
    assert!(d.inert_notes_for(&key("vendor/y.nml")).is_empty());
    // No `--root` advice anywhere in Layer B (E35): the CLI appends it.
    assert!(notes.iter().all(|n| !n.message.contains("--root")));
}

#[test]
fn load_error_is_attributed_by_key() {
    let ws = Ws::new()
        .text("bad.package.nml", "package bad:\n    version = \"1\"\n")
        .file("x.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(d.claims.is_empty());
    assert_eq!(d.load_errors.len(), 1);
    assert_eq!(d.load_errors[0].source.as_deref(), Some("bad.package.nml"));
    // A manifest that fails to load is not a claim — but it still CLOSES
    // the universe (fail-closed: a malformed operator manifest must not
    // reopen the repo into the permissive default) and counts as
    // discovered.
    let u = d.universe();
    assert!(u.is_closed());
    assert_eq!(u.workspace_claims(), 1);
    assert!(matches!(governing(&u, &key("x.nml")), Governing::Unbound));
}

#[test]
fn closed_symlinked_file_is_2083_form1() {
    // The checked file resolved in a closed universe: `lib` is a symlink
    // ⇒ NML2083 form 1, unbound, byte-identical across target existence.
    let mut messages = Vec::new();
    for with_target in [true, false] {
        let mut ws = Ws::new()
            .manifest("demo.package.nml", "demo", &[], TENANTS)
            .symlink("tenants/cu/lib", "../../admin/secretdir");
        if with_target {
            ws = ws.file("admin/secretdir/x.flow.nml");
        }
        let root = ws.root();
        let d = ws.discover(&root, vec![]);
        let u = d.universe();
        ws.fs.clear_probes();
        let r = crate::workspace::discover::resolve_file(
            &u,
            Path::new("/ws/tenants/cu/lib/x.flow.nml"),
            &ws.fs,
        )
        .unwrap();
        assert_eq!(r.key.as_str(), "tenants/cu/lib/x.flow.nml");
        assert!(matches!(r.governing, Governing::Unbound));
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].code, Some(codes::SYMLINKED_CONTENT_REJECTED));
        assert!(
            !ws.fs
                .probes()
                .iter()
                .any(|p| matches!(p, crate::workspace::mock::Probe::ResolveSymlink(..)))
        );
        messages.push(r.findings[0].message.clone());
    }
    assert_eq!(messages[0], messages[1]);
    assert!(
        messages[0].contains("path component `lib` is a symlink"),
        "{}",
        messages[0]
    );
    assert!(
        !messages[0].contains("admin"),
        "never the target: {}",
        messages[0]
    );
    // A symlink LEAF: same code, caught at verify.
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("admin/secret.nml")
        .symlink("tenants/cu/leaf.flow.nml", "../../admin/secret.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let r = crate::workspace::discover::resolve_file(
        &u,
        Path::new("/ws/tenants/cu/leaf.flow.nml"),
        &ws.fs,
    )
    .unwrap();
    assert_eq!(r.findings[0].code, Some(codes::SYMLINKED_CONTENT_REJECTED));
    assert!(
        r.findings[0]
            .message
            .contains("`leaf.flow.nml` is a symlink")
    );
    // OPEN universe: the same link is followed and the file binds to
    // nothing (no manifest) with no finding.
    let ws = Ws::new()
        .file("admin/secretdir/x.flow.nml")
        .symlink("tenants/cu/lib", "../../admin/secretdir");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let r = crate::workspace::discover::resolve_file(
        &u,
        Path::new("/ws/tenants/cu/lib/x.flow.nml"),
        &ws.fs,
    )
    .unwrap();
    assert_eq!(r.key.as_str(), "admin/secretdir/x.flow.nml");
    assert!(r.findings.is_empty());
}

#[test]
fn norealpath_closed_is_2083_form2_open_proceeds() {
    // The wasi DENY pin: on a backend without realpath, a respelled
    // lookup under a closed universe is form 2; an exact lookup binds;
    // an open universe proceeds lexically.
    let mut ws = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("all", &["Admin/*.nml"])],
        )
        .file("Admin/s.nml");
    ws.fs = ws
        .fs
        .insensitive()
        .spelling(crate::workspace::mock::Spelling::Membership);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let r =
        crate::workspace::discover::resolve_file(&u, Path::new("/ws/Admin/s.nml"), &ws.fs).unwrap();
    assert!(r.findings.is_empty());
    assert_eq!(bound(&r.governing).1, "all");
    let r =
        crate::workspace::discover::resolve_file(&u, Path::new("/ws/ADMIN/s.nml"), &ws.fs).unwrap();
    assert_eq!(r.findings.len(), 1);
    assert_eq!(r.findings[0].code, Some(codes::SYMLINKED_CONTENT_REJECTED));
    assert!(
        r.findings[0]
            .message
            .contains("cannot verify the on-disk spelling")
    );
    assert!(matches!(r.governing, Governing::Unbound));
    // Open: lexical key, no finding.
    let mut ws = Ws::new().file("Admin/s.nml");
    ws.fs = ws
        .fs
        .insensitive()
        .spelling(crate::workspace::mock::Spelling::Membership);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let r =
        crate::workspace::discover::resolve_file(&u, Path::new("/ws/ADMIN/s.nml"), &ws.fs).unwrap();
    assert_eq!(r.key.as_str(), "ADMIN/s.nml");
    assert!(r.findings.is_empty());
}

#[test]
fn resolve_file_uses_verified_spelling_for_governing() {
    // A11 at the checked file: the parent respells at mint, the leaf at
    // verify; the binding is judged on the FINAL key.
    let mut ws = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("lower", &["admin/s.nml"]), ("upper", &["Admin/s.nml"])],
        )
        .file("Admin/s.nml");
    ws.fs = ws.fs.insensitive();
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let r =
        crate::workspace::discover::resolve_file(&u, Path::new("/ws/ADMIN/S.NML"), &ws.fs).unwrap();
    assert_eq!(r.key.as_str(), "Admin/s.nml");
    assert_eq!(bound(&r.governing).1, "upper");
    // Escapes / absent surface as kernel errors, not findings.
    assert!(matches!(
        crate::workspace::discover::resolve_file(&u, Path::new("/etc/passwd"), &ws.fs).unwrap_err(),
        PathError::Escapes { .. }
    ));
    let r =
        crate::workspace::discover::resolve_file(&u, Path::new("/ws/nope/x.nml"), &ws.fs).unwrap();
    assert_eq!(r.key.as_str(), "nope/x.nml");
}

#[test]
fn manifest_stem_must_match_its_declared_name() {
    // E35 (RFC 0030's file-name rule, arch finding 6): rule 3 keys claims
    // on the DECLARED name, so `evil.package.nml` declaring `name =
    // "demo"` beside the operator's `demo.package.nml` would be a second
    // `demo` ambiguating the operator's. It is a load error naming both
    // — closed-denied like every load error — and the operator's `demo`
    // governs alone; nothing is ambiguous.
    let tenant: &[(&str, &[&str])] = &[("tenantFlows", &["tenants/**/*.flow.nml"])];
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], tenant)
        .text("evil.package.nml", &manifest_text("demo", &[], tenant))
        .file("tenants/cu/x.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(manifest_keys(&d), ["demo.package.nml"]);
    assert_eq!(d.load_errors.len(), 1, "{:?}", d.load_errors);
    let error = &d.load_errors[0];
    assert_eq!(error.source.as_deref(), Some("evil.package.nml"));
    assert_eq!(
        error.message,
        "manifest failed to load: manifest file `evil.package.nml` declares name `demo` — a \
         workspace manifest is `<name>.package.nml` (expected `demo.package.nml`)"
    );
    let u = d.universe();
    assert!(u.is_closed());
    match governing(&u, &key("tenants/cu/x.flow.nml")) {
        Governing::Bound { claimant, .. } => {
            assert_eq!(claimant.claim.manifest_label, "demo.package.nml");
        }
        other => panic!("the operator's manifest governs alone, got {other:?}"),
    }
}

#[test]
fn resolve_file_carries_the_verified_leaf_kind() {
    // E35 (sec 1c): the reader refuses anything but a regular file from
    // the kernel's own verdict, BEFORE any open — so `Resolved` carries
    // what the verified leaf is (`None` = absent).
    let tenant: &[(&str, &[&str])] = &[("tenantFlows", &["tenants/**/*.flow.nml"])];
    let mut ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], tenant)
        .file("tenants/cu/x.flow.nml");
    ws.fs = ws.fs.dir("/ws/tenants/cu/d.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let kind = |p: &str| {
        resolve_file(&u, Path::new(p), &ws.fs)
            .expect("resolves")
            .kind
    };
    assert_eq!(kind("/ws/tenants/cu/x.flow.nml"), Some(EntryKind::File));
    assert_eq!(kind("/ws/tenants/cu/d.flow.nml"), Some(EntryKind::Dir));
    assert_eq!(kind("/ws/tenants/cu/absent.flow.nml"), None);
}

#[test]
fn resolve_file_surfaces_the_symlink_verdict_in_open_universes_only() {
    // r69b (arch r68 finding 3): the walk's symlink verdict reaches the
    // front end. OPEN: a followed link is reported by the index of the
    // first linked component; the key names the target. CLOSED: never a
    // `Through` — the walk halts at the link (NML2083 in `findings`).
    let ws = Ws::new()
        .file("vendor/base.flow.nml")
        .file("tenants/cu/plain.flow.nml")
        .symlink("tenants/cu/lib", "../../vendor")
        .symlink("tenants/cu/leaf.flow.nml", "../../vendor/base.flow.nml");
    let root = ws.root();
    let open = ws.discover(&root, vec![]);
    let u = open.universe();
    assert!(!u.is_closed());
    let via = resolve_file(&u, Path::new("/ws/tenants/cu/lib/base.flow.nml"), &ws.fs).unwrap();
    assert_eq!(via.key.as_str(), "vendor/base.flow.nml");
    assert_eq!(via.via_symlink, SymlinkVerdict::Through(2));
    assert!(via.findings.is_empty());
    let leaf = resolve_file(&u, Path::new("/ws/tenants/cu/leaf.flow.nml"), &ws.fs).unwrap();
    assert_eq!(leaf.via_symlink, SymlinkVerdict::Through(2));
    assert_eq!(leaf.kind, Some(EntryKind::Symlink));
    let plain = resolve_file(&u, Path::new("/ws/tenants/cu/plain.flow.nml"), &ws.fs).unwrap();
    assert_eq!(plain.via_symlink, SymlinkVerdict::None);
    // An absent leaf under a followed link still reports the link.
    let absent = resolve_file(&u, Path::new("/ws/tenants/cu/lib/nope.flow.nml"), &ws.fs).unwrap();
    assert_eq!(
        (absent.kind, absent.via_symlink),
        (None, SymlinkVerdict::Through(2))
    );

    let ws = ws.manifest("demo.package.nml", "demo", &[], TENANTS);
    let root = ws.root();
    let closed = ws.discover(&root, vec![]);
    let u = closed.universe();
    assert!(u.is_closed());
    for p in [
        "/ws/tenants/cu/lib/base.flow.nml",
        "/ws/tenants/cu/leaf.flow.nml",
    ] {
        let r = resolve_file(&u, Path::new(p), &ws.fs).unwrap();
        assert_eq!(r.via_symlink, SymlinkVerdict::None, "{p}");
        assert_eq!(r.findings.len(), 1, "{p}: halted, not followed");
        assert_eq!(r.findings[0].code, Some(codes::SYMLINKED_CONTENT_REJECTED));
    }
}

/// The per-kind read caps are the kernel's contract (r69a; the CLI's
/// reader and the editor's must refuse an oversized input at one size):
/// 256 KiB for a manifest or a project config, 4 MiB for a declared
/// schema source.
#[test]
fn input_caps_are_the_kernels_per_kind_policy() {
    assert_eq!(input_cap(InputKind::Manifest), 256 * 1024);
    assert_eq!(input_cap(InputKind::ProjectConfig), 256 * 1024);
    assert_eq!(input_cap(InputKind::Source), 4 * 1024 * 1024);
}

// ---------------------------------------------------------------------
// r75 — the live-input byte budget is PER UNIT (r74-kernel F1), live
// project configs are charged (F2), the purge covers the live inputs
// settled under a spent unit (F3), operator-level claims delegate
// innermost (F4), an unlistable directory inside a unit is the unit's
// (F5), and the five certification probes the tree's own pins lacked
// (F8).
// ---------------------------------------------------------------------

/// r103-cov: the universe-wide live-input BYTE backstop
/// ([`MAX_TOTAL_LIVE_INPUT_BYTES`]) FIRES. Its entry twin
/// ([`MAX_TOTAL_ENTRIES`]) has a pin; this one had none — the bound is a
/// gigabyte, out of a default-lane test's reach, so deleting the check
/// left every test green and a layout that mints many units could read
/// and hold unbounded text. At mock scale through the byte seam
/// (`discover_byte_scaled`): three tenant units, each spending less than
/// its OWN bound, together cross the universe's — and the walk truncates
/// in FULL, naming the input that crossed it, exactly as the entry
/// backstop does.
#[test]
fn the_universe_wide_byte_backstop_denies_the_whole_walk() {
    const UNIT: usize = 4_096;
    // Room for the operator's own manifest and core source (the ROOT
    // unit's bytes, which count toward the universe's sum too) plus
    // three tenant units at exactly their own bound.
    const TOTAL: usize = 4 * UNIT;
    // Under `tenants/<x>/other/` (a gap of the operator's glob) a
    // manifest is LIVE and charged to its tenant's unit.
    let junk = |n: usize| "# ".to_string() + &"j".repeat(n - 3) + "\n";
    let build = |tenants: &[&str]| {
        let mut ws = Ws::new()
            .manifest("demo.package.nml", "demo", &[], FLOWS_ONLY)
            .file("admin/ops.flow.nml");
        for t in tenants {
            ws = ws
                .file(&format!("tenants/{t}/flows/a.flow.nml"))
                .text(&format!("tenants/{t}/other/m.package.nml"), &junk(UNIT));
        }
        ws
    };
    // Three units at exactly the unit bound each: 3 * 4096 = TOTAL, and
    // the bound is a bound — `>` not `>=` — so the walk finishes.
    let ws = build(&["aa", "bb", "cc"]);
    let root = ws.root();
    let d = ws.discover_byte_scaled(&root, vec![], UNIT, TOTAL);
    assert_eq!(d.truncated, None, "the sum is AT the backstop, not past it");
    assert!(d.truncated_units.is_empty(), "{:?}", d.truncated_units);
    // A fourth unit crosses the universe's bound while every unit stays
    // within its own: the WHOLE walk truncates, never one unit.
    let ws = build(&["aa", "bb", "cc", "dd"]);
    let root = ws.root();
    let d = ws.discover_byte_scaled(&root, vec![], UNIT, TOTAL);
    let Some(Truncation::TotalLiveInputBytes { key: spent }) = &d.truncated else {
        panic!("not the universe-wide byte backstop: {:?}", d.truncated);
    };
    assert_eq!(spent.file_name(), "m.package.nml");
    assert!(
        d.truncated_units.is_empty(),
        "the backstop denies the universe, never a unit: {:?}",
        d.truncated_units
    );
    let u = d.universe();
    assert_eq!(u.closure, Closure::Truncated);
    assert!(u.is_closed());
    assert!(d.claims.is_empty() && d.files.is_empty());
    assert!(matches!(
        governing(&u, &key("admin/ops.flow.nml")),
        Governing::Unbound
    ));
    let error = d.truncation_error().expect("loud");
    assert_eq!(error.severity, nml_core::diagnostic::Severity::Error);
    assert_eq!(error.code, Some(codes::UNIVERSE_TRUNCATED));
    assert_eq!(error.source.as_deref(), Some(spent.as_str()));
    // The sentence is the one the full-size bound prints (the row the
    // `diag` pin renders), naming the universe-wide total.
    assert!(
        error.message.contains(&format!(
            "the universe-wide live-input budget ({MAX_TOTAL_LIVE_INPUT_BYTES} bytes, every \
             budget unit summed) was spent reading `{spent}`"
        )),
        "{}",
        error.message
    );
}

#[test]
fn a_tenants_live_inputs_spend_only_its_own_byte_budget() {
    // r75 (r74-kernel F1). Sixteen distinct 4 MiB sources declared by
    // live tenant manifests cross the tenant's OWN 64 MiB: the unit is
    // denied in full — its live manifests purged with it — while the
    // sibling tenant and the operator's file bind in the same walk, the
    // universe intact. (The r73 tree, certified in r74, denied the WHOLE
    // universe here: the byte budget was the one bound still charged
    // per universe.)
    let big: String = "# ".to_string() + &"x".repeat(4 * 1024 * 1024) + "\n";
    let mut ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], FLOWS_ONLY)
        .file("tenants/cu/flows/a.flow.nml")
        .file("tenants/du/flows/b.flow.nml")
        .file("admin/ops.flow.nml");
    for i in 0..17 {
        ws = ws
            .text(&format!("tenants/cu/other/v{i:02}/core.model.nml"), &big)
            .text(
                &format!("tenants/cu/other/v{i:02}/m{i:02}.package.nml"),
                &manifest_text(&format!("m{i:02}"), &[], &[("b", &["never/*.q.nml"])]),
            );
    }
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None, "the universe is intact");
    assert_eq!(d.truncated_units.len(), 1, "{:?}", d.truncated_units);
    let t = &d.truncated_units[0];
    assert_eq!(t.unit, key("tenants/cu"));
    assert_eq!(t.why, UnitBound::LiveInputBytes);
    assert!(
        t.stop.as_str().starts_with("tenants/cu/other/v") && t.stop.file_name() == "core.model.nml",
        "the stop is the INPUT that crossed the bound: {}",
        t.stop
    );
    assert_eq!(
        manifest_keys(&d),
        ["demo.package.nml"],
        "the tenant's live manifests are purged with the unit"
    );
    assert!(d.load_errors.is_empty() && d.inert.is_empty());
    let u = d.universe();
    assert!(u.is_closed());
    assert_eq!(u.closure, Closure::Complete);
    assert_eq!(u.workspace_claims(), 1);
    assert!(d.universe_errors().is_empty());
    // DENIED: the tenant, with the byte-budget sentence on its key.
    assert!(matches!(
        governing(&u, &key("tenants/cu/flows/a.flow.nml")),
        Governing::Unbound
    ));
    let f = findings_of(&u, &ws.fs, "/ws/tenants/cu/flows/a.flow.nml");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].code, Some(codes::UNIVERSE_TRUNCATED));
    assert!(
        f[0].message
            .contains("live-input budget for this subtree was spent reading it"),
        "{}",
        f[0].message
    );
    // UNAFFECTED: the sibling and the operator.
    assert_eq!(
        bound(&governing(&u, &key("tenants/du/flows/b.flow.nml"))).1,
        "tenantFlows"
    );
    assert_eq!(bound(&governing(&u, &key("admin/ops.flow.nml"))).1, "ops");
    assert!(!files(&d).iter().any(|f| f.starts_with("tenants/cu/")));
    assert!(files(&d).contains(&"tenants/du/flows/b.flow.nml".to_string()));
}

#[test]
fn a_units_byte_budget_admits_256_maximal_texts_and_denies_the_257th() {
    // r75. The manifest's own text is charged BEFORE it is parsed, so
    // 256 KiB junk texts spend the budget without a single source —
    // 256 of them are exactly 64 MiB and fit (each a plain NML2088 the
    // unit carries on around); the 257th crosses it: the unit is denied,
    // its 256 load errors are purged with it, and the sibling binds.
    let junk: String = "# ".to_string() + &"j".repeat(256 * 1024 - 3) + "\n";
    assert_eq!(junk.len(), 256 * 1024);
    assert_eq!(256 * junk.len(), MAX_LIVE_INPUT_BYTES);
    let build = |n: usize| {
        let mut ws = Ws::new()
            .manifest("demo.package.nml", "demo", &[], FLOWS_ONLY)
            .file("tenants/du/flows/b.flow.nml");
        for i in 0..n {
            ws = ws.text(&format!("tenants/cu/other/m{i:03}.package.nml"), &junk);
        }
        ws
    };
    let ws = build(256);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    assert!(d.truncated_units.is_empty(), "{:?}", d.truncated_units);
    assert_eq!(
        d.load_errors.len(),
        256,
        "exactly the bound is not past it; each junk manifest is its own NML2088"
    );
    let ws = build(257);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    assert_eq!(
        d.truncated_units,
        [UnitTruncation {
            unit: key("tenants/cu"),
            stop: key("tenants/cu/other/m256.package.nml"),
            why: UnitBound::LiveInputBytes,
        }]
    );
    assert!(
        d.load_errors.is_empty(),
        "purged with the unit: {} load errors survived",
        d.load_errors.len()
    );
    let u = d.universe();
    assert_eq!(u.closure, Closure::Complete);
    assert_eq!(
        bound(&governing(&u, &key("tenants/du/flows/b.flow.nml"))).1,
        "tenantFlows"
    );
}

/// The unit's byte bound is a BOUND, to the byte: a unit whose live
/// inputs weigh EXACTLY it is admitted, and one byte more denies it.
/// The full-size pin beside this one
/// (`a_units_byte_budget_admits_256_maximal_texts_and_denies_the_257th`)
/// steps in 256 KiB manifests, so it cannot see a bound that moved by
/// one — measured: widening the charge by a single byte left it green.
/// At mock scale through the byte seam the step IS one byte.
#[test]
fn a_units_byte_budget_admits_the_bound_exactly_and_denies_one_byte_past_it() {
    const UNIT: usize = 4_096;
    // Far above the unit's, so the BACKSTOP never speaks for the unit
    // (it wins when one input crosses both).
    const TOTAL: usize = 64 * UNIT;
    let junk = |n: usize| "# ".to_string() + &"j".repeat(n - 3) + "\n";
    let build = |bytes: usize| {
        Ws::new()
            .manifest("demo.package.nml", "demo", &[], FLOWS_ONLY)
            .file("tenants/du/flows/b.flow.nml")
            .text("tenants/cu/other/m.package.nml", &junk(bytes))
    };
    let ws = build(UNIT);
    assert_eq!(junk(UNIT).len(), UNIT, "the text weighs exactly the bound");
    let root = ws.root();
    let d = ws.discover_byte_scaled(&root, vec![], UNIT, TOTAL);
    assert_eq!(d.truncated, None);
    assert!(
        d.truncated_units.is_empty(),
        "AT the bound is not past it: {:?}",
        d.truncated_units
    );
    assert_eq!(
        d.load_errors.len(),
        1,
        "the junk manifest is its own NML2088"
    );

    let ws = build(UNIT + 1);
    let root = ws.root();
    let d = ws.discover_byte_scaled(&root, vec![], UNIT, TOTAL);
    assert_eq!(d.truncated, None, "a UNIT is denied, never the universe");
    assert_eq!(
        d.truncated_units,
        [UnitTruncation {
            unit: key("tenants/cu"),
            stop: key("tenants/cu/other/m.package.nml"),
            why: UnitBound::LiveInputBytes,
        }],
        "one byte past the bound denies the unit"
    );
    let u = d.universe();
    assert_eq!(u.closure, Closure::Complete);
    assert_eq!(
        bound(&governing(&u, &key("tenants/du/flows/b.flow.nml"))).1,
        "tenantFlows",
        "the sibling unit is untouched"
    );
}

#[test]
fn live_project_configs_are_charged_to_their_units_byte_budget() {
    // r75 (r74-kernel F2). A live `nml-project.nml` is read through the
    // same capped reader as a manifest and was never charged: 300 of
    // them at 256 KiB (75 MiB) were all read and kept. Now the 257th
    // spends the tenant's unit — the configs read so far are purged with
    // it, the rest are never read — and under the root unit the same
    // flood is the whole universe, exactly like a manifest's bytes.
    let prefix = "project p:\n    autoAssociate = false\n# ";
    let junk: String = prefix.to_string() + &"c".repeat(256 * 1024 - prefix.len() - 1) + "\n";
    assert_eq!(junk.len(), 256 * 1024);
    let mut ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], FLOWS_ONLY)
        .file("tenants/du/flows/b.flow.nml");
    for i in 0..300 {
        ws = ws.text(&format!("tenants/cu/other/d{i:03}/nml-project.nml"), &junk);
    }
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    assert_eq!(
        d.truncated_units,
        [UnitTruncation {
            unit: key("tenants/cu"),
            stop: key("tenants/cu/other/d256/nml-project.nml"),
            why: UnitBound::LiveInputBytes,
        }]
    );
    assert!(d.configs.is_empty(), "purged with the unit");
    assert_eq!(
        ws.read_keys()
            .iter()
            .filter(|k| k.ends_with("nml-project.nml"))
            .count(),
        257,
        "the crossing read is the last one under the unit"
    );
    let u = d.universe();
    assert_eq!(
        bound(&governing(&u, &key("tenants/du/flows/b.flow.nml"))).1,
        "tenantFlows"
    );
    // The root unit: the same flood in content no glob reaches is the
    // whole universe, closed-denied in full.
    let mut ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], FLOWS_ONLY)
        .file("tenants/du/flows/b.flow.nml");
    for i in 0..300 {
        ws = ws.text(&format!("vendor/d{i:03}/nml-project.nml"), &junk);
    }
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let Some(Truncation::LiveInputBytes { key: spent }) = &d.truncated else {
        panic!("not a byte-budget truncation: {:?}", d.truncated);
    };
    assert_eq!(spent.file_name(), "nml-project.nml");
    assert!(spent.as_str().starts_with("vendor/d"), "{spent}");
    assert!(d.truncated_units.is_empty() && d.configs.is_empty() && d.claims.is_empty());
    let u = d.universe();
    assert_eq!(u.closure, Closure::Truncated);
    assert!(matches!(
        governing(&u, &key("tenants/du/flows/b.flow.nml")),
        Governing::Unbound
    ));
}

#[test]
fn a_live_tenant_input_settled_before_its_unit_is_spent_is_purged() {
    // r75 (r74-kernel F3). Under `FLOWS_ONLY` a tenant manifest and a
    // tenant config in `tenants/cu/other/` are LIVE and settled at key
    // depth 4, two rounds before the spam at depth 6 spends the unit.
    // They were read (their bytes charged) — and they are purged with
    // the unit: not a claim, not a config, not counted by
    // `workspace_claims()` (the CLI row's `manifests`). Pre-r75 both
    // survived, and the doc on `truncated_units` overclaimed.
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], FLOWS_ONLY)
        .manifest(
            "tenants/cu/other/own.package.nml",
            "own",
            &[],
            &[("all", &["**"])],
        )
        .config(
            "tenants/cu/other/nml-project.nml",
            "    autoAssociate = false\n",
        )
        .file("tenants/cu/flows/a.flow.nml")
        .file("tenants/cu/other/x.nml")
        .file("tenants/du/flows/b.flow.nml")
        .file("admin/ops.flow.nml");
    let ws = spam(ws, "tenants/cu/other/deep/spam", MAX_ENTRIES + 1);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated_units,
        [UnitTruncation {
            unit: key("tenants/cu"),
            stop: key("tenants/cu/other/deep/spam"),
            why: UnitBound::Entries,
        }]
    );
    assert!(
        ws.read_keys()
            .contains(&"tenants/cu/other/own.package.nml".to_string()),
        "live, so read before the bound: {:?}",
        ws.read_keys()
    );
    assert_eq!(
        manifest_keys(&d),
        ["demo.package.nml"],
        "purged with the unit"
    );
    assert!(d.configs.is_empty(), "so is its config");
    let u = d.universe();
    assert_eq!(u.workspace_claims(), 1);
    assert!(u.is_closed());
    assert!(!files(&d).iter().any(|f| f.starts_with("tenants/cu/")));
    assert!(matches!(
        governing(&u, &key("tenants/cu/other/x.nml")),
        Governing::Unbound
    ));
    assert_eq!(
        bound(&governing(&u, &key("tenants/du/flows/b.flow.nml"))).1,
        "tenantFlows"
    );
    assert_eq!(bound(&governing(&u, &key("admin/ops.flow.nml"))).1, "ops");
}

#[test]
fn a_catch_all_root_glob_leaves_the_operator_level_tenant_unit_in_place() {
    // r75 (r74-kernel F4). `demo` claims `tenants/**/*.flow.nml` (units
    // `tenants/<x>`); a second root manifest `all` claims
    // `**/*.model.nml` (every top-level directory a unit, `tenants`
    // included). Plain outermost attribution made `tenants` the unit,
    // so `cu`'s spam denied `du` — per-tenant isolation collapsed to
    // per-top-level-directory. Attribution is now INNERMOST among the
    // unit roots OPERATOR-LEVEL claims induce (both manifests sit under
    // no unit): `tenants/cu` is the unit, `du` binds, and `admin` keeps
    // its own unit. The security half stands: a TENANT's own live
    // manifest inside a unit is not operator-level, so its unit roots
    // buy it nothing (`nested_units_are_charged_to_the_outermost`).
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .manifest(
            "all.package.nml",
            "all",
            &[],
            &[("models", &["**/*.model.nml"])],
        )
        .file("tenants/cu/plain.flow.nml")
        .file("tenants/du/plain.flow.nml")
        .file("admin/ops.model.nml");
    let ws = spam(ws, "tenants/cu/spam", MAX_ENTRIES + 1);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated_units,
        [UnitTruncation {
            unit: key("tenants/cu"),
            stop: key("tenants/cu/spam"),
            why: UnitBound::Entries,
        }],
        "the tenant, not `tenants`"
    );
    let u = d.universe();
    assert!(u.is_closed());
    let f = findings_of(&u, &ws.fs, "/ws/tenants/cu/plain.flow.nml");
    assert!(
        f[0].message
            .contains("the discovery budget for `tenants/cu` is exhausted"),
        "{:?}",
        f
    );
    assert_eq!(
        bound(&governing(&u, &key("tenants/du/plain.flow.nml"))).1,
        "tenantFlows"
    );
    assert_eq!(bound(&governing(&u, &key("admin/ops.model.nml"))).0, "all");
    assert!(files(&d).contains(&"tenants/du/plain.flow.nml".to_string()));
}

#[test]
fn an_unlistable_directory_inside_a_unit_denies_the_unit_alone() {
    // r75 (r74-kernel F5). A directory a tenant makes unlistable inside
    // its own unit denied the WHOLE universe (`Truncation::Unreadable`
    // was not unit-scoped): now the unit is denied with the unlistable
    // directory as its stop, the sibling tenant binds, and at the root
    // unit it is still the whole universe
    // (`truncated_discovery_is_closed_denied`).
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/plain.flow.nml")
        .file("tenants/du/plain.flow.nml")
        .file("admin/ops.flow.nml")
        .denied("tenants/cu/locked");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    assert_eq!(
        d.truncated_units,
        [UnitTruncation {
            unit: key("tenants/cu"),
            stop: key("tenants/cu/locked"),
            why: UnitBound::Unreadable(FsError::Denied),
        }]
    );
    let u = d.universe();
    assert!(u.is_closed());
    assert_eq!(u.closure, Closure::Complete);
    assert!(d.universe_errors().is_empty());
    assert!(matches!(
        governing(&u, &key("tenants/cu/plain.flow.nml")),
        Governing::Unbound
    ));
    let f = findings_of(&u, &ws.fs, "/ws/tenants/cu/plain.flow.nml");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].code, Some(codes::UNIVERSE_TRUNCATED));
    assert!(
        f[0].message.contains(
            "discovery under `tenants/cu` was cut short: the walk stopped at \
             `tenants/cu/locked` (unreadable: permission denied on a path component)"
        ),
        "{}",
        f[0].message
    );
    assert_eq!(
        bound(&governing(&u, &key("tenants/du/plain.flow.nml"))).1,
        "tenantFlows"
    );
    assert!(d.universe_notes().is_empty());
    assert!(
        d.inert_notes_for(&key("tenants/du/plain.flow.nml"))
            .is_empty()
    );
    assert_eq!(
        files(&d),
        [
            "core.model.nml",
            "demo.package.nml",
            "admin/ops.flow.nml",
            "tenants/du/plain.flow.nml"
        ]
    );
}

/// r105-cov: `unit_errors_under` is a gate over DIRECTORY arguments, and
/// its containment is REFLEXIVE: the operator naming the denied unit's
/// own directory (`nml check tenants/cu`) gets its row, as the root and
/// any ancestor do; a sibling, an unrelated directory and a name that is
/// merely a prefix get none; and two arguments that both contain the
/// unit yield ONE row, never one per argument. The CLI pin ran only the
/// root and a sibling, so a strict-ancestry mutant
/// (`is_strict_ancestor_of`), which silences exactly the unit's own
/// directory, survived it.
#[test]
fn unit_errors_under_answers_the_units_own_directory_and_never_twice() {
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/plain.flow.nml")
        .file("tenants/du/plain.flow.nml")
        .denied("tenants/cu/locked");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated_units.len(), 1, "{:?}", d.truncated_units);
    let row = shown(&d.unit_errors());
    assert_eq!(row.len(), 1, "{row:?}");
    assert!(
        row[0].contains("NML2089") && row[0].contains("tenants/cu"),
        "{row:?}"
    );
    let rows = |dirs: &[SourceKey]| shown(&d.unit_errors_under(dirs));
    for dirs in [
        vec![SourceKey::root()],
        vec![key("tenants")],
        vec![key("tenants/cu")],
        vec![SourceKey::root(), key("tenants/cu")],
        vec![key("tenants/du"), key("tenants/cu")],
    ] {
        assert_eq!(rows(&dirs), row, "{dirs:?}");
    }
    for dirs in [
        vec![],
        vec![key("tenants/du")],
        vec![key("admin")],
        vec![key("tenants/c")],
        vec![key("tenants/cu/locked")],
    ] {
        assert!(rows(&dirs).is_empty(), "{dirs:?}: {:?}", rows(&dirs));
    }
}

/// r85 D8: a target INSIDE the unreadable directory resolves to the
/// unit's row too — answered on the LEXICAL key before any probe, so
/// nothing under `locked` is `lstat`ed (the mock records every probe)
/// and the OS's EACCES never becomes a bare error (it exited 1 in
/// `check` and 2 in `binding` with the unit's row unspoken). A `..`
/// spelling that pops back into the unit is the same answer.
#[test]
fn a_target_inside_an_unreadable_unit_gets_the_units_row_without_a_probe() {
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/plain.flow.nml")
        .file("tenants/du/plain.flow.nml")
        .denied("tenants/cu/locked");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    ws.fs.clear_probes();
    for path in [
        "/ws/tenants/cu/locked/sub/l.flow.nml",
        "/ws/tenants/cu/locked/../locked/l.flow.nml",
        "/ws/tenants/cu/plain.flow.nml",
    ] {
        let r = crate::workspace::discover::resolve_file(&u, Path::new(path), &ws.fs)
            .unwrap_or_else(|e| panic!("{path}: {e:?}"));
        assert_eq!(r.findings.len(), 1, "{path}: {:?}", r.findings);
        assert_eq!(
            r.findings[0].code,
            Some(codes::UNIVERSE_TRUNCATED),
            "{path}"
        );
        assert!(
            r.findings[0]
                .message
                .contains("discovery under `tenants/cu` was cut short"),
            "{path}: {}",
            r.findings[0].message
        );
        assert_eq!(r.kind, None, "{path}: never probed");
    }
    let probes = ws.fs.probes();
    assert!(
        !probes
            .iter()
            .any(|p| format!("{p:?}").contains("/ws/tenants/cu/locked")),
        "probed under the denied unit: {probes:?}"
    );
    // The sibling tenant still resolves and binds.
    let r = crate::workspace::discover::resolve_file(
        &u,
        Path::new("/ws/tenants/du/plain.flow.nml"),
        &ws.fs,
    )
    .expect("keyed");
    assert!(r.findings.is_empty(), "{:?}", r.findings);
    assert!(matches!(r.governing, Governing::Bound { .. }));
}

/// r85 (r84-sec F3): the unit question charges a probe only for an
/// operator-level claim whose manifest directory CONTAINS the listed
/// directory — `is_budget_unit` answers `false` for every other claim
/// at once, so the probe was pure work. Ten operator-level manifests in
/// top-level subtrees no other glob reaches (`m<i>/`, settled after the
/// first round) used to cost every directory listed in the NEXT round
/// eleven charged probes — ten of them for manifests that could never
/// contain it: at a 128-entry unit the root unit was denied (~186
/// entries charged) — the whole universe, from a handful of committed
/// entries. Now each listing charges only its containing claims (~96)
/// and the walk completes; the verdicts are byte-identical (every `m<i>`
/// manifest still claims its own files).
#[test]
fn the_unit_question_charges_only_containing_claims() {
    const UNIT: usize = 128;
    const TOTAL: usize = 16 * UNIT;
    let mut ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/plain.flow.nml")
        .file("tenants/du/plain.flow.nml");
    for i in 0..10 {
        ws = ws
            .manifest(
                &format!("m{i:02}/own.package.nml"),
                "own",
                &[],
                &[("mine", &["sub/*.flow.nml"])],
            )
            .file(&format!("m{i:02}/sub/a.flow.nml"));
    }
    let root = ws.root();
    let d = ws.discover_scaled(&root, vec![], UNIT, TOTAL);
    assert_eq!(d.truncated, None, "{:?}", d.truncated);
    assert!(d.truncated_units.is_empty(), "{:?}", d.truncated_units);
    assert_eq!(d.claims.len(), 11);
    let u = d.universe();
    assert_eq!(
        bound(&governing(&u, &key("tenants/cu/plain.flow.nml"))).1,
        "tenantFlows"
    );
    assert_eq!(bound(&governing(&u, &key("m07/sub/a.flow.nml"))).1, "mine");
}

#[test]
fn guard_drops_same_round_siblings_of_a_spent_unit() {
    // r74-kernel F8 (probe adopted r75). Tenant cu spreads its entries
    // over sibling directories at ONE depth: a/ 40,000, b/ 30,000, c/
    // 10. The unit crosses its bound inside b; c is already in this
    // round's frontier and must be dropped UNLISTED (the guard at the
    // top of the listing loop) — the sibling tenant is listed as usual.
    // A pin for the guard, which r73 found by measuring and no pin
    // caught (mutant M5 changed no verdict, only what was listed).
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/plain.flow.nml")
        .file("tenants/du/plain.flow.nml");
    let ws = spam(ws, "tenants/cu/a", 40_000);
    let ws = spam(ws, "tenants/cu/b", 30_000);
    let ws = spam(ws, "tenants/cu/c", 10);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated_units,
        [UnitTruncation {
            unit: key("tenants/cu"),
            stop: key("tenants/cu/b"),
            why: UnitBound::Entries,
        }]
    );
    let l = listed(&ws);
    assert!(l.contains(&"/ws/tenants/cu/a".to_string()), "{l:?}");
    assert!(l.contains(&"/ws/tenants/cu/b".to_string()), "{l:?}");
    assert!(
        !l.contains(&"/ws/tenants/cu/c".to_string()),
        "the same-round sibling of a spent unit must not be listed: {l:?}"
    );
    assert!(l.contains(&"/ws/tenants/du".to_string()), "{l:?}");
    let u = d.universe();
    assert_eq!(
        bound(&governing(&u, &key("tenants/du/plain.flow.nml"))).1,
        "tenantFlows"
    );
    assert!(!files(&d).iter().any(|f| f.starts_with("tenants/cu")));
}

#[test]
#[ignore = "perf tier: run with `cargo test -p nml-validate --release --lib -- --ignored perf_` \
            (≈1.05 M scripted nodes, 12 s in release, r74-kernel)"]
fn perf_backstop_boundary_and_a_within_budget_tenant_tips_the_universe() {
    // r74-kernel F8 (probe adopted r75). 15 tenants each at EXACTLY
    // their unit budget (65,536 — the spam entry, the unit-root probe
    // `spam`'s listing asks of the operator's claim, and 65,534 files;
    // none exhausted) plus the root unit's 19 entries (3 at the root,
    // 16 tenant dirs) and its 17 unit-root probes (`tenants` and each
    // `tenants/tNN` listing asks the one operator claim once) =
    // 983,076. Tenant t15 within its own budget: with K = 65,498 spam
    // files the total is exactly MAX_TOTAL_ENTRIES and the universe is
    // whole; ONE more file — t15 still 36 entries under its own bound —
    // and the backstop denies the whole universe, every tenant and the
    // operator. (The lane's own backstop pin overshoots the bound by
    // 16: an off-by-one there is caught only here; the mock-scaled
    // twin `the_backstop_is_exact_at_mock_scale` runs this arithmetic
    // in the default lane.)
    const FULL: usize = MAX_ENTRIES - 2;
    let mut ws = Ws::new().manifest("demo.package.nml", "demo", &[], TENANTS);
    for t in 0..15 {
        for i in 0..FULL {
            ws.fs = ws.fs.file(&format!("/ws/tenants/t{t:02}/spam/f{i}"));
        }
    }
    let root_unit = 3 + 16 + 17;
    let k = MAX_TOTAL_ENTRIES - root_unit - 15 * MAX_ENTRIES - 2;
    assert_eq!(k, 65_498);
    for i in 0..k {
        ws.fs = ws.fs.file(&format!("/ws/tenants/t15/spam/f{i}"));
    }
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated, None,
        "total == MAX_TOTAL_ENTRIES is not past it"
    );
    assert!(
        d.truncated_units.is_empty(),
        "no unit is past its own bound"
    );
    assert_eq!(d.claims.len(), 1);
    // One more entry in t15 — 65,501 in a 65,536 unit — and the backstop
    // is the whole universe.
    ws.fs = ws.fs.file(&format!("/ws/tenants/t15/spam/f{k}"));
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated,
        Some(Truncation::Entries {
            dir: key("tenants/t15/spam")
        })
    );
    assert!(d.truncated_units.is_empty());
    let u = d.universe();
    assert!(matches!(
        governing(&u, &key("tenants/t00/plain.flow.nml")),
        Governing::Unbound
    ));
}

#[test]
fn unit_bound_is_exact_at_65536() {
    // r74-kernel F8 (probe adopted r75). The unit's spend is the `spam`
    // entry in the tenant's own listing, the ONE unit-root probe the
    // listing of `tenants/cu/spam` asks of the operator's claim
    // (settlement probes are entries), and its files: 65,534 files + 2
    // = 65,536 = MAX_ENTRIES is NOT past the bound; one more is. (The
    // tree's other pins use 65,537 files plus two or three sibling
    // entries, so an off-by-one in either direction would not turn them
    // red — this one does.)
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/du/plain.flow.nml");
    let mut ws = spam(ws, "tenants/cu/spam", MAX_ENTRIES - 2);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(d.truncated_units.is_empty(), "{:?}", d.truncated_units);
    assert_eq!(d.truncated, None);
    assert_eq!(files(&d).len(), 3 + MAX_ENTRIES - 2);
    ws.fs = ws
        .fs
        .file(&format!("/ws/tenants/cu/spam/f{}", MAX_ENTRIES - 2));
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated_units,
        [UnitTruncation {
            unit: key("tenants/cu"),
            stop: key("tenants/cu/spam"),
            why: UnitBound::Entries,
        }]
    );
}

#[test]
fn purge_covers_files_and_notes_seen_rounds_before_the_bound() {
    // r74-kernel F8 (probe adopted r75). Files seen two and three rounds
    // before the unit is spent, an inert tenant config and an inert
    // nested manifest (each an NML2080 note when seen) — all gone from
    // `files` and `inert` once the unit is truncated at depth 6.
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/plain.flow.nml")
        .file("tenants/cu/nested/deeper.flow.nml")
        .file("tenants/cu/nested/x/deepest.flow.nml")
        .config("tenants/cu/nml-project.nml", "    autoAssociate = false\n")
        .manifest(
            "tenants/cu/nested/evil.package.nml",
            "evil",
            &[],
            &[("all", &["**"])],
        )
        .file("tenants/du/plain.flow.nml");
    let ws = spam(ws, "tenants/cu/nested/x/y/spam", MAX_ENTRIES + 1);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated_units.len(), 1);
    assert_eq!(d.truncated_units[0].stop, key("tenants/cu/nested/x/y/spam"));
    assert!(d.inert.is_empty(), "{:?}", inert_keys(&d));
    assert!(d.load_errors.is_empty());
    assert_eq!(
        files(&d),
        [
            "core.model.nml",
            "demo.package.nml",
            "tenants/du/plain.flow.nml"
        ]
    );
    assert_eq!(ws.read_keys(), ["demo.package.nml", "core.model.nml"]);
    assert!(d.universe_notes().is_empty());
    assert!(
        d.inert_notes_for(&key("tenants/du/plain.flow.nml"))
            .is_empty()
    );
}

#[test]
fn inert_manifest_inside_a_unit_under_the_canonical_glob_is_never_read() {
    // r74-kernel F8 (probe adopted r75). Under `tenants/**/*.flow.nml`
    // every directory under a tenant is reached, so a tenant manifest
    // anywhere in the unit is inert: never read, never a claim, and
    // irrelevant to the unit logic.
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .manifest("tenants/cu/cu.package.nml", "cu", &[], &[("all", &["**"])])
        .manifest(
            "tenants/cu/deep/er/cu2.package.nml",
            "cu2",
            &[],
            &[("all", &["**"])],
        )
        .file("tenants/du/plain.flow.nml");
    let ws = spam(ws, "tenants/cu/spam", MAX_ENTRIES + 1);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(manifest_keys(&d), ["demo.package.nml"]);
    assert_eq!(ws.read_keys(), ["demo.package.nml", "core.model.nml"]);
    assert_eq!(d.truncated_units.len(), 1);
    assert_eq!(d.truncated_units[0].unit, key("tenants/cu"));
    assert!(d.inert.is_empty());
}

#[test]
fn a_tenant_manifest_in_a_gap_of_the_operators_glob_mints_no_budget_unit() {
    // r77 (r76 F1 (c), the certifier's two FINDING probes adopted). Under
    // `tenants/*/flows/**` the tenant's `other/` is root-unit content,
    // and pre-r77 a live manifest there was "operator-level" (its
    // directory under no unit): the walk let it MINT unit roots inside
    // its own subtree, each with a full 65,536-entry and 64 MiB budget —
    // two spam directories were two unit truncations, and 45 planted
    // 4 MiB sources (180 MiB) were read and held with the universe
    // `Complete` and every file bound. Operator-level now means "no
    // strictly shallower live claim reaches a strict ancestor of the
    // manifest's directory"; `tenants/cu` IS reached, so the manifest
    // mints nothing and its subtree is the root unit's — loud, as E38's
    // residual says: spam under its would-be unit is the universe's
    // entry bound, and its sources spend the root unit's bytes at the
    // sixteenth 4 MiB read (the manifests' own bytes tip it over).
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], FLOWS_DEEP)
        .file("tenants/cu/flows/a.flow.nml")
        .file("tenants/du/flows/b.flow.nml")
        .file("admin/ops.flow.nml");
    let ws = minting_tenant(ws);
    let ws = spam(ws, "tenants/cu/other/z0/deep/w/spam", MAX_ENTRIES + 1);
    let ws = spam(ws, "tenants/cu/other/z1/deep/w/spam", MAX_ENTRIES + 1);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated,
        Some(Truncation::Entries {
            dir: key("tenants/cu/other/z0/deep/w/spam")
        }),
        "the root unit's bound, not a minted unit's"
    );
    assert!(d.truncated_units.is_empty());
    let u = d.universe();
    assert_eq!(u.closure, Closure::Truncated);
    assert!(matches!(
        governing(&u, &key("tenants/du/flows/b.flow.nml")),
        Governing::Unbound
    ));
    // The walk stopped: the second spam directory was never listed.
    assert!(
        !listed(&ws).iter().any(|l| l.ends_with("/z1/deep/w/spam")),
        "{:?}",
        listed(&ws)
    );

    // The byte half: distinct 4 MiB sources under the would-be units
    // spend the ROOT unit's budget at the sixteenth read; nothing more
    // is read, and the universe is denied in full.
    let big: String = "# ".to_string() + &"x".repeat(4 * 1024 * 1024) + "\n";
    let mut ws = minting_tenant(
        Ws::new()
            .manifest("demo.package.nml", "demo", &[], FLOWS_DEEP)
            .file("tenants/cu/flows/a.flow.nml")
            .file("tenants/du/flows/b.flow.nml")
            .file("admin/ops.flow.nml"),
    );
    for i in 0..3 {
        for j in 0..15 {
            let dir = format!("tenants/cu/other/z{i}/deep/w/side/s{j:02}");
            ws = ws
                .manifest(
                    &format!("{dir}/m{i}x{j:02}.package.nml"),
                    &format!("m{i}x{j:02}"),
                    &[],
                    &[("b", &["never/*.q.nml"])],
                )
                .text(&format!("{dir}/core.model.nml"), &big);
        }
    }
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(
        matches!(d.truncated, Some(Truncation::LiveInputBytes { .. })),
        "{:?}",
        d.truncated
    );
    let big_reads = ws
        .read_keys()
        .iter()
        .filter(|k| k.contains("/side/s") && k.ends_with("core.model.nml"))
        .count();
    assert_eq!(
        big_reads, 16,
        "the root unit's budget, spent at the sixteenth source"
    );
    assert!(d.truncated_units.is_empty() && d.claims.is_empty());
    assert_eq!(d.universe().closure, Closure::Truncated);
}

#[test]
fn an_operators_manifest_below_a_gap_in_its_own_glob_mints_nothing_either() {
    // r77 (r76 F1 (c), the caveat recorded for the owner). The
    // operator-level test cannot tell an operator's second manifest
    // from a tenant's: `apps/svc/config/svc.package.nml` under the
    // operator's own `apps/*/flows/**` sits below `apps/svc`, which the
    // glob reaches, so it is not operator-level either and its `**`
    // mints no unit — spam under `apps/svc/config/t/` is the root
    // unit's: the universe, loud, the safe direction. An operator who
    // wants that manifest to delegate needs item 4's explicit spelling.
    let ws = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("appFlows", &["apps/*/flows/**"])],
        )
        .manifest(
            "apps/svc/config/svc.package.nml",
            "svc",
            &[],
            &[("all", &["**"])],
        )
        .file("apps/svc/flows/a.flow.nml")
        .file("apps/web/flows/b.flow.nml");
    let ws = spam(ws, "apps/svc/config/t/spam", MAX_ENTRIES + 1);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated,
        Some(Truncation::Entries {
            dir: key("apps/svc/config/t/spam")
        })
    );
    assert!(d.truncated_units.is_empty());
    // Whereas a manifest in UNREACHED content stays operator-level: the
    // same manifest at `services/` (no glob reaches `services`) mints
    // `services/<t>` under its `*/**`, and spam there is that unit's
    // alone — the universe whole, the operator's file bound.
    let ws = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("appFlows", &["apps/*/flows/**"])],
        )
        .manifest(
            "services/svc.package.nml",
            "svc",
            &[],
            &[("all", &["*/**"])],
        )
        .file("apps/svc/flows/a.flow.nml")
        .file("services/x/y.nml");
    let ws = spam(ws, "services/t/spam", MAX_ENTRIES + 1);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None, "{:?}", d.truncated);
    assert_eq!(
        d.truncated_units,
        [UnitTruncation {
            unit: key("services/t"),
            stop: key("services/t/spam"),
            why: UnitBound::Entries,
        }]
    );
    let u = d.universe();
    assert_eq!(
        bound(&governing(&u, &key("apps/svc/flows/a.flow.nml"))).1,
        "appFlows"
    );
    assert_eq!(bound(&governing(&u, &key("services/x/y.nml"))).1, "all");
}

#[test]
fn a_tenant_manifest_two_levels_below_the_reached_directory_mints_nothing() {
    // r79 (r78 pin depth). The r77 pins put the tenant's manifest ONE
    // level below the reached directory, so a walk asking only the
    // PARENT of the manifest's directory passed them and would let
    // this one mint: `tenants/cu/other/deep/own.package.nml` under
    // `tenants/*/flows/**` — `tenants/cu/other` is reached by nobody,
    // its parent `tenants/cu` is. Every strict ancestor is asked.
    let build = || {
        Ws::new()
            .manifest("demo.package.nml", "demo", &[], FLOWS_DEEP)
            .manifest(
                "tenants/cu/other/deep/own.package.nml",
                "own",
                &[],
                &[("all", &["*/**"])],
            )
            .file("tenants/cu/flows/a.flow.nml")
            .file("tenants/du/flows/b.flow.nml")
            .file("tenants/cu/other/deep/z/f.nml")
            .file("admin/ops.flow.nml")
    };
    // Governance (R5′) is untouched: the manifest binds its own subtree.
    let ws = build();
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    let u = d.universe();
    assert_eq!(
        bound(&governing(&u, &key("tenants/cu/other/deep/z/f.nml"))).0,
        "own"
    );
    assert_eq!(
        bound(&governing(&u, &key("tenants/cu/flows/a.flow.nml"))).1,
        "tenantFlows"
    );
    // Minting: none — spam under its would-be unit is the ROOT unit's.
    let ws = spam(build(), "tenants/cu/other/deep/z/spam", MAX_ENTRIES + 1);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated,
        Some(Truncation::Entries {
            dir: key("tenants/cu/other/deep/z/spam")
        }),
        "{:?}",
        d.truncated_units
    );
    assert!(d.truncated_units.is_empty(), "{:?}", d.truncated_units);
    let u = d.universe();
    assert_eq!(u.closure, Closure::Truncated);
    assert!(matches!(
        governing(&u, &key("tenants/du/flows/b.flow.nml")),
        Governing::Unbound
    ));
}

#[test]
fn a_gap_below_a_non_root_reaching_claim_is_tenant_level() {
    // r79 (r78 pin depth). The reaching claim is NOT the root's: an
    // operator manifest at `a/c.package.nml` claiming `p/flows/**`
    // reaches `a/p` (its own subtree, R5′); a manifest at
    // `a/p/other/m.package.nml` sits in a gap of it and is tenant-level.
    // A walk asking each ancestor's PARENT instead (shifted one level
    // up) finds no claim strictly above `a` and would let it mint.
    let ws = Ws::new()
        .manifest("a/c.package.nml", "c", &[], &[("flows", &["p/flows/**"])])
        .manifest("a/p/other/m.package.nml", "m", &[], &[("all", &["*/**"])])
        .file("a/p/flows/x.flow.nml")
        .file("a/p/other/z/f.nml");
    let ws = spam(ws, "a/p/other/z/spam", MAX_ENTRIES + 1);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated,
        Some(Truncation::Entries {
            dir: key("a/p/other/z/spam")
        }),
        "{:?}",
        d.truncated_units
    );
    assert!(d.truncated_units.is_empty(), "{:?}", d.truncated_units);
    // The positive control: the same manifest at `a/q/` — `q` is not
    // `p`, and `a` itself is reached by no claim strictly above it —
    // IS operator-level and mints `a/q/<t>` under its `*/**`: spam
    // there is that unit's alone, the universe whole, the operator's
    // file bound.
    let ws = Ws::new()
        .manifest("a/c.package.nml", "c", &[], &[("flows", &["p/flows/**"])])
        .manifest("a/q/m.package.nml", "m", &[], &[("all", &["*/**"])])
        .file("a/p/flows/x.flow.nml")
        .file("a/q/z/f.nml");
    let ws = spam(ws, "a/q/z/spam", MAX_ENTRIES + 1);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None, "{:?}", d.truncated);
    assert_eq!(
        d.truncated_units,
        [UnitTruncation {
            unit: key("a/q/z"),
            stop: key("a/q/z/spam"),
            why: UnitBound::Entries,
        }]
    );
    let u = d.universe();
    assert_eq!(
        bound(&governing(&u, &key("a/p/flows/x.flow.nml"))).1,
        "flows"
    );
}

#[test]
fn a_star_only_glob_leaves_the_tenant_directory_live_but_tenant_level() {
    // r79 (r78). `tenants/*` reaches the files directly under `tenants`
    // and no directory below: `tenants/cu` is unreached, so a manifest
    // there is LIVE — and tenant-level, because `tenants` above it is
    // reached. Under r75's "under no unit" test it was operator-level
    // and minted `tenants/cu/z` with its `*/**`; the spam there is the
    // root unit's.
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], &[("top", &["tenants/*"])])
        .manifest("tenants/cu/m.package.nml", "m", &[], &[("all", &["*/**"])])
        .file("tenants/x.nml")
        .file("tenants/cu/z/f.nml");
    let ws = spam(ws, "tenants/cu/z/spam", MAX_ENTRIES + 1);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(
        ws.read_keys()
            .iter()
            .any(|k| k.ends_with("tenants/cu/m.package.nml")),
        "live: {:?}",
        ws.read_keys()
    );
    assert_eq!(
        d.truncated,
        Some(Truncation::Entries {
            dir: key("tenants/cu/z/spam")
        }),
        "{:?}",
        d.truncated_units
    );
    assert!(d.truncated_units.is_empty());
}

#[test]
#[ignore = "perf tier: run with `cargo test -p nml-validate --release --lib -- --ignored perf_` \
            (four discoveries over sixteen units × 64 MiB of scripted sources, ~2 GiB resident \
            in release, r77/r79)"]
fn perf_live_input_backstop_boundary_is_exact_at_sixteen_units() {
    // r77 (r76 F1 (b)). Sixteen tenants under `tenants/*/flows/*.flow.nml`
    // (unit `tenants/<x>`), each with sixteen live manifests in
    // `other/v<j>/` declaring a distinct source sized so that the unit's
    // live inputs — manifest texts included — are EXACTLY 64 MiB. The
    // last unit's last source is shrunk by the root unit's own bytes
    // (the operator's manifest and its `core`), so the WHOLE walk reads
    // exactly MAX_TOTAL_LIVE_INPUT_BYTES: admitted (`>`, not `>=`), the
    // universe whole, every tenant and the operator bound. ONE more
    // byte in that source — the last unit still under its own bound —
    // and the backstop denies the whole universe, naming the source
    // (the sixteen-tenant doctrine applied to bytes). And when one
    // charge crosses the unit's bound AND the backstop, the backstop
    // wins: the universe, not a unit truncation.
    const UNITS: usize = 16;
    const PER_UNIT: usize = 16;
    let manifest = |t: usize, j: usize| {
        manifest_text(
            &format!("n{t:02}x{j:02}"),
            &[],
            &[("b", &["never/*.q.nml"])],
        )
    };
    let per_manifest = manifest(0, 0).len();
    let source_len = 4 * 1024 * 1024 - per_manifest;
    let source: String = "# ".to_string() + &"x".repeat(source_len - 3) + "\n";
    assert_eq!(source.len(), source_len);
    assert_eq!(PER_UNIT * (per_manifest + source_len), MAX_LIVE_INPUT_BYTES);
    assert_eq!(UNITS * MAX_LIVE_INPUT_BYTES, MAX_TOTAL_LIVE_INPUT_BYTES);
    let root_unit = manifest_text("demo", &[], FLOWS_ONLY).len() + DEMO_CORE.len();
    assert!(root_unit > 1 && root_unit < 4096, "{root_unit}");
    // `last_delta` adjusts the LAST unit's LAST source against `source_len`.
    let build = |last_delta: isize| {
        let mut ws = Ws::new()
            .manifest("demo.package.nml", "demo", &[], FLOWS_ONLY)
            .file("admin/ops.flow.nml");
        for t in 0..UNITS {
            ws = ws.file(&format!("tenants/t{t:02}/flows/a.flow.nml"));
            for j in 0..PER_UNIT {
                let dir = format!("tenants/t{t:02}/other/v{j:02}");
                let text = manifest(t, j);
                assert_eq!(text.len(), per_manifest);
                ws = ws.text(&format!("{dir}/n{t:02}x{j:02}.package.nml"), &text);
                let last = t == UNITS - 1 && j == PER_UNIT - 1;
                let len = if last {
                    (source_len as isize + last_delta) as usize
                } else {
                    source_len
                };
                ws = ws.text(
                    &format!("{dir}/core.model.nml"),
                    &("# ".to_string() + &"x".repeat(len - 3) + "\n"),
                );
            }
        }
        ws
    };
    // Exactly the backstop: admitted.
    let ws = build(-(root_unit as isize));
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated, None,
        "total == MAX_TOTAL_LIVE_INPUT_BYTES is not past it: {:?}",
        d.truncated
    );
    assert!(
        d.truncated_units.is_empty(),
        "no unit is past its own bound: {:?}",
        d.truncated_units
    );
    assert_eq!(d.claims.len(), 1 + UNITS * PER_UNIT);
    let u = d.universe();
    assert_eq!(u.closure, Closure::Complete);
    assert_eq!(
        bound(&governing(&u, &key("tenants/t15/flows/a.flow.nml"))).1,
        "tenantFlows"
    );
    assert_eq!(bound(&governing(&u, &key("admin/ops.flow.nml"))).1, "ops");
    drop(d);
    drop(ws);
    // One byte past it — the last unit within its own budget: the
    // universe, naming the input, with no unit truncation.
    let ws = build(-(root_unit as isize) + 1);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated,
        Some(Truncation::TotalLiveInputBytes {
            key: key("tenants/t15/other/v15/core.model.nml")
        })
    );
    assert!(d.truncated_units.is_empty() && d.claims.is_empty() && d.files.is_empty());
    let u = d.universe();
    assert_eq!(u.closure, Closure::Truncated);
    assert!(matches!(
        governing(&u, &key("tenants/t00/flows/a.flow.nml")),
        Governing::Unbound
    ));
    drop(d);
    drop(ws);
    // One charge crossing both the unit's bound and the backstop: the
    // backstop wins.
    let ws = build(1);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated,
        Some(Truncation::TotalLiveInputBytes {
            key: key("tenants/t15/other/v15/core.model.nml")
        }),
        "the universe, not `tenants/t15`"
    );
    assert!(d.truncated_units.is_empty());
    drop(d);
    drop(ws);
    // The config arm (r79, r78 F-b): exactly the backstop again, plus a
    // live project config of a few bytes in UNREACHED content under
    // the last unit (`deep/` is reached by no glob: the config is live,
    // charged to `tenants/t15` at ITS settlement, one depth below the
    // sources). The unit stays under its own bound — the config is
    // shorter than the root unit's slack — and the total does not:
    // the backstop names the config.
    let cfg = "tenants/t15/other/v15/deep/nml-project.nml";
    let body = "    # r79\n";
    assert!("project p:\n".len() + body.len() < root_unit);
    let ws = build(-(root_unit as isize)).config(cfg, body);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(
        d.truncated,
        Some(Truncation::TotalLiveInputBytes { key: key(cfg) }),
        "the config's charge, and no unit's: {:?}",
        d.truncated_units
    );
    assert!(d.truncated_units.is_empty() && d.claims.is_empty() && d.configs.is_empty());
}

/// The entry budget bounds the walk's WORK (r80-sec F1 residual): every
/// `reaches`/`is_budget_unit` probe a settlement asks is charged as an
/// entry to the unit being settled. K sibling manifests whose globs
/// reach nothing (`zzz/*.nml`) over M inputs beneath them cost 2·M·K
/// probes — quadratic, uncharged before — and the exact spend is
/// pinned at the bound: root listing (K manifests + 1 source + M dirs +
/// P pad files), K unit-root probes per listed subdirectory, 2 entries
/// per subdirectory, K inert probes per sub-manifest = MAX_ENTRIES with
/// P = MAX_ENTRIES − (K + 1 + 3M + 2MK); one more pad file and the
/// LAST charge — the last sub-manifest's K probes — denies the whole
/// universe (the root unit's), naming that manifest's directory.
#[test]
fn settlement_probes_are_charged_as_entries_at_the_exact_bound() {
    const K: usize = 1_000;
    const M: usize = 16;
    const P: usize = MAX_ENTRIES - (K + 1 + 3 * M + 2 * M * K);
    let build = |pad: usize| {
        let mut ws = Ws::new();
        for i in 0..K {
            ws = ws.manifest(
                &format!("m{i}.package.nml"),
                &format!("m{i}"),
                &[],
                &[("own", &["zzz/*.nml"])],
            );
        }
        for j in 0..M {
            ws = ws.manifest(
                &format!("s{j}/x{j}.package.nml"),
                &format!("x{j}"),
                &[],
                &[("own", &["zzz/*.nml"])],
            );
        }
        for i in 0..pad {
            ws = ws.file(&format!("pad{i}.nml"));
        }
        ws
    };
    let ws = build(P);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None, "exactly MAX_ENTRIES is not past it");
    assert!(d.truncated_units.is_empty());
    assert_eq!(
        d.claims.len(),
        K + M,
        "every sibling and sub-manifest is live"
    );
    assert!(
        d.inert.is_empty(),
        "nothing reaches anything: {:?}",
        d.inert
    );
    let ws = build(P + 1);
    let d = ws.discover(&root, vec![]);
    // Sub-directories settle in key order: `s9` is the last of s0…s15.
    assert_eq!(
        d.truncated,
        Some(Truncation::Entries { dir: key("s9") }),
        "the probes of the last settlement cross the bound"
    );
    assert!(d.claims.is_empty() && d.files.is_empty());
}

/// The same work inside a tenant's UNIT denies that unit alone: under
/// `tenants/*/flows/*.flow.nml` the gap `tenants/cu/other/` is live and
/// charged to `tenants/cu`; K siblings over M sub-manifests spend the
/// (scaled) unit budget on probes, the unit is `Entries`-truncated at
/// the sub-manifest whose settlement crossed it, and the sibling tenant
/// and the operator stand — exactly as an entry flood would.
#[test]
fn settlement_probes_spend_the_unit_budget_like_a_listing() {
    const K: usize = 64;
    const M: usize = 16;
    let mut ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], FLOWS_ONLY)
        .file("tenants/du/flows/y.flow.nml")
        .file("tenants/cu/flows/x.flow.nml");
    for i in 0..K {
        ws = ws.manifest(
            &format!("tenants/cu/other/m{i}.package.nml"),
            &format!("m{i}"),
            &[],
            &[("own", &["zzz/*.nml"])],
        );
    }
    for j in 0..M {
        ws = ws.manifest(
            &format!("tenants/cu/other/s{j}/x{j}.package.nml"),
            &format!("x{j}"),
            &[],
            &[("own", &["zzz/*.nml"])],
        );
    }
    let root = ws.root();
    // Within a 4,096-entry unit the shape fits (K + 1 + 3M + 2MK = 2,161).
    let d = ws.discover_scaled(&root, vec![], 4_096, 16 * 4_096);
    assert_eq!(d.truncated, None);
    assert!(d.truncated_units.is_empty(), "{:?}", d.truncated_units);
    assert_eq!(d.claims.len(), 1 + K + M);
    // Within a 1,024-entry unit it does not: the unit is spent by the
    // probes of a sub-manifest's settlement (its key is the stop), the
    // universe is whole, and the sibling tenant binds.
    let d = ws.discover_scaled(&root, vec![], 1_024, 16 * 1_024);
    assert_eq!(d.truncated, None);
    assert_eq!(d.truncated_units.len(), 1, "{:?}", d.truncated_units);
    let t = &d.truncated_units[0];
    assert_eq!(t.unit, key("tenants/cu"));
    assert_eq!(t.why, UnitBound::Entries);
    assert!(
        t.stop.as_str().starts_with("tenants/cu/other/s"),
        "the stop is the settling input: {}",
        t.stop
    );
    assert_eq!(d.claims.len(), 1, "every claim under the unit is dropped");
    let u = d.universe();
    assert!(matches!(
        governing(&u, &key("tenants/du/flows/y.flow.nml")),
        Governing::Bound { .. }
    ));
    assert!(matches!(
        governing(&u, &key("tenants/cu/flows/x.flow.nml")),
        Governing::Unbound
    ));
}

/// m07 (r80-sec): the universe-wide backstop, pinned in the DEFAULT
/// lane at mock scale through the test seam (`discover_scaled`) — the
/// same arithmetic as the perf tier's
/// `perf_backstop_boundary_and_a_within_budget_tenant_tips_the_universe`
/// with a 64-entry unit and a 1,024-entry backstop: 15 tenants exactly
/// at their unit budget (62 files + the spam entry + the unit-root
/// probe), the root unit's 19 entries + 17 probes, and t15 with K = 26
/// files make exactly the backstop; one more file and the whole
/// universe is denied while t15 is still within its own bound.
#[test]
fn the_backstop_is_exact_at_mock_scale() {
    const UNIT: usize = 64;
    const TOTAL: usize = 16 * UNIT;
    let mut ws = Ws::new().manifest("demo.package.nml", "demo", &[], TENANTS);
    for t in 0..15 {
        for i in 0..UNIT - 2 {
            ws.fs = ws.fs.file(&format!("/ws/tenants/t{t:02}/spam/f{i}"));
        }
    }
    let root_unit = 3 + 16 + 17;
    let k = TOTAL - root_unit - 15 * UNIT - 2;
    assert_eq!(k, 26);
    for i in 0..k {
        ws.fs = ws.fs.file(&format!("/ws/tenants/t15/spam/f{i}"));
    }
    let root = ws.root();
    let d = ws.discover_scaled(&root, vec![], UNIT, TOTAL);
    assert_eq!(d.truncated, None, "total == the backstop is not past it");
    assert!(d.truncated_units.is_empty(), "{:?}", d.truncated_units);
    assert_eq!(d.claims.len(), 1);
    ws.fs = ws.fs.file(&format!("/ws/tenants/t15/spam/f{k}"));
    let d = ws.discover_scaled(&root, vec![], UNIT, TOTAL);
    assert_eq!(
        d.truncated,
        Some(Truncation::Entries {
            dir: key("tenants/t15/spam")
        })
    );
    assert!(d.truncated_units.is_empty());
    // And the unit bound alone, at the same scale: one file past it in
    // one tenant denies that tenant only.
    let mut ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/du/plain.flow.nml");
    ws = spam(ws, "tenants/cu/spam", UNIT - 1);
    let d = ws.discover_scaled(&root, vec![], UNIT, TOTAL);
    assert_eq!(d.truncated, None);
    assert_eq!(
        d.truncated_units,
        [UnitTruncation {
            unit: key("tenants/cu"),
            stop: key("tenants/cu/spam"),
            why: UnitBound::Entries,
        }]
    );
    assert!(files(&d).contains(&"tenants/du/plain.flow.nml".to_string()));
}

/// r80-sec F4: the walk REPORTS what it left out of its enumeration by
/// policy — every symlink (whatever it names: its kind is what the walk
/// never learns), a `.nml` FIFO, a `.nml` dot-file, every dot-directory
/// and policy directory — by key with one word each; a `.gitignore`, a
/// `.nml`-less socket and the content INSIDE a skipped directory are
/// not rows (the directory is). Rows under a spent unit are dropped
/// with the unit's files; a truncated universe reports none.
#[test]
fn the_walk_reports_what_it_skipped() {
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/plain.flow.nml")
        .symlink("tenants/cu/link.flow.nml", "plain.flow.nml")
        .symlink("tenants/cu/lib", "../../vendor")
        .other("tenants/cu/fifo.flow.nml")
        .other("tenants/cu/sock")
        .file("tenants/cu/.secret.flow.nml")
        .file("tenants/cu/.hidden/bad.flow.nml")
        .file(".gitignore")
        .file("node_modules/pkg/x.nml")
        .file("target/x.nml")
        .file("tenants/du/plain.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let mut rows: Vec<(String, &str)> = d
        .skipped
        .iter()
        .map(|s| (s.key.to_string(), s.why.tag()))
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        [
            ("node_modules".to_string(), "policyDirectory"),
            ("target".to_string(), "policyDirectory"),
            ("tenants/cu/.hidden".to_string(), "dotDirectory"),
            ("tenants/cu/.secret.flow.nml".to_string(), "dotFile"),
            ("tenants/cu/fifo.flow.nml".to_string(), "fifo"),
            ("tenants/cu/lib".to_string(), "symlink"),
            ("tenants/cu/link.flow.nml".to_string(), "symlink"),
        ]
    );
    // Depth then key, like `files`.
    let keys: Vec<(usize, &SourceKey)> =
        d.skipped.iter().map(|s| (s.key.depth(), &s.key)).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    assert_eq!(keys[0].1.as_str(), "node_modules");
    assert_eq!(keys[2].1.as_str(), "tenants/cu/.hidden");
    // The dot-file is listed (in `files`) but never enumerated.
    assert!(files(&d).contains(&"tenants/cu/.secret.flow.nml".to_string()));
    assert!(
        !d.nml_files_under(&SourceKey::root())
            .any(|k| k.as_str() == "tenants/cu/.secret.flow.nml")
    );
    // A spent unit keeps no rows, as it keeps no files.
    let ws = spam(ws, "tenants/cu/spam", 64);
    let d = ws.discover_scaled(&root, vec![], 32, 16 * 32);
    assert_eq!(d.truncated_units.len(), 1);
    assert!(
        d.skipped
            .iter()
            .all(|s| !s.key.as_str().starts_with("tenants/cu/")),
        "{:?}",
        d.skipped
    );
    assert!(d.skipped.iter().any(|s| s.key.as_str() == "node_modules"));
    // A truncated universe reports none.
    let ws = spam(ws, "spam", 64);
    let d = ws.discover_scaled(&root, vec![], 32, 16 * 32);
    assert!(d.truncated.is_some());
    assert!(d.skipped.is_empty() && d.files.is_empty());
}

/// The hidden-directory audit a gate runs over a skipped dot-directory
/// (r80-sec F4): the `.nml` files, links and FIFOs beneath it at any
/// depth (nested dot-directories included), never `.git` (VCS metadata,
/// never audited) nor a policy directory beneath, by depth then key;
/// bounded by ONE budget per run shared by every hidden directory the
/// run audits — [`MAX_TOTAL_ENTRIES`], the universe's own backstop, so
/// a developer's `.venv` past the unit bound audits whole — a hidden
/// tree past it is reported incomplete at the directory that crossed
/// it, and an unlistable directory likewise.
#[test]
fn the_hidden_audit_lists_nml_content_and_stops_at_the_bound() {
    use crate::workspace::discover::{
        AuditBudget, MAX_AUDIT_EXAMPLES, MAX_TOTAL_ENTRIES, audit_hidden,
    };
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/.hidden/bad.flow.nml")
        .file("tenants/cu/.hidden/notes.txt")
        .file("tenants/cu/.hidden/deep/.also/x.nml")
        .symlink("tenants/cu/.hidden/l.nml", "bad.flow.nml")
        .symlink("tenants/cu/.hidden/l.txt", "notes.txt")
        .other("tenants/cu/.hidden/f.nml")
        .file("tenants/cu/.hidden/node_modules/pkg/y.nml")
        .file("tenants/cu/.hidden/.git/objects/z.nml")
        .file("tenants/cu/.git/hooks/h.nml");
    let root = ws.root();
    let mut budget = AuditBudget::default();
    assert_eq!(budget, AuditBudget::of(MAX_TOTAL_ENTRIES));
    let audit = audit_hidden(&root, &ws.fs, &key("tenants/cu/.hidden"), &mut budget);
    assert_eq!(audit.incomplete, None);
    // A count and examples by depth then key — a file, a FIFO and a link
    // named `.nml` all count (content a runtime could read); `.txt`,
    // `node_modules` and `.git` never do.
    assert_eq!(audit.nml, 4);
    let examples: Vec<String> = audit.examples.iter().map(ToString::to_string).collect();
    assert_eq!(
        examples,
        [
            "tenants/cu/.hidden/bad.flow.nml",
            "tenants/cu/.hidden/f.nml",
            "tenants/cu/.hidden/l.nml",
            "tenants/cu/.hidden/deep/.also/x.nml",
        ]
    );
    // `.git` is never audited.
    assert_eq!(
        audit_hidden(&root, &ws.fs, &key("tenants/cu/.git"), &mut budget),
        Default::default()
    );
    // Unlistable inside: reported incomplete there, what was seen kept.
    let ws = ws.denied("tenants/cu/.hidden/deep");
    let audit = audit_hidden(&root, &ws.fs, &key("tenants/cu/.hidden"), &mut budget);
    assert_eq!(audit.incomplete, Some(key("tenants/cu/.hidden/deep")));
    assert_eq!(audit.nml, 3);
    // A hidden tree past the UNIT bound audits whole under the run's
    // budget (a `.venv`), and is incomplete at the directory being
    // listed under a budget of exactly the unit bound.
    let ws = spam(
        Ws::new().manifest("demo.package.nml", "demo", &[], TENANTS),
        "tenants/cu/.big/spam",
        MAX_ENTRIES,
    );
    let audit = audit_hidden(
        &ws.root(),
        &ws.fs,
        &key("tenants/cu/.big"),
        &mut AuditBudget::default(),
    );
    assert_eq!(audit.incomplete, None);
    let audit = audit_hidden(
        &ws.root(),
        &ws.fs,
        &key("tenants/cu/.big"),
        &mut AuditBudget::of(MAX_ENTRIES),
    );
    assert_eq!(audit.incomplete, Some(key("tenants/cu/.big/spam")));
    // The budget is the RUN's: two hidden directories share it, and the
    // second is cut short where the first left off — exactly.
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/.one/a.nml")
        .file("tenants/cu/.one/b.nml")
        .file("tenants/cu/.two/c.nml")
        .file("tenants/cu/.two/d.nml");
    let root = ws.root();
    let mut shared = AuditBudget::of(3);
    let first = audit_hidden(&root, &ws.fs, &key("tenants/cu/.one"), &mut shared);
    assert_eq!(first.incomplete, None);
    assert_eq!(first.nml, 2);
    let second = audit_hidden(&root, &ws.fs, &key("tenants/cu/.two"), &mut shared);
    assert_eq!(second.incomplete, Some(key("tenants/cu/.two")));
    assert_eq!(second.nml, 1, "{:?}", second.examples);
    // The examples are bounded at MAX_AUDIT_EXAMPLES; the count is exact
    // past them (r85, r84-sec F1: the gate's memory is the directory's).
    let mut ws = Ws::new().manifest("demo.package.nml", "demo", &[], TENANTS);
    for i in 0..(MAX_AUDIT_EXAMPLES + 5) {
        ws = ws.file(&format!("tenants/cu/.many/f{i:02}.nml"));
    }
    let root = ws.root();
    let audit = audit_hidden(
        &root,
        &ws.fs,
        &key("tenants/cu/.many"),
        &mut AuditBudget::default(),
    );
    assert_eq!(audit.nml, MAX_AUDIT_EXAMPLES + 5);
    assert_eq!(audit.examples.len(), MAX_AUDIT_EXAMPLES);
    assert_eq!(audit.examples[0], key("tenants/cu/.many/f00.nml"));
    assert_eq!(
        audit.examples[MAX_AUDIT_EXAMPLES - 1],
        key(&format!(
            "tenants/cu/.many/f{:02}.nml",
            MAX_AUDIT_EXAMPLES - 1
        ))
    );
}

/// An unkeyable name inside a hidden directory: a `.nml`-named entry is
/// COUNTED (no key to show as an example), and a directory beneath is
/// never listed — the audit is incomplete there and the count a lower
/// bound; a `.txt` so named is no content.
#[test]
fn the_hidden_audit_counts_unkeyable_nml_names_and_is_incomplete_at_an_unkeyable_directory() {
    use crate::workspace::discover::{AuditBudget, audit_hidden};
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/.hidden/plain.flow.nml")
        .file("tenants/cu/.hidden/od\\d.flow.nml")
        .file("tenants/cu/.hidden/no\\te.txt");
    let root = ws.root();
    let mut budget = AuditBudget::default();
    let audit = audit_hidden(&root, &ws.fs, &key("tenants/cu/.hidden"), &mut budget);
    assert_eq!(audit.nml, 2, "{audit:?}");
    assert_eq!(audit.incomplete, None);
    assert_eq!(audit.examples, [key("tenants/cu/.hidden/plain.flow.nml")]);
    let mut ws = ws;
    ws.fs = ws
        .fs
        .dir("/ws/tenants/cu/.hidden/d\\e")
        .file("/ws/tenants/cu/.hidden/d\\e/x.nml");
    let audit = audit_hidden(&root, &ws.fs, &key("tenants/cu/.hidden"), &mut budget);
    assert_eq!(audit.nml, 2, "{audit:?}");
    assert_eq!(audit.incomplete, Some(key("tenants/cu/.hidden")));
    // Every counted name unkeyable: no example, and the row says so
    // instead of rendering an empty list (`(, and 1 more)`).
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/.hidden/od\\d.flow.nml");
    let audit = audit_hidden(&ws.root(), &ws.fs, &key("tenants/cu/.hidden"), &mut budget);
    assert_eq!((audit.nml, audit.examples.len()), (1, 0));
    let row = crate::workspace::diag::skipped_under(&key("tenants/cu/.hidden"), &audit)
        .expect("a counted row");
    assert!(
        row.message
            .contains("holding 1 `.nml` file(s) no verb judged (1 of them named by no key)"),
        "{}",
        row.message
    );
}

/// r80-sec F7: a tenant entry whose name no key can carry (a `\` on
/// unix — a legal byte in a file name, tracked by git) is charged and
/// skipped like a non-UTF-8 one, never joined: `SourceKey::join`
/// asserts plain names, and a debug build aborted every check under the
/// root on `a\b.flow.nml` (exit 101, the closing row never printed)
/// while release refused the file at its open. Nothing bound changes:
/// the sibling file is enumerated and bound, the odd name is absent
/// from `files` — and PRESENT in the skipped report, under the
/// directory holding it, with its kind and its name (an unreported one
/// let the gate certify a directory of content it never entered); a
/// `.txt` so named is no content and no row.
#[test]
fn a_backslash_named_entry_is_skipped_not_joined() {
    use crate::fs::EntryKind;
    use crate::workspace::Skip;
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/plain.flow.nml")
        .file("tenants/cu/a\\b.flow.nml")
        .file("tenants/cu/n\\o.txt");
    let mut ws = ws;
    ws.fs = ws.fs.dir("/ws/tenants/cu/d\\e");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    assert!(d.truncated_units.is_empty());
    let listed = files(&d);
    assert!(
        listed.contains(&"tenants/cu/plain.flow.nml".to_string()),
        "{listed:?}"
    );
    assert!(!listed.iter().any(|f| f.contains('\\')), "{listed:?}");
    assert!(!d.skipped.iter().any(|s| s.key.as_str().contains('\\')));
    let odd: Vec<(String, Skip)> = d
        .skipped
        .iter()
        .filter(|s| matches!(s.why, Skip::UnkeyableName { .. }))
        .map(|s| (s.key.to_string(), s.why.clone()))
        .collect();
    assert_eq!(
        odd,
        [
            (
                "tenants/cu".to_string(),
                Skip::UnkeyableName {
                    kind: EntryKind::File,
                    name: "a\\b.flow.nml".to_string(),
                },
            ),
            (
                "tenants/cu".to_string(),
                Skip::UnkeyableName {
                    kind: EntryKind::Dir,
                    name: "d\\e".to_string(),
                },
            ),
        ]
    );
    // The wire's `entry` is the variant's own name — present exactly there.
    assert!(
        d.skipped
            .iter()
            .all(|s| s.entry().is_some() == matches!(s.why, Skip::UnkeyableName { .. }))
    );
    let u = d.universe();
    assert!(matches!(
        governing(&u, &key("tenants/cu/plain.flow.nml")),
        Governing::Bound { .. }
    ));
}

/// r103-cov: the walk's two OUTPUT ORDERS are a contract, and both
/// held only by accident on the fixtures that existed.
///
/// `files` is "(depth, key)" — but the walk appends each round's
/// listings in FRONTIER order, and a concatenation of sorted listings of
/// sorted directories is not sorted: `a` sorts before `a-b`, while
/// `a-b/x` sorts before `a/x` (`-` < `/`). Deleting the round's sort
/// left every test green.
///
/// `skipped` is "(depth, key, why)" — but an unkeyable entry's row is
/// keyed at the directory HOLDING it (depth *d*) and is pushed in the
/// same listing as the deeper rows (depth *d+1*) of the same directory,
/// so the push order is unsorted exactly when a dot-file, FIFO or link
/// sorts before an unkeyable name. Deleting `sort_by_depth` left every
/// test green too.
#[test]
fn the_walks_files_and_skipped_rows_come_out_in_their_stated_order() {
    let mut ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        // Sibling directories whose SORTED order is not the sorted order
        // of the keys beneath them.
        .file("tenants/a/x.flow.nml")
        .file("tenants/a-b/x.flow.nml")
        .file("tenants/ab/x.flow.nml")
        // One directory holding a dot-file (a depth-d+1 row) and, after
        // it in the listing, an unkeyable name (a depth-d row).
        .file("tenants/cu/plain.flow.nml")
        .file("tenants/cu/.secret.flow.nml")
        .file("tenants/cu/ev\\il.flow.nml");
    ws.fs = ws.fs.dir("/ws/tenants/cu/z\\z");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    let keys: Vec<(usize, &SourceKey)> = d.files.iter().map(|k| (k.depth(), k)).collect();
    let mut want = keys.clone();
    want.sort();
    assert_eq!(keys, want, "`files` is (depth, key)");
    assert!(
        files(&d).contains(&"tenants/a-b/x.flow.nml".to_string()),
        "{:?}",
        files(&d)
    );
    let rows: Vec<(usize, &SourceKey, &Skip)> = d
        .skipped
        .iter()
        .map(|s| (s.key.depth(), &s.key, &s.why))
        .collect();
    let mut want = rows.clone();
    want.sort();
    assert_eq!(rows, want, "`skipped` is (depth, key, why)");
    // The fixture really does exercise both shapes: a same-directory
    // pair whose push order is depth d+1 then depth d.
    assert!(
        d.skipped
            .iter()
            .any(|s| matches!(s.why, Skip::DotFile)
                && s.key.as_str() == "tenants/cu/.secret.flow.nml")
    );
    assert_eq!(
        d.skipped
            .iter()
            .filter(|s| matches!(s.why, Skip::UnkeyableName { .. }))
            .count(),
        2,
        "{:?}",
        d.skipped
    );
}

/// RFC 0026 B-2: an unkeyable name is reported WHATEVER holds it — a
/// symlink (whether it points at a directory of content is exactly what
/// the walk never learns) and a `.nml`-named special entry — each with
/// its `lstat` kind inside the reason (`discover::listed`, the one
/// classifier); a special entry not so named is no content and no row,
/// as a plain-named one is not.
#[test]
fn an_unkeyable_symlink_and_a_nml_named_special_entry_are_reported_by_kind() {
    use crate::fs::EntryKind;
    use crate::workspace::Skip;
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/plain.flow.nml");
    let mut ws = ws;
    ws.fs = ws
        .fs
        .symlink("/ws/tenants/cu/l\\n", "plain.flow.nml")
        .other("/ws/tenants/cu/f\\i.nml")
        .other("/ws/tenants/cu/q\\r.sock");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let mut rows: Vec<(String, String, EntryKind)> = d
        .skipped
        .iter()
        .filter_map(|s| match &s.why {
            Skip::UnkeyableName { kind, name } => Some((name.clone(), s.key.to_string(), *kind)),
            _ => None,
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        rows,
        [
            (
                "f\\i.nml".to_string(),
                "tenants/cu".to_string(),
                EntryKind::Other
            ),
            (
                "l\\n".to_string(),
                "tenants/cu".to_string(),
                EntryKind::Symlink
            ),
        ],
        "{:?}",
        d.skipped
    );
    assert!(
        d.files.iter().all(|f| !f.as_str().contains('\\')),
        "{:?}",
        d.files
    );
}

/// r84-cov (mutant D2 survived): the verdict order at the one entry
/// chokepoint. A listing entry that crosses a unit's own bound AND the
/// universe backstop at once is the BACKSTOP's — the whole universe
/// truncates, no unit stands denied alone. With the unit verdict first,
/// sixteen tenants' worth of entries were visited and the universe still
/// read `Complete` with one tenant denied. Sixteen tenants exactly at
/// their unit budget (62 files + the spam entry + the unit-root probe),
/// the root unit's 36, the backstop set to that sum: t15's 63rd file is
/// one entry past BOTH bounds on one charge.
#[test]
fn a_charge_crossing_both_bounds_is_the_backstops() {
    const UNIT: usize = 64;
    let mut ws = Ws::new().manifest("demo.package.nml", "demo", &[], TENANTS);
    for t in 0..16 {
        for i in 0..UNIT - 2 {
            ws.fs = ws.fs.file(&format!("/ws/tenants/t{t:02}/spam/f{i}"));
        }
    }
    let root_unit = 3 + 16 + 17;
    let total = root_unit + 16 * UNIT;
    let root = ws.root();
    let d = ws.discover_scaled(&root, vec![], UNIT, total);
    assert_eq!(
        d.truncated, None,
        "every unit and the whole are exactly at their bounds"
    );
    assert!(d.truncated_units.is_empty(), "{:?}", d.truncated_units);
    ws.fs = ws.fs.file(&format!("/ws/tenants/t15/spam/f{}", UNIT - 2));
    let d = ws.discover_scaled(&root, vec![], UNIT, total);
    assert_eq!(
        d.truncated,
        Some(Truncation::Entries {
            dir: key("tenants/t15/spam")
        }),
        "one charge past both bounds is the backstop's verdict"
    );
    assert!(
        d.truncated_units.is_empty(),
        "no unit is denied alone: {:?}",
        d.truncated_units
    );
    assert!(d.claims.is_empty() && d.files.is_empty());
}

/// r84-cov (mutant D6 survived): the hidden audit shares the walk's
/// component bound — a directory at depth 64 is never listed
/// (`SourceKey::child_dir`, the one descent rule), so no audit example
/// ever carries a 65-component key and none is counted (the walk's own
/// "exact skip, never a truncation"): a `.nml` in the deepest listable
/// directory (depth 64) is counted and named, one directory further is
/// not — and the audit says it is INCOMPLETE there: the walk's exact
/// skip is a gate row, never a silent one.
#[test]
fn the_hidden_audit_never_mints_a_key_past_the_component_bound() {
    use crate::workspace::discover::{AuditBudget, audit_hidden};
    use crate::workspace::paths::MAX_COMPONENTS;
    let mut chain = String::from("tenants/cu/.h");
    for i in 1..=61 {
        chain.push_str(&format!("/d{i}"));
    }
    assert_eq!(key(&chain).depth(), MAX_COMPONENTS);
    let deepest_listable = chain.rsplit_once('/').unwrap().0.to_string();
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file(&format!("{deepest_listable}/x.nml"))
        .file(&format!("{chain}/y.nml"));
    let root = ws.root();
    let audit = audit_hidden(
        &root,
        &ws.fs,
        &key("tenants/cu/.h"),
        &mut AuditBudget::default(),
    );
    assert_eq!(
        audit.incomplete,
        Some(key(&deepest_listable)),
        "never a truncation, never silent: incomplete at the deepest listable directory"
    );
    assert_eq!(
        audit.nml, 1,
        "the file past the bound is not counted — a lower bound the row says is one"
    );
    let rows: Vec<String> = audit.examples.iter().map(ToString::to_string).collect();
    assert_eq!(rows, [format!("{deepest_listable}/x.nml")]);
    assert!(
        audit.examples.iter().all(|k| k.depth() <= MAX_COMPONENTS),
        "{rows:?}"
    );
}

/// r84-cov (mutant D10 survived): the audit's examples are by depth then
/// key — the order the gate names them in and `Discovery::files` keeps —
/// never the listing's own order (a breadth-first walk over UNSORTED
/// listings would name a later sibling first).
#[test]
fn the_hidden_audit_reports_by_depth_then_key() {
    use crate::workspace::discover::{AuditBudget, audit_hidden};
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/.h/b/y.nml")
        .file("tenants/cu/.h/b/deep/z.nml")
        .file("tenants/cu/.h/a/x.nml");
    let root = ws.root();
    let audit = audit_hidden(
        &root,
        &ws.fs,
        &key("tenants/cu/.h"),
        &mut AuditBudget::default(),
    );
    let rows: Vec<String> = audit.examples.iter().map(ToString::to_string).collect();
    assert_eq!(
        rows,
        [
            "tenants/cu/.h/a/x.nml",
            "tenants/cu/.h/b/y.nml",
            "tenants/cu/.h/b/deep/z.nml",
        ]
    );
}

/// r86: an unlistable directory INSIDE a hidden tree stops the audit
/// short THERE, not everywhere — the siblings still in the frontier are
/// listed, so the count is everything the gate could see (a lower bound
/// the row says is one), and `incomplete` names the first directory it
/// could not list (the gate's own row). It used to `break` at the first
/// unlistable directory: `z/2.nml`, queued after it, went uncounted and
/// unnamed while the row claimed an exact count.
#[test]
fn the_hidden_audit_keeps_listing_past_an_unlistable_subdirectory() {
    use crate::workspace::discover::{AuditBudget, audit_hidden};
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/.inc/a/1.nml")
        .file("tenants/cu/.inc/locked/3.nml")
        .file("tenants/cu/.inc/z/2.nml")
        .denied("tenants/cu/.inc/locked");
    let root = ws.root();
    let audit = audit_hidden(
        &root,
        &ws.fs,
        &key("tenants/cu/.inc"),
        &mut AuditBudget::default(),
    );
    assert_eq!(audit.incomplete, Some(key("tenants/cu/.inc/locked")));
    assert_eq!(audit.nml, 2, "{:?}", audit.examples);
    assert_eq!(
        audit.examples,
        [
            key("tenants/cu/.inc/a/1.nml"),
            key("tenants/cu/.inc/z/2.nml")
        ]
    );
    let row =
        crate::workspace::diag::skipped_under(&key("tenants/cu/.inc"), &audit).expect("a row");
    assert!(
        row.message
            .contains("holding at least 2 `.nml` file(s) no verb judged"),
        "{}",
        row.message
    );
}

/// The externals' marker probes are charged to the root unit like every
/// settlement probe (r80-sec F1): a store package whose `rootMarkers`
/// name a file N tenants committed costs the walk exactly one charged
/// probe per candidate marker beneath a live claim — pinned as the
/// exact difference between the smallest total bound that completes
/// the universe WITH the external claim and WITHOUT it (N), so an
/// uncharged probe is the one difference that vanishes; at the exact
/// bound the markers are live and anchor the claim, one entry less
/// denies the whole universe at the root.
#[test]
fn external_marker_probes_are_charged_one_entry_per_candidate() {
    const N: usize = 40;
    let mut ws = Ws::new().manifest("m.package.nml", "m", &[], &[("own", &["zzz/*.nml"])]);
    for i in 0..N {
        ws = ws.file(&format!("s{i}/nudge.nml"));
    }
    let root = ws.root();
    let store = || {
        vec![external(
            "demo",
            &["nudge.nml"],
            &[("own", &["**/*.flow.nml"])],
            ExternalClass::Store,
        )]
    };
    let completes = |with_store: bool, total: usize| {
        let extra = if with_store { store() } else { Vec::new() };
        ws.discover_scaled(&root, extra, total, total)
            .truncated
            .is_none()
    };
    let smallest = |with_store: bool| -> usize {
        let (mut lo, mut hi) = (1usize, 8 * N + 64);
        assert!(completes(with_store, hi), "the search's ceiling completes");
        while lo < hi {
            let mid = (lo + hi) / 2;
            if completes(with_store, mid) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    };
    let without = smallest(false);
    let with = smallest(true);
    assert_eq!(
        with,
        without + N,
        "one charged probe per candidate marker ({without} without the store claim)"
    );
    let d = ws.discover_scaled(&root, store(), with, with);
    assert!(d.truncated.is_none() && d.truncated_units.is_empty());
    let anchored = d
        .claims
        .iter()
        .find(|c| c.name() == "demo")
        .expect("the store claim is minted");
    assert!(
        matches!(anchored.origin(), ClaimOrigin::External { markers, .. } if markers.len() == N),
        "every marker anchors it: {:?}",
        anchored.origin()
    );
    let d = ws.discover_scaled(&root, store(), with - 1, with - 1);
    assert_eq!(
        d.truncated,
        Some(Truncation::Entries {
            dir: SourceKey::root()
        }),
        "the last probe crosses the bound at the root"
    );
    assert!(d.claims.is_empty() && d.files.is_empty());
}

/// RFC 0026 B-3, the harm demonstrated: two operator globs whose
/// inferred units nest the multiplying way (`tenants/*` and
/// `tenants/*/plugins/*`) give one tenant a fresh entry budget per
/// plugin directory, so its directory count carries its spend past the
/// universe-wide BACKSTOP — the whole universe truncated, everyone
/// denied — where one unit per tenant denies that tenant alone. The
/// layout is NML2092's nested form at the inner glob; the declaration it names
/// restores the isolation on the same tree.
#[test]
fn nested_inferred_units_carry_a_tenants_flood_to_the_backstop() {
    let flows = "tenants/**/*.flow.nml";
    let plugins = "tenants/*/plugins/*/**/*.model.nml";
    let tree = |ws: Ws| {
        let mut ws = ws;
        for p in 1..=6 {
            for f in 1..=7 {
                ws = ws.file(&format!("tenants/cu/plugins/p{p}/m{f}.model.nml"));
            }
        }
        ws.file("tenants/du/x.flow.nml")
    };
    // Under inference: nested units, the backstop crossed, the universe gone.
    let ws = tree(Ws::new().manifest(
        "demo.package.nml",
        "demo",
        &[],
        &[("a", &[flows]), ("b", &[plugins])],
    ));
    let root = ws.root();
    let d = ws.discover_scaled(&root, vec![], 20, 50);
    assert!(
        matches!(d.truncated, Some(Truncation::Entries { .. })),
        "the backstop: {:?} / {:?}",
        d.truncated,
        d.truncated_units
    );
    assert!(d.files.is_empty(), "nothing enumerated: {:?}", d.files);
    // The same tree, same bounds, the unit declared once: the tenant alone.
    let text = manifest_text_with_units(
        "demo",
        &[],
        &["tenants/*"],
        &[("a", &[flows]), ("b", &[plugins])],
    );
    let ws = tree(
        Ws::new()
            .text("demo.package.nml", &text)
            .text("core.model.nml", crate::test_support::DEMO_CORE),
    );
    let root = ws.root();
    let d = ws.discover_scaled(&root, vec![], 20, 50);
    assert_eq!(d.truncated, None, "{:?}", d.truncated);
    assert_eq!(
        d.truncated_units
            .iter()
            .map(|u| u.unit.to_string())
            .collect::<Vec<_>>(),
        ["tenants/cu"]
    );
    assert!(d.layout_notes().is_empty(), "declared: silent");
    let listed = files(&d);
    assert!(
        listed.contains(&"tenants/du/x.flow.nml".to_string()),
        "{listed:?}"
    );
    // And the inferred layout is warned about, at the inner glob — ONE
    // row: the gap it is by construction, in its nested form.
    let ws = tree(Ws::new().manifest(
        "demo.package.nml",
        "demo",
        &[],
        &[("a", &[flows]), ("b", &[plugins])],
    ));
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let notes = d.layout_notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].code, Some(codes::BUDGET_UNIT_GAP));
    assert_eq!(notes[0].source.as_deref(), Some("demo.package.nml"));
    assert!(
        notes[0].message.contains("nests inside `tenants/*`")
            && notes[0]
                .message
                .contains("declare budgetUnits = [\"tenants/*\"]"),
        "{}",
        notes[0].message
    );
}

/// RFC 0026 B-24: the source's own remedy rides the NML2091 row — the
/// NML2054 `Delete` of the shadowing field, stamped with the SOURCE's key
/// (`caused_by`'s stamping, the file given), so the row's consumer applies
/// or routes it in the source's file, never against the governed file's
/// text. The row's cause is the same finding in the same file.
#[test]
fn a_bindings_unbuildable_row_carries_the_sources_remedy_in_the_sources_file() {
    use nml_core::diagnostic::{Suggestion, SuggestionKind};
    let manifest = "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema \
                    schemas:\n    - core:\n        file = \"core.model.nml\"\n\n[]validator \
                    validators:\n    - tenantFlows:\n        files:\n            - \
                    \"tenants/**/*.flow.nml\"\n        schemas:\n            - core\n";
    let source = "model logEntry:\n    kind string?\n    msg string?\n\noneof record by kind:\n    \
                  \"log\" -> logEntry\n";
    let ws = Ws::new()
        .text("demo.package.nml", manifest)
        .text("core.model.nml", source)
        .file("tenants/cu/plain.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(d.load_errors.is_empty(), "{:?}", d.load_errors);
    let u = d.universe();
    let tenant = crate::workspace::discover::resolve_file(
        &u,
        Path::new("/ws/tenants/cu/plain.flow.nml"),
        &ws.fs,
    )
    .expect("keyed");
    let [finding] = tenant.findings.as_slice() else {
        panic!("one finding, got {:?}", tenant.findings);
    };
    assert_eq!(finding.code, Some(codes::VALIDATOR_UNBUILDABLE));
    let field = source.find("kind string?").expect("the field");
    let span = nml_core::span::Span::new(field, field + "kind string?".len());
    assert_eq!(
        finding.suggestions,
        vec![Suggestion::delete().at(span).in_file("core.model.nml")],
        "{finding:?}"
    );
    assert_eq!(finding.suggestions[0].kind, SuggestionKind::Delete);
    assert_eq!(
        finding.suggestion_source(&finding.suggestions[0]),
        Some("core.model.nml")
    );
    assert_eq!(finding.cause_source(), Some("core.model.nml"));
    assert_eq!(
        finding.cause.as_ref().map(|c| c.code),
        Some(codes::SHADOWED_DISCRIMINATOR)
    );
}

/// The input kind is a FILE-NAME question, and the kernel answers it
/// once ([`InputKind::of_name`]) so a front end reading such a file
/// outside the walk reads it under the cap the walk would. A bare
/// `package.nml` is deliberately NOT a manifest (`is_manifest_name`),
/// and the editor's own copy of this rule said it was.
#[test]
fn input_kind_of_name_is_the_walks_own_name_rule() {
    use crate::workspace::InputKind;
    for (name, want) in [
        ("demo.package.nml", InputKind::Manifest),
        (".package.nml", InputKind::Manifest),
        ("package.nml", InputKind::Source),
        ("nml-project.nml", InputKind::ProjectConfig),
        ("nml-project.nml.bak", InputKind::Source),
        ("core.model.nml", InputKind::Source),
        ("", InputKind::Source),
    ] {
        assert_eq!(InputKind::of_name(name), want, "{name}");
    }
    // The rule IS the walk's: a name the walk reads as a manifest is one
    // here, and nothing else is.
    for name in ["demo.package.nml", ".package.nml", "package.nml", "x.nml"] {
        assert_eq!(
            crate::file_names::is_manifest_name(name),
            InputKind::of_name(name) == InputKind::Manifest,
            "{name}"
        );
    }
}

/// r103-cov: R3′ is STRICT ancestry, and a SIBLING in the same directory
/// proves it.
///
/// `reaching_outer` filters the live claims by
/// `manifest.dir_is_strict_ancestor_of(dir)`. Relaxing that to
/// `dir_contains` — the same rule with "or the directory itself" — left
/// every test green, because no fixture had two live manifests in ONE
/// directory. It changes the settlement: a manifest settled earlier in
/// the same round, whose glob reaches its own directory, would make its
/// sibling INERT — "content, not configuration" — so an operator adding
/// a second package beside the first would silently lose it, and which
/// one survived would be decided by their names.
#[test]
fn a_manifest_never_inerts_a_sibling_in_its_own_directory() {
    let ws = Ws::new()
        // `a` claims everything under the root, its own directory
        // included; `b` sits beside it.
        .manifest("a.package.nml", "a", &[], &[("all", &["**"])])
        .manifest("b.package.nml", "b", &[], &[("bees", &["bees/**"])])
        .file("bees/x.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.truncated, None);
    assert!(
        d.inert.is_empty(),
        "neither manifest is content: {:?}",
        inert_keys(&d)
    );
    assert_eq!(
        manifest_keys(&d),
        ["a.package.nml", "b.package.nml"],
        "both are live claims"
    );
    // The same at depth, where the pair sits INSIDE content the shallower
    // claim reaches: both are inert THERE — for the shallower claim's
    // reach, never for each other's.
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], &[("apps", &["apps/**"])])
        .manifest("apps/a.package.nml", "a", &[], &[("all", &["**"])])
        .manifest("apps/b.package.nml", "b", &[], &[("bees", &["bees/**"])]);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let mut inert = inert_keys(&d);
    inert.sort();
    assert_eq!(inert, ["apps/a.package.nml", "apps/b.package.nml"]);
    for diag in &d.inert {
        assert!(
            diag.message.contains("demo.package.nml"),
            "inert for the SHALLOWER claim's reach, not a sibling's: {}",
            diag.message
        );
    }
    assert_eq!(manifest_keys(&d), ["demo.package.nml"]);
}
