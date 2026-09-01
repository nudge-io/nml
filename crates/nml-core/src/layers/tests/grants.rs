use super::super::*;
use super::*;

#[test]
fn no_grant_is_2064_naming_binding_and_manifest() {
    fn lookup(_: &str) -> GrantLookup<'static> {
        GrantLookup::NoGrant {
            binding: "tenantFlows",
            manifest: "nml-package.nml",
        }
    }
    let (resolved, diags) =
        compose_with(LIN_SCHEMA, BASE_AND_T, "thing", "t", &TestGrants { lookup });
    assert!(resolved.is_none());
    assert_eq!(codes_of(&diags), [codes::COMPOSITION_DENIED]);
    assert!(diags[0].message.contains("tenantFlows"));
    assert!(diags[0].message.contains("nml-package.nml"));
    assert!(diags[0].message.contains("nml binding"));
}

#[test]
fn ambiguous_claim_is_2064_naming_both() {
    fn lookup(_: &str) -> GrantLookup<'static> {
        GrantLookup::Ambiguous {
            manifests: vec!["a/nml-package.nml", "b/nml-package.nml"],
        }
    }
    let (resolved, diags) =
        compose_with(LIN_SCHEMA, BASE_AND_T, "thing", "t", &TestGrants { lookup });
    assert!(resolved.is_none());
    assert_eq!(codes_of(&diags), [codes::COMPOSITION_DENIED]);
    assert!(diags[0].message.contains("a/nml-package.nml"));
    assert!(diags[0].message.contains("b/nml-package.nml"));
}

#[test]
fn unbound_closed_is_2064_and_unbound_open_composes() {
    fn closed(_: &str) -> GrantLookup<'static> {
        GrantLookup::Unbound {
            open_context: false,
        }
    }
    let (resolved, diags) = compose_with(
        LIN_SCHEMA,
        BASE_AND_T,
        "thing",
        "t",
        &TestGrants { lookup: closed },
    );
    assert!(resolved.is_none());
    assert_eq!(codes_of(&diags), [codes::COMPOSITION_DENIED]);

    let (resolved, diags) = compose(LIN_SCHEMA, BASE_AND_T, "thing", "t");
    assert!(diags.is_empty(), "{diags:?}");
    assert!(resolved.is_some());
}

#[test]
fn allow_miss_denies_without_naming_path() {
    static GRANT: LayerGrant = LayerGrant {
        allow_refs: Vec::new(),
        deny_refs: Vec::new(),
        max_stack_depth: None,
    };
    fn lookup(_: &str) -> GrantLookup<'static> {
        GrantLookup::Granted {
            grant: &GRANT,
            binding: "tenantFlows",
            manifest: "nml-package.nml",
        }
    }
    let (resolved, diags) =
        compose_with(LIN_SCHEMA, BASE_AND_T, "thing", "t", &TestGrants { lookup });
    assert!(resolved.is_none());
    assert!(codes_of(&diags).contains(&codes::LAYER_REF_DENIED));
    assert!(diags[0].message.contains("no allowRefs entry"));
    // The denial CLAUSE never names the denied target's path; the
    // recovery tail names the CHECKED file (the author's own — the
    // contract's `nml binding <file>` pointer), which in this
    // single-file harness is the same string — so split the tail off
    // before asserting non-disclosure.
    let clause = diags[0]
        .message
        .split(" — an operator change")
        .next()
        .unwrap();
    assert!(
        !clause.contains("main.nml"),
        "allow-miss never names the denied path: {clause}"
    );
    assert!(
        diags[0].message.ends_with("run `nml binding main.nml`"),
        "recovery pointer names the checked file: {}",
        diags[0].message
    );
}

#[test]
fn grant_depth_cap_is_2066_naming_operator_change() {
    static GRANT: LayerGrant = LayerGrant {
        allow_refs: Vec::new(),
        deny_refs: Vec::new(),
        max_stack_depth: Some(1),
    };
    fn lookup(_: &str) -> GrantLookup<'static> {
        GrantLookup::Granted {
            grant: &GRANT,
            binding: "b",
            manifest: "m",
        }
    }
    struct AllowAll;
    impl LayerGrantProvider for AllowAll {
        fn grant_for(&self, _: &str) -> GrantLookup<'_> {
            lookup("")
        }
        fn ref_decision(&self, _: &LayerGrant, _: &str) -> RefDecision {
            RefDecision::Allowed
        }
    }
    let (resolved, diags) = compose_with(LIN_SCHEMA, BASE_AND_T, "thing", "t", &AllowAll);
    assert!(resolved.is_none());
    assert!(codes_of(&diags).contains(&codes::LAYER_BOUND_EXCEEDED));
    assert!(diags.iter().any(|d| d.message.contains("operator change")));
}

#[test]
fn deny_veto_names_rule_index() {
    static GRANT: LayerGrant = LayerGrant {
        allow_refs: Vec::new(),
        deny_refs: Vec::new(),
        max_stack_depth: None,
    };
    struct VetoAll;
    impl LayerGrantProvider for VetoAll {
        fn grant_for(&self, _: &str) -> GrantLookup<'_> {
            GrantLookup::Granted {
                grant: &GRANT,
                binding: "tenantFlows",
                manifest: "m",
            }
        }
        fn ref_decision(&self, _: &LayerGrant, _: &str) -> RefDecision {
            RefDecision::DenyVeto(2)
        }
    }
    let (resolved, diags) = compose_with(LIN_SCHEMA, BASE_AND_T, "thing", "t", &VetoAll);
    assert!(resolved.is_none());
    assert!(codes_of(&diags).contains(&codes::LAYER_REF_DENIED));
    assert!(
        diags.iter().any(|d| d.message.contains("denyRefs[2]")),
        "deny-veto names the rule by index: {diags:?}"
    );
}

#[test]
fn stack_level_denial_wording_contract() {
    // Stack-level allow-miss names the BINDING and the entering ref —
    // never the denied layer's author-chosen instance name (that
    // would leak through the denial); site-level names the author's
    // own listed token. Deny-veto may name (the allow admitted it).
    let gref = GrantRef {
        binding: "tenantFlows",
        manifest: "site/nml.binding.toml",
        file: "tenants/cu-x/a.flow.nml",
    };
    let d = ref_denial(
        RefDecision::AllowMiss,
        "secretName",
        &gref,
        Denial::Stack {
            entering: Some("vendorBase"),
        },
    )
    .unwrap();
    assert!(
        !d.message.contains("secretName"),
        "no denied-layer disclosure: {}",
        d.message
    );
    assert!(
        d.message
            .contains("'vendorBase' in this clause pulls it in")
    );
    // The denial-family contract tail: binding AND manifest named,
    // operator ownership stated, recovery pointer with the real path.
    assert!(
        d.message.contains("(site/nml.binding.toml)"),
        "manifest named: {}",
        d.message
    );
    assert!(
        d.message
            .ends_with("run `nml binding tenants/cu-x/a.flow.nml`"),
        "recovery pointer last, real path: {}",
        d.message
    );
    let d = ref_denial(RefDecision::AllowMiss, "ownRef", &gref, Denial::Site).unwrap();
    assert!(
        d.message.contains("ownRef"),
        "site names the author's token"
    );
    let d = ref_denial(
        RefDecision::DenyVeto(2),
        "x",
        &gref,
        Denial::Stack { entering: None },
    )
    .unwrap();
    assert!(d.message.contains("denyRefs[2]"));
    assert!(d.message.contains("stack-level"));
}
