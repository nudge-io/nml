//! Grant pins (step 0c): the owned [`Grant`] a resolved file carries —
//! the engine's lookup shapes over a real universe, deny-first byte-exact
//! matching, and the allow-miss non-disclosure through the engine itself.

use super::*;
use nml_core::diagnostic::codes;
use nml_core::layers::{
    GrantLookup, LayerGrant, LayerGrantProvider, ManifestHome, RefDecision, UnboundContext,
};

const TENANTS: &[(&str, &[&str])] = &[("tenantFlows", &["tenants/**/*.flow.nml"])];

fn grant(allow: &[&str], deny: &[&str]) -> LayerGrant {
    LayerGrant {
        allow_refs: allow.iter().map(|s| s.to_string()).collect(),
        deny_refs: deny.iter().map(|s| s.to_string()).collect(),
        max_stack_depth: None,
    }
}

/// The universe's grant for `key`, as `resolve_file` copies it out of
/// the governing binding.
fn grant_of(u: &Universe<'_>, key: &SourceKey) -> Grant {
    Grant::of(u, &governing(u, key))
}

#[test]
fn grant_for_matches_engine_lookup_shapes() {
    // Bound without `layers:` ⇒ NoGrant naming binding and manifest KEY.
    let ws = Ws::new()
        .manifest("schemas/demo.package.nml", "demo", &[], &[("all", &["**"])])
        .file("schemas/x.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let file = key("schemas/x.flow.nml");
    let r = grant_of(&u, &file);
    match r.grant_for("schemas/x.flow.nml") {
        GrantLookup::NoGrant {
            binding, manifest, ..
        } => {
            assert_eq!((binding, manifest), ("all", "schemas/demo.package.nml"));
        }
        other => panic!("{other:?}"),
    }
    // Bound WITH a grant ⇒ Granted carrying it.
    let mut d = d;
    let g = grant(&["vendor/**"], &[]);
    Arc::get_mut(&mut d.claims[0].package)
        .unwrap()
        .manifest
        .validators[0]
        .layers = Some(g.clone());
    let u = d.universe();
    let r = grant_of(&u, &file);
    assert!(matches!(
        r.grant_for("schemas/x.flow.nml"),
        GrantLookup::Granted { grant, binding: "all", manifest: "schemas/demo.package.nml" } if *grant == g
    ));
    // Ambiguous ⇒ both manifest keys (two live root manifests, `demo`
    // and `other`, both claiming the file — since E35's stem rule a
    // second `demo` under another file name is a load error instead).
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
    let u = d.universe();
    let file = key("tenants/cu/x.flow.nml");
    let r = grant_of(&u, &file);
    match r.grant_for("tenants/cu/x.flow.nml") {
        GrantLookup::Ambiguous { mut manifests } => {
            manifests.sort_unstable();
            assert_eq!(manifests, ["demo.package.nml", "other.package.nml"]);
        }
        other => panic!("{other:?}"),
    }
    // Unbound in a closed universe ⇒ Closed { root, claims }; open ⇒ Open.
    let file = key("docs/y.nml");
    let r = grant_of(&u, &file);
    assert_eq!(
        match r.grant_for("docs/y.nml") {
            GrantLookup::Unbound { context } => context,
            other => panic!("{other:?}"),
        },
        UnboundContext::Closed { claims: 2 }
    );
    let ws = Ws::new().file("docs/y.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let r = grant_of(&u, &file);
    assert!(matches!(
        r.grant_for("docs/y.nml"),
        GrantLookup::Unbound {
            context: UnboundContext::Open
        }
    ));
    assert_eq!(r, Grant::open(), "no universe closes it: the open context");
    // Truncated ⇒ closed-denied even for a file a binding would claim —
    // and denied in FULL (E28): nothing was loaded, so the closed form
    // counts no claims; the truncation error is the one note. (The
    // unlistable directory sits in the ROOT unit — under `**` it would
    // be a unit of its own and denied alone.)
    let ws = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("all", &["tenants/**/*.flow.nml"])],
        )
        .file("tenants/cu/x.flow.nml")
        .denied("locked");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let file = key("tenants/cu/x.flow.nml");
    let r = grant_of(&u, &file);
    assert!(matches!(
        r.grant_for("tenants/cu/x.flow.nml"),
        GrantLookup::Unbound {
            context: UnboundContext::Closed { claims: 0 }
        }
    ));
}

