use super::super::*;
use super::*;
use crate::diagnostic::{Suggestion, SuggestionKind};

/// A manifest from outside the walk is nobody's to edit here: no note,
/// no insertion — the sentence itself is the remedy, and it says WHERE
/// the manifest lives and WHAT change it needs, by class: the store's
/// current copy (republish the package), a package embedded in the
/// binary (rebuild the embedder), nml's builtin (write the manifest
/// without `uses`). One line, the binding and its manifest label named,
/// the `nml binding` pointer kept.
#[test]
fn no_grant_under_an_external_manifest_names_where_it_lives_and_the_next_action() {
    use std::sync::atomic::{AtomicU8, Ordering};
    static CLASS: AtomicU8 = AtomicU8::new(0);
    fn lookup(_: &str) -> GrantLookup<'static> {
        let (manifest, class) = match CLASS.load(Ordering::Relaxed) {
            0 => ("<store current>", ExternalClass::Store),
            1 => ("<in-binary>", ExternalClass::Injected),
            _ => ("<builtin>", ExternalClass::Builtin),
        };
        GrantLookup::NoGrant {
            binding: "tenantFlows",
            manifest,
            package: "demo",
            home: ManifestHome::External(class),
        }
    }
    let cases = [
        (
            0,
            "<store current>",
            "the store's current copy of package `demo`, never edited in place: add the grant in \
             the package's source and republish it",
        ),
        (
            1,
            "<in-binary>",
            "package `demo`, embedded in this binary by its embedder and never edited in place: \
             add the grant in the embedded package's source and rebuild",
        ),
        (
            2,
            "<builtin>",
            "nml's own builtin package `demo`, which grants no composition to the manifests it \
             binds: write the manifest whole, without `uses`",
        ),
    ];
    for (class, label, sentence) in cases {
        CLASS.store(class, Ordering::Relaxed);
        let (resolved, diags) =
            compose_with(LIN_SCHEMA, BASE_AND_T, "thing", "t", &TestGrants { lookup });
        assert!(resolved.is_none());
        assert_eq!(codes_of(&diags), [codes::COMPOSITION_DENIED]);
        let message = &diags[0].message;
        assert!(!message.contains('\n'), "one line: {message}");
        assert!(
            message.starts_with(&format!(
                "composition not permitted: binding 'tenantFlows' ({label}) carries no `layers:` \
                 grant — the manifest is {sentence}; run `nml binding "
            )),
            "{message}"
        );
        assert!(
            !message.contains("operator change"),
            "the workspace sentence never names an external manifest: {message}"
        );
        assert!(diags[0].related.is_empty(), "{:?}", diags[0].related);
        assert!(
            diags[0].suggestions.is_empty(),
            "{:?}",
            diags[0].suggestions
        );
    }
}

/// RFC 0026 B-1: with the binding's span in an editable manifest, the
/// no-grant denial carries its remedy as a LOCATED note in that
/// manifest — at the binding, naming the exact key an `allowRefs` entry
/// must admit (the referenced instance's defining file: the checked
/// file's own while refs are same-file) — and as the STRUCTURED
/// insertion of the block under that binding, in that manifest (rustc's
/// suggestion with a replacement): zero-indent, the resolver re-indents
/// it to the body's own. The message itself is unchanged: a finding's
/// message is one sanitized line by contract.
#[test]
fn no_grant_with_an_editable_manifest_carries_a_located_remedy() {
    fn lookup(_: &str) -> GrantLookup<'static> {
        GrantLookup::NoGrant {
            binding: "tenantFlows",
            manifest: "nml-package.nml",
            package: "demo",
            home: ManifestHome::Workspace {
                at: Span::new(120, 131),
            },
        }
    }
    let (resolved, diags) =
        compose_with(LIN_SCHEMA, BASE_AND_T, "thing", "t", &TestGrants { lookup });
    assert!(resolved.is_none());
    assert_eq!(codes_of(&diags), [codes::COMPOSITION_DENIED]);
    assert!(
        !diags[0].message.contains('\n'),
        "one line: {}",
        diags[0].message
    );
    let [note] = diags[0].related.as_slice() else {
        panic!("{:?}", diags[0].related);
    };
    assert_eq!(note.span, Span::new(120, 131));
    assert_eq!(note.source.as_deref(), Some("nml-package.nml"));
    assert_eq!(
        note.message,
        "to permit it, give this binding a `layers:` grant whose `allowRefs` admits \"main.nml\""
    );
    let [block] = diags[0].suggestions.as_slice() else {
        panic!("{:?}", diags[0].suggestions);
    };
    assert_eq!(
        *block,
        Suggestion {
            replacement: "layers:\n    allowRefs:\n        - \"main.nml\"".to_string(),
            span: Span::new(120, 131),
            kind: SuggestionKind::Insert,
            source: Some("nml-package.nml".to_string()),
        }
    );
    assert_eq!(diags[0].suggestion_source(block), Some("nml-package.nml"));
}

