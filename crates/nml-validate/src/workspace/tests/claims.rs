//! Governing-binding pins (step 0c): the three hardening rules as
//! running code — subtree governance (R5′), ambiguity over shadowing
//! (rule 3), and the class ladder — over discovered universes.

use super::*;

const TENANTS: &[(&str, &[&str])] = &[("tenantFlows", &["tenants/**/*.flow.nml"])];

#[test]
fn same_name_nested_manifest_is_ambiguous_never_shadow() {
    // Rule 3: a manifest declaring the SAME package name as an outer one
    // never shadows it. Two cases. (1) NESTED inside the outer binding's
    // claimed content (`tenants/**/*.flow.nml` reaches `tenants/cu`): the
    // tenant's `demo` is INERT (rule 2 fires first) — it defines nothing,
    // and the operator's binding governs the file. (2) At EQUAL depth
    // (a second root manifest declaring `demo` under another file name):
    // since E35's stem rule (RFC 0030) that is a LOAD ERROR naming both
    // — two definitions of one name can never share a directory, and a
    // nested one is inert — so the operator's `demo` governs alone and
    // nothing shadows it. Ambiguity survives for DIFFERENT names claiming
    // one file (rule 3 over `demo`/`other` in the fixtures).
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .manifest(
            "tenants/cu/demo.package.nml",
            "demo",
            &[],
            &[("mine", &["**/*.flow.nml"])],
        )
        .file("tenants/cu/x.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(inert_keys(&d), ["tenants/cu/demo.package.nml"]);
    assert_eq!(manifest_keys(&d), ["demo.package.nml"]);
    let u = d.universe();
    let (pkg, binding, ..) = bound(&governing(&u, &key("tenants/cu/x.flow.nml")));
    assert_eq!((pkg.as_str(), binding.as_str()), ("demo", "tenantFlows"));

    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .manifest(
            "second.package.nml",
            "demo",
            &[],
            &[("mine", &["**/*.flow.nml"])],
        )
        .file("tenants/cu/x.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(d.inert.is_empty(), "{:?}", d.inert);
    assert_eq!(manifest_keys(&d), ["demo.package.nml"]);
    assert_eq!(d.load_errors.len(), 1, "{:?}", d.load_errors);
    assert_eq!(
        d.load_errors[0].source.as_deref(),
        Some("second.package.nml")
    );
    let u = d.universe();
    let (pkg, binding, glob, _) = bound(&governing(&u, &key("tenants/cu/x.flow.nml")));
    assert_eq!(
        (pkg.as_str(), binding.as_str(), glob),
        ("demo", "tenantFlows", 0)
    );
    // Rule 3 over DIFFERENT names still ambiguates: `other` claiming
    // the same file beside `demo` — both live, both candidates, denied.
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .manifest(
            "other.package.nml",
            "other",
            &[],
            &[("mine", &["**/*.flow.nml"])],
        )
        .file("tenants/cu/x.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(d.load_errors.is_empty(), "{:?}", d.load_errors);
    let u = d.universe();
    match governing(&u, &key("tenants/cu/x.flow.nml")) {
        Governing::Ambiguous(claimants) => {
            let mut keys: Vec<&str> = claimants
                .iter()
                .map(|c| c.claim.manifest_label.as_str())
                .collect();
            keys.sort_unstable();
            assert_eq!(keys, ["demo.package.nml", "other.package.nml"]);
            // Each claimant carries its matched glob index (the r48 nit).
            assert!(claimants.iter().all(|c| c.glob == 0));
        }
        other => panic!("expected ambiguous: {other:?}"),
    }
    // A pin cannot revive a second `demo` either: the stem rule refused
    // it at load, and the pin binds the one definition.
    let ws = Ws::new()
        .config("nml-project.nml", "    schemaPackages:\n        - demo\n")
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .manifest(
            "second.package.nml",
            "demo",
            &[],
            &[("mine", &["**/*.flow.nml"])],
        )
        .file("tenants/cu/x.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(manifest_keys(&d), ["demo.package.nml"]);
    assert_eq!(d.load_errors.len(), 1, "{:?}", d.load_errors);
    assert_eq!(
        d.load_errors[0].source.as_deref(),
        Some("second.package.nml")
    );
    let u = d.universe();
    assert!(u.is_closed());
    let (pkg, binding, ..) = bound(&governing(&u, &key("tenants/cu/x.flow.nml")));
    assert_eq!((pkg.as_str(), binding.as_str()), ("demo", "tenantFlows"));
}

#[test]
fn pin_with_two_live_workspace_definitions_binds_uniquely_per_directory() {
    // Two live workspace manifests define `demo` in SIBLING subtrees; a
    // file under one of them is pinned to `demo`. Only the manifest whose
    // directory contains the file is eligible (R5′) — so it binds
    // uniquely. A directory holding a SECOND `demo` definition under
    // another file name (same depth, neither inside the other's claimed
    // content) is E35's stem-rule load error, never a second candidate:
    // the honest one still binds the pin uniquely.
    let ws = Ws::new()
        .config("nml-project.nml", "    schemaPackages:\n        - demo\n")
        .manifest(
            "a/demo.package.nml",
            "demo",
            &[],
            &[("all", &["**/*.flow.nml"])],
        )
        .manifest(
            "b/demo.package.nml",
            "demo",
            &[],
            &[("all", &["**/*.flow.nml"])],
        )
        .manifest(
            "b/y.package.nml",
            "demo",
            &[],
            &[("all", &["**/*.flow.nml"])],
        )
        .file("a/x.flow.nml")
        .file("b/c/y.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(d.inert.is_empty(), "{:?}", d.inert);
    assert_eq!(
        manifest_keys(&d),
        ["a/demo.package.nml", "b/demo.package.nml"]
    );
    assert_eq!(d.load_errors[0].source.as_deref(), Some("b/y.package.nml"));
    let u = d.universe();
    let (pkg, binding, glob, anchor) = bound(&governing(&u, &key("a/x.flow.nml")));
    assert_eq!(
        (pkg.as_str(), binding.as_str(), glob, anchor.as_str()),
        ("demo", "all", 0, "")
    );
    assert!(matches!(
        governing(&u, &key("a/x.flow.nml")),
        Governing::Bound {
            step: BindingStep::Pinned,
            ..
        }
    ));
    match governing(&u, &key("b/c/y.flow.nml")) {
        Governing::Bound { claimant, step } => {
            assert_eq!(claimant.claim.manifest_label, "b/demo.package.nml");
            assert_eq!(step, BindingStep::Pinned);
        }
        other => panic!("the honest definition binds: {other:?}"),
    }
}

#[test]
fn manifest_governs_own_subtree_only() {
    // R5′: a manifest anchored at the root (via the root config) still
    // governs only keys under its own directory — its glob's relative
    // base is the root, its authority is `schemas/**`.
    let ws = Ws::new()
        .config("nml-project.nml", "")
        .manifest("schemas/demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/x.flow.nml")
        .file("schemas/tenants/cu/y.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    assert!(
        matches!(&d.claims[0].origin, ClaimOrigin::Workspace { anchor, .. } if anchor.as_str().is_empty())
    );
    assert!(matches!(
        governing(&u, &key("tenants/cu/x.flow.nml")),
        Governing::Unbound
    ));
    // Under its own dir the root-anchored glob matches the FULL key.
    assert!(matches!(
        governing(&u, &key("schemas/tenants/cu/y.flow.nml")),
        Governing::Unbound
    ));
    let ws = Ws::new()
        .config("nml-project.nml", "")
        .manifest(
            "schemas/demo.package.nml",
            "demo",
            &[],
            &[("own", &["schemas/tenants/**/*.flow.nml"])],
        )
        .file("schemas/tenants/cu/y.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    assert_eq!(
        bound(&governing(&u, &key("schemas/tenants/cu/y.flow.nml"))).1,
        "own"
    );
}

#[test]
fn sibling_manifest_cannot_inert_or_govern() {
    // The alphabet attack (E26 (2)): `apps/aaa-tenant/evil.package.nml`
    // sorts before `apps/core/demo.package.nml` and claims `**` under
    // the root anchor. Path order is never authority: it is not a strict
    // ancestor of the operator's manifest, so it inerts nothing and
    // governs nothing outside `apps/aaa-tenant/`.
    let ws = Ws::new()
        .config("nml-project.nml", "")
        .manifest(
            "apps/aaa-tenant/evil.package.nml",
            "evil",
            &[],
            &[("all", &["**"])],
        )
        .manifest(
            "apps/core/demo.package.nml",
            "demo",
            &[],
            &[("core", &["apps/core/**/*.nml"])],
        )
        .file("apps/core/flow.nml")
        .file("apps/aaa-tenant/own.nml")
        .file("docs/readme.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(d.inert.is_empty(), "nothing is inert: {:?}", d.inert);
    assert_eq!(
        manifest_keys(&d),
        [
            "apps/aaa-tenant/evil.package.nml",
            "apps/core/demo.package.nml"
        ]
    );
    let u = d.universe();
    assert_eq!(bound(&governing(&u, &key("apps/core/flow.nml"))).0, "demo");
    assert!(matches!(
        governing(&u, &key("docs/readme.nml")),
        Governing::Unbound
    ));
    assert_eq!(
        bound(&governing(&u, &key("apps/aaa-tenant/own.nml"))).0,
        "evil"
    );
    // The operator's manifest itself is claimed by its OWN glob (a
    // `.nml` under `apps/core/`), never by the sibling.
    assert_eq!(
        bound(&governing(&u, &key("apps/core/demo.package.nml"))).0,
        "demo"
    );
}

#[test]
fn unbound_is_open_iff_no_claims() {
    // Open iff no WORKSPACE manifest was discovered: store, injected and
    // builtin packages bind files but never close a developer's repo.
    let ws = Ws::new().file("x.nml").file("demo.package.nml.bak");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    assert!(!u.is_closed());
    assert!(matches!(governing(&u, &key("x.nml")), Governing::Unbound));
    let d = ws.discover(
        &root,
        vec![external("demo", &[], TENANTS, ExternalClass::Store)],
    );
    let u = d.universe();
    assert!(!u.is_closed(), "a store package alone keeps the repo open");
    assert_eq!(u.workspace_claims(), 0);
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("x.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    assert!(u.is_closed());
    assert_eq!(u.workspace_claims(), 1);
    assert!(matches!(governing(&u, &key("x.nml")), Governing::Unbound));
}

#[test]
fn a_pin_with_two_definitions_of_one_name_in_one_class_is_ambiguous() {
    // A PIN selects among the live definitions of that name in the
    // highest class present, and the same one/more/none rule that
    // governs auto-association governs it: two definitions of the pinned
    // name in ONE class, both claiming the key, are AMBIGUOUS — denied,
    // naming both — never nearest-wins. Two live WORKSPACE definitions
    // can never reach one key (the stem rule refuses two in a directory,
    // R3′ inerts a nested one, R5′ keeps siblings apart), so the arm is
    // reached the way an embedder reaches it: two INJECTED packages of
    // one name, which nothing dedupes — `discover`'s `extra` is the
    // caller's list.
    let ws = Ws::new()
        .config("nml-project.nml", "    schemaPackages:\n        - demo\n")
        .file("apps/a/app.nml");
    let root = ws.root();
    let d = ws.discover(
        &root,
        vec![
            external(
                "demo",
                &[],
                &[("first", &["apps/*/app.nml"])],
                ExternalClass::Injected,
            ),
            external(
                "demo",
                &[],
                &[("second", &["apps/*/app.nml"])],
                ExternalClass::Injected,
            ),
        ],
    );
    let u = d.universe();
    match governing(&u, &key("apps/a/app.nml")) {
        Governing::Ambiguous(claimants) => {
            let bindings: Vec<&str> = claimants.iter().map(|c| c.binding.name.as_str()).collect();
            assert_eq!(bindings, ["first", "second"], "both are named, in order");
        }
        other => panic!("a pin with two definitions of one name denies: {other:?}"),
    }
    // ONE definition of the pinned name still binds through the pin: the
    // denial is the ambiguity, not the pin.
    let d = ws.discover(
        &root,
        vec![external(
            "demo",
            &[],
            &[("first", &["apps/*/app.nml"])],
            ExternalClass::Injected,
        )],
    );
    let u = d.universe();
    match governing(&u, &key("apps/a/app.nml")) {
        Governing::Bound { claimant, step } => {
            assert_eq!(claimant.binding.name, "first");
            assert_eq!(step, BindingStep::Pinned);
        }
        other => panic!("one definition binds: {other:?}"),
    }
}

#[test]
fn workspace_shadows_store_but_store_binds_unclaimed_files() {
    // The class ladder: a workspace `demo` shadows the store's `demo` for
    // auto-association (never ambiguous across classes); a different
    // store package still binds what no workspace manifest claims.
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], TENANTS)
        .file("tenants/cu/x.flow.nml")
        .file("apps/a/app.nml");
    let root = ws.root();
    let store_demo = external("demo", &[], &[("all", &["**"])], ExternalClass::Store);
    let store_other = external(
        "other",
        &[],
        &[("apps", &["apps/*/app.nml"])],
        ExternalClass::Store,
    );
    let d = ws.discover(&root, vec![store_demo, store_other]);
    let u = d.universe();
    let (pkg, binding, ..) = bound(&governing(&u, &key("tenants/cu/x.flow.nml")));
    assert_eq!((pkg.as_str(), binding.as_str()), ("demo", "tenantFlows"));
    let g = governing(&u, &key("apps/a/app.nml"));
    assert_eq!(bound(&g).0, "other");
    assert!(matches!(
        g,
        Governing::Bound {
            step: BindingStep::AutoAssociated,
            ..
        }
    ));
    // Injected beats store for one name.
    let d = ws.discover(
        &root,
        vec![
            external(
                "other",
                &[],
                &[("store", &["apps/*/app.nml"])],
                ExternalClass::Store,
            ),
            external(
                "other",
                &[],
                &[("injected", &["apps/*/app.nml"])],
                ExternalClass::Injected,
            ),
        ],
    );
    let u = d.universe();
    assert_eq!(bound(&governing(&u, &key("apps/a/app.nml"))).1, "injected");
}

#[test]
fn auto_associate_false_and_pin_order() {
    // The nearest live config's `autoAssociate = false` leaves unpinned
    // files unbound; pins bind in list order, first match wins; an
    // invalid pin name is skipped, never joined into anything.
    let ws = Ws::new()
        .config(
            "nml-project.nml",
            "    autoAssociate = false\n    schemaPackages:\n        - \"../../x\"\n        - other\n        - demo\n",
        )
        .manifest("demo.package.nml", "demo", &[], &[("all", &["**/*.nml"])])
        .manifest("other.package.nml", "other", &[], &[("flows", &["**/*.flow.nml"])])
        .file("a.flow.nml")
        .file("b.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    assert_eq!(bound(&governing(&u, &key("a.flow.nml"))).0, "other");
    assert_eq!(bound(&governing(&u, &key("b.nml"))).0, "demo");
    let ws = Ws::new()
        .config("nml-project.nml", "    autoAssociate = false\n")
        .manifest("demo.package.nml", "demo", &[], &[("all", &["**/*.nml"])])
        .file("b.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    assert!(matches!(governing(&u, &key("b.nml")), Governing::Unbound));
    assert!(u.is_closed());
}

#[test]
fn the_nearest_live_config_pins_before_a_shallower_one() {
    // r80-cov (mutant C5 `max_by_key` → `min_by_key` survived): "pins
    // from the NEAREST live config first" — with two live configs on the
    // file's chain whose pin orders disagree, the deeper one decides.
    // Both are live: the manifests sit BESIDE the deeper config (not
    // strictly shallower), so nothing inerts it.
    let ws = Ws::new()
        .config(
            "nml-project.nml",
            "    schemaPackages:\n        - demo\n        - other\n",
        )
        .config(
            "sub/nml-project.nml",
            "    schemaPackages:\n        - other\n        - demo\n",
        )
        .manifest(
            "sub/demo.package.nml",
            "demo",
            &[],
            &[("all", &["**/*.nml"])],
        )
        .manifest(
            "sub/other.package.nml",
            "other",
            &[],
            &[("flows", &["**/*.flow.nml"])],
        )
        .file("sub/a.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert!(d.inert.is_empty(), "both configs are live: {:?}", d.inert);
    assert!(d.load_errors.is_empty(), "{:?}", d.load_errors);
    let u = d.universe();
    let (pkg, binding, _, _) = bound(&governing(&u, &key("sub/a.flow.nml")));
    assert_eq!(
        (pkg.as_str(), binding.as_str()),
        ("other", "flows"),
        "the nearest config's order decides, not the root's"
    );
    assert!(matches!(
        governing(&u, &key("sub/a.flow.nml")),
        Governing::Bound {
            step: BindingStep::Pinned,
            ..
        }
    ));
}

#[test]
fn claim_identity_renders_one_label() {
    // A store claim is minted by the walk from the `ExternalClaim` handed
    // in (r89: the walk is the only minter, by type).
    let store = external("demo", &[], TENANTS, ExternalClass::Store);
    let ws = Ws::new();
    let root = ws.root();
    let d = ws.discover(&root, vec![store]);
    let claim = &d.claims[0];
    let rendered = claim.identity().render();
    assert!(rendered.starts_with("demo blake3:"), "{rendered}");
    assert!(rendered.ends_with(", store current"), "{rendered}");
    assert_eq!(claim.manifest_label, "<store current>");
    let ws = Ws::new().manifest("schemas/demo.package.nml", "demo", &[], TENANTS);
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    assert_eq!(d.claims[0].manifest_label, "schemas/demo.package.nml");
    assert!(
        d.claims[0]
            .identity()
            .render()
            .ends_with(", workspace manifest")
    );
}

/// The claim door (r89 P10): a `ManifestClaim` is minted by the walk
/// alone — every field but the package is crate-private and no
/// constructor is `pub`, so a claim cannot be assembled outside the
/// kernel (a workspace claim is unrepresentable from another crate). A
/// source ratchet: the compile-fail probe that proved it cannot be a
/// test, and a field made `pub` would compile every pin green.
#[test]
fn a_manifest_claim_cannot_be_assembled_outside_the_kernel() {
    let src = include_str!("../claims.rs");
    let body = src
        .split("pub struct ManifestClaim {")
        .nth(1)
        .and_then(|rest| rest.split_once('}'))
        .map(|(body, _)| body)
        .expect("the struct");
    for field in ["origin", "content_hash", "manifest_label"] {
        assert!(
            body.contains(&format!("pub(crate) {field}:")),
            "{field} must stay crate-private:\n{body}"
        );
        assert!(
            !body.contains(&format!("pub {field}:")),
            "{field} is public:\n{body}"
        );
    }
    assert!(body.contains("pub package:"), "{body}");
    let impl_block = src
        .split("impl ManifestClaim {")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("the impl");
    for ctor in ["fn new(", "fn workspace(", "fn external("] {
        let line = impl_block
            .lines()
            .find(|l| l.contains(ctor))
            .unwrap_or_else(|| panic!("{ctor} exists"));
        assert!(
            !line.trim_start().starts_with("pub fn"),
            "{ctor} is public: {line}"
        );
    }
}