#[test]
fn ref_decision_deny_first_byte_exact() {
    let ws = Ws::new().file("x.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let file = key("x.nml");
    let r = grant_of(&u, &file);
    let g = grant(
        &["vendor/**", "tenants/cu-xyz/**"],
        &["vendor/admin/**", "vendor/lib/secret.nml"],
    );
    assert_eq!(
        r.ref_decision(&g, "vendor/lib/base.nml"),
        RefDecision::Allowed
    );
    // Deny wins, named by index.
    assert_eq!(
        r.ref_decision(&g, "vendor/admin/x.nml"),
        RefDecision::DenyVeto(0)
    );
    assert_eq!(
        r.ref_decision(&g, "vendor/lib/secret.nml"),
        RefDecision::DenyVeto(1)
    );
    assert_eq!(
        r.ref_decision(&g, "tenants/other/x.nml"),
        RefDecision::AllowMiss
    );
    // Byte-exact: case never folds, in either direction.
    assert_eq!(
        r.ref_decision(&g, "Vendor/lib/base.nml"),
        RefDecision::AllowMiss
    );
    assert_eq!(
        r.ref_decision(&g, "vendor/Admin/x.nml"),
        RefDecision::Allowed
    );
    // An empty allowlist denies all; a `..` never matches (keys carry none).
    assert_eq!(
        r.ref_decision(&grant(&[], &[]), "vendor/x.nml"),
        RefDecision::AllowMiss
    );
    // Over-cap globs match nothing — fail-closed for an allow (item 4's
    // NML2081 rejects them at load so a deny can never be fail-open).
    let huge = (0..65).map(|_| "a").collect::<Vec<_>>().join("/");
    assert_eq!(
        r.ref_decision(&grant(&[&huge], &[]), &huge),
        RefDecision::AllowMiss
    );
}

#[test]
fn allow_miss_never_names_path() {
    // Through the engine: a bound file whose binding carries an empty
    // grant composes a same-file stack — the allow-miss names the
    // binding and the manifest key, never the denied target's path.
    let ws = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("tenantFlows", &["tenants/**"])],
        )
        .file("tenants/cu/x.nml");
    let root = ws.root();
    let mut d = ws.discover(&root, vec![]);
    Arc::get_mut(&mut d.claims[0].package)
        .unwrap()
        .manifest
        .validators[0]
        .layers = Some(grant(&[], &[]));
    let u = d.universe();
    let file = key("tenants/cu/x.nml");
    let source_name = "tenants/cu/x.nml";
    let r = grant_of(&u, &file);
    let (parsed, diags) = nml_core::cst::parse_to_ast_all(
        "thing base:\n    v = \"b\"\n\nthing t uses base:\n    v = \"t\"\n",
    );
    assert!(diags.is_empty());
    let index = nml_core::schema_index::SchemaIndex::build(vec![], vec![], vec![]);
    let composed = nml_core::layers::compose_file(&index, source_name, &parsed, &r);
    let denial = composed
        .diagnostics
        .iter()
        .find(|d| d.code == Some(codes::LAYER_REF_DENIED))
        .expect("denied");
    assert!(
        denial
            .message
            .contains("no allowRefs entry of binding 'tenantFlows' (demo.package.nml)"),
        "{}",
        denial.message
    );
    let clause = denial
        .message
        .split(" — an operator change")
        .next()
        .unwrap();
    assert!(!clause.contains("tenants/cu/x.nml"), "{clause}");
    // No grant at all ⇒ NML2064 naming binding and manifest key.
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let r = grant_of(&u, &file);
    let composed = nml_core::layers::compose_file(&index, source_name, &parsed, &r);
    let denial = composed
        .diagnostics
        .iter()
        .find(|d| d.code == Some(codes::COMPOSITION_DENIED))
        .expect("denied");
    assert!(
        denial
            .message
            .contains("binding 'tenantFlows' (demo.package.nml) carries no `layers:` grant"),
        "{}",
        denial.message
    );
}