/// The remedy block is exact for ANY key the walk can mint: a name
/// carrying a control character, a bidi override, a quote, a tab, a
/// line break or a non-ASCII glyph is spelled as the manifest's own
/// string literal, so the inserted `allowRefs` entry decodes to the key
/// itself — resolved through the one insertion engine, spliced, parsed,
/// decoded. (The spelling is the sanitizer's escape form too: nothing a
/// terminal or a JSON line must not carry ever rides the block raw.)
#[test]
fn the_remedy_block_spells_any_key_as_the_manifest_reads_it() {
    use crate::cst::{SyntaxKind, decode_value, edit, parse};
    let manifest =
        "[]validator validators:\n    - b:\n        files:\n            - \"tenants/**\"\n";
    let at = manifest.find("- b:").expect("the item") + 2;
    for key in [
        "tenants/a\u{1b}[31mb.flow.nml",
        "tenants/a\u{202e}b.flow.nml",
        "tenants/a\u{feff}b.flow.nml",
        "tenants/a\u{2028}b.flow.nml",
        "tenants/a\u{85}b.flow.nml",
        "tenants/a\u{e0041}b.flow.nml",
        "tenants/a\"b.flow.nml",
        "tenants/a\tb.flow.nml",
        "tenants/a\nb.flow.nml",
        "tenants/a\r\nb.flow.nml",
        "tenants/caf\u{e9}\u{2014}b.flow.nml",
        "tenants/a'b.flow.nml",
        "tenants/a{{b}}.flow.nml",
        "tenants/{{x}}/y.flow.nml",
    ] {
        let snippet = no_grant_snippet(&[key]);
        assert!(
            !snippet
                .chars()
                .any(|c| c != '\n' && crate::diagnostic::needs_escape(c)),
            "the block carries nothing raw a surface must escape: {snippet:?}"
        );
        let insert = Suggestion {
            replacement: snippet,
            span: Span::new(at, at + 1),
            kind: SuggestionKind::Insert,
            source: None,
        };
        let resolved = edit::resolve_suggestions(manifest, std::slice::from_ref(&insert));
        assert_eq!(
            resolved.outcomes,
            [Ok(edit::Applied::Inserted {
                head: "layers".to_string(),
                into: "b".to_string(),
            })],
            "{key:?}"
        );
        let out = edit::splice(manifest, &resolved.edits).expect("splice-ready");
        let parsed = parse(&out);
        assert!(parsed.errors().is_empty(), "{key:?}: {out}");
        let values: Vec<String> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::Value)
            .filter_map(|n| decode_value(&n).ok())
            .filter_map(|v| match v.value {
                crate::types::Value::String(s) => Some(s),
                _ => None,
            })
            .collect();
        assert!(
            values.iter().any(|v| v == key),
            "{key:?} decodes from {out}"
        );
    }
}

/// The remedy names each referenced file's key ONCE (two refs into one
/// file, one key — in the note and in the block alike), and —
/// where the denial arises with no listed refs in hand (a layer's own
/// site check inside a stack, `linearize`'s call) — the declaring
/// instance's own key, never a placeholder.
#[test]
fn the_remedy_names_each_key_once_and_falls_back_to_the_declaring_file() {
    fn lookup(_: &str) -> GrantLookup<'static> {
        GrantLookup::NoGrant {
            binding: "b",
            manifest: "m.package.nml",
            package: "m",
            home: ManifestHome::Workspace {
                at: Span::new(1, 2),
            },
        }
    }
    let two_refs = "\
thing base:
    v = \"b\"

thing base2:
    v = \"c\"

thing t uses base, base2:
    v = \"t\"
";
    let (resolved, diags) =
        compose_with(LIN_SCHEMA, two_refs, "thing", "t", &TestGrants { lookup });
    assert!(resolved.is_none());
    assert_eq!(codes_of(&diags), [codes::COMPOSITION_DENIED]);
    let [note] = diags[0].related.as_slice() else {
        panic!("{:?}", diags[0].related);
    };
    assert!(
        note.message.ends_with("admits \"main.nml\""),
        "{}",
        note.message
    );
    assert_eq!(
        note.message.matches("\"main.nml\"").count(),
        1,
        "{}",
        note.message
    );
    let [block] = diags[0].suggestions.as_slice() else {
        panic!("{:?}", diags[0].suggestions);
    };
    assert_eq!(
        block.replacement.matches("\"main.nml\"").count(),
        1,
        "{block:?}"
    );
    // No refs in hand: the declaring file's own key.
    let file = file_of(two_refs);
    let instances = InstanceIndex::from_file("lib/other.nml", &file);
    let id = instances.resolve_ref("t").expect("indexed");
    let block = instances.get(id).expect("declared");
    let denial = deny_diagnostic(&lookup(""), id, block, &[]).expect("denied");
    assert!(
        denial.related[0]
            .message
            .ends_with("admits \"lib/other.nml\""),
        "{}",
        denial.related[0].message
    );
    assert!(
        denial.suggestions[0]
            .replacement
            .contains("- \"lib/other.nml\""),
        "{:?}",
        denial.suggestions[0]
    );
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
fn unbound_closed_names_the_claim_count() {
    // E22: the closed form names the UNIVERSE by its discovered-claim
    // count — the root is the run's fact, stated once per front end,
    // never embedded per file (r89 D8) — and ends with the denial
    // family's recovery pointer. Byte-pinned: the CLI's golden output
    // and the error index quote this shape.
    fn closed(_: &str) -> GrantLookup<'static> {
        GrantLookup::Unbound {
            context: UnboundContext::Closed { claims: 2 },
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
    assert_eq!(
        diags[0].message,
        "composition not permitted: no binding governs this file in the \
         closed universe (2 manifest(s) discovered) — add a `files` glob \
         that claims it (an operator change), then run `nml binding \
         main.nml`"
    );
}

#[test]
fn unbound_open_composes_unchanged() {
    fn open(_: &str) -> GrantLookup<'static> {
        GrantLookup::Unbound {
            context: UnboundContext::Open,
        }
    }
    let (resolved, diags) = compose_with(
        LIN_SCHEMA,
        BASE_AND_T,
        "thing",
        "t",
        &TestGrants { lookup: open },
    );
    assert!(diags.is_empty(), "{diags:?}");
    assert!(resolved.is_some());
    // `OpenContext` IS the open unbound context — one provider, one
    // lookup shape, so the two can never drift.
    assert!(matches!(
        OpenContext.grant_for("main.nml"),
        GrantLookup::Unbound {
            context: UnboundContext::Open
        }
    ));
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

/// `LayerGrant::rules` is the ONE spelling of a grant's rows for every
/// human surface (`nml binding`'s rows, the editor's hover): the allow
/// globs first, then the deny globs, each indexed and quoted as the
/// manifest reads it — through the one string speller, so a glob
/// carrying `{{` or a control character is spelled as a literal the
/// manifest would read back, never as Rust's `{:?}` — then the cap, and
/// no cap row when the grant sets none.
#[test]
fn a_grants_rules_are_spelled_once_in_declaration_order() {
    let grant = LayerGrant {
        allow_refs: vec![
            "tenants/**".to_string(),
            "a\"b".to_string(),
            "t/{{x}}/**".to_string(),
            "t/\u{1b}/**".to_string(),
        ],
        deny_refs: vec!["tenants/cu/**".to_string()],
        max_stack_depth: Some(3),
    };
    assert_eq!(
        grant.rules().collect::<Vec<_>>(),
        [
            "allowRefs[0] = \"tenants/**\"",
            "allowRefs[1] = \"a\\\"b\"",
            "allowRefs[2] = \"t/\\u{7B}{x}}/**\"",
            "allowRefs[3] = \"t/\\u{1B}/**\"",
            "denyRefs[0] = \"tenants/cu/**\"",
            "maxStackDepth = 3",
        ]
    );
    let uncapped = LayerGrant {
        max_stack_depth: None,
        ..grant
    };
    assert_eq!(uncapped.rules().count(), 5);
}

/// The remedy block nests by the canonical unit the insertion engine
/// decodes (`INDENT_UNIT`): one step for `allowRefs:`, two for each
/// entry — the producer spells the unit, never a width of its own.
#[test]
fn the_remedy_block_nests_by_the_canonical_unit() {
    let unit = crate::cst::INDENT_UNIT;
    let snippet = no_grant_snippet(&["a.nml", "b.nml"]);
    let lines: Vec<&str> = snippet.lines().collect();
    assert_eq!(lines[0], "layers:");
    assert_eq!(lines[1], format!("{unit}allowRefs:"));
    assert_eq!(lines[2], format!("{unit}{unit}- \"a.nml\""));
    assert_eq!(lines[3], format!("{unit}{unit}- \"b.nml\""));
    assert_eq!(lines.len(), 4);
}