/// The grant a resolved file carries IS the universe's verdict for its
/// key — `resolve_file` copies it out of the governing binding it
/// already computed, so no front end asks `governing` a second time —
/// and a path the closed universe rejects (NML2083: no key was verified,
/// nothing governs it) carries the unbound form naming that universe.
#[test]
fn a_resolved_file_carries_the_universes_grant() {
    let ws = Ws::new()
        .manifest("demo.package.nml", "demo", &[], &[("all", &["**"])])
        .file("x.nml")
        .file("vendor/base.flow.nml")
        .symlink("tenants/cu/lib", "../../vendor");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let bound = crate::workspace::discover::resolve_file(&u, Path::new("/ws/x.nml"), &ws.fs)
        .expect("resolves");
    assert_eq!(
        bound.grant,
        Grant::NoGrant {
            binding: "all".to_string(),
            manifest: "demo.package.nml".to_string(),
            package: "demo".to_string(),
            home: ManifestHome::Workspace {
                at: d.claims[0].package.manifest.validators[0].span,
            },
        }
    );
    assert_eq!(bound.grant, grant_of(&u, &key("x.nml")));
    let rejected = crate::workspace::discover::resolve_file(
        &u,
        Path::new("/ws/tenants/cu/lib/base.flow.nml"),
        &ws.fs,
    )
    .expect("a rejection is a resolution");
    assert_eq!(
        rejected.findings[0].code,
        Some(codes::SYMLINKED_CONTENT_REJECTED)
    );
    assert!(matches!(rejected.governing, Governing::Unbound));
    assert_eq!(rejected.grant, Grant::Unbound { closed: Some(1) });
    assert_eq!(rejected.grant, Grant::unbound(&u));
}

/// RFC 0026 B-1: the manifest's `layers:` block IS the binding's grant
/// the universe hands the engine (one grant provider, both front ends):
/// a bound file's `Grant::Granted` carries the block as loaded, and the
/// no-grant verdict of a WORKSPACE manifest carries the binding's own
/// name span (the located remedy's anchor) — an injected package's
/// carries none: nobody edits that manifest here.
#[test]
fn a_manifest_layers_block_is_the_bindings_grant() {
    let manifest = "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    - core:\n        \
                    file = \"core.model.nml\"\n\n[]validator validators:\n    - vendorFlows:\n        files:\n            - \
                    \"vendor/**/*.flow.nml\"\n        schemas:\n            - core\n        layers:\n            allowRefs:\n                - \
                    \"vendor/**\"\n            denyRefs:\n                - \"vendor/vetoed/**\"\n            maxStackDepth = 4\n    - \
                    tenantFlows:\n        files:\n            - \"tenants/**/*.flow.nml\"\n        schemas:\n            - core\n";
    let ws = Ws::new()
        .text("demo.package.nml", manifest)
        .text("core.model.nml", crate::test_support::DEMO_CORE)
        .file("vendor/x.flow.nml")
        .file("tenants/cu/x.flow.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let u = d.universe();
    let vendor =
        crate::workspace::discover::resolve_file(&u, Path::new("/ws/vendor/x.flow.nml"), &ws.fs)
            .expect("resolves");
    let mut expected = grant(&["vendor/**"], &["vendor/vetoed/**"]);
    expected.max_stack_depth = Some(4);
    assert_eq!(
        vendor.grant,
        Grant::Granted {
            grant: expected.clone(),
            binding: "vendorFlows".to_string(),
            manifest: "demo.package.nml".to_string(),
        }
    );
    // The engine's two questions, answered from the block.
    assert_eq!(
        vendor
            .grant
            .ref_decision(&expected, "vendor/lib/base.flow.nml"),
        RefDecision::Allowed
    );
    assert_eq!(
        vendor
            .grant
            .ref_decision(&expected, "vendor/vetoed/v.flow.nml"),
        RefDecision::DenyVeto(0)
    );
    assert_eq!(
        vendor
            .grant
            .ref_decision(&expected, "tenants/cu/x.flow.nml"),
        RefDecision::AllowMiss
    );
    // No block: the no-grant verdict, located at the binding's name.
    let tenant = crate::workspace::discover::resolve_file(
        &u,
        Path::new("/ws/tenants/cu/x.flow.nml"),
        &ws.fs,
    )
    .expect("resolves");
    let at = d.claims[0].package.manifest.validators[1].span;
    assert_eq!(&manifest[at.start..at.end], "tenantFlows");
    assert_eq!(
        tenant.grant,
        Grant::NoGrant {
            binding: "tenantFlows".to_string(),
            manifest: "demo.package.nml".to_string(),
            package: "demo".to_string(),
            home: ManifestHome::Workspace { at },
        }
    );
    // An injected package's binding: the same verdict, no editable span —
    // the home names the class, so the denial names the change it needs.
    let ws = Ws::new().file("tenants/cu/x.flow.nml");
    let root = ws.root();
    let d = ws.discover(
        &root,
        vec![external("demo", &[], TENANTS, ExternalClass::Injected)],
    );
    let u = d.universe();
    let injected = grant_of(&u, &key("tenants/cu/x.flow.nml"));
    assert!(
        matches!(
            &injected,
            Grant::NoGrant { home: ManifestHome::External(ExternalClass::Injected), binding, package, .. }
                if binding == "tenantFlows" && package == "demo"
        ),
        "{injected:?}"
    );
}
