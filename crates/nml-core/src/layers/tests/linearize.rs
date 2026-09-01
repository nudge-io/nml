use super::super::*;
use super::*;

#[test]
fn c3_redundant_but_legal_order_composes() {
    let src = "\
thing base:
    v = \"b\"

thing mid uses base:
    v = \"m\"

thing top uses base, mid:
    v = \"t\"
";
    let (resolved, diags) = compose(LIN_SCHEMA, src, "thing", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    assert_eq!(scalar(&body, "v"), Some(&Value::String("t".into())));
}

#[test]
fn c3_mirror_order_is_2077() {
    let src = "\
thing base:
    v = \"b\"

thing mid uses base:
    v = \"m\"

thing top uses mid, base:
    v = \"t\"
";
    let (resolved, diags) = compose(LIN_SCHEMA, src, "thing", "top");
    assert!(resolved.is_none());
    assert_eq!(codes_of(&diags), [codes::INCONSISTENT_LINEARIZATION]);
}

#[test]
fn sibling_subtree_contradiction_is_2077() {
    let src = "\
thing slow:
    v = \"s\"

thing fast:
    v = \"f\"

thing vendorX uses slow, fast:
    v = \"x\"

thing productY uses fast, slow:
    v = \"y\"

thing tenant uses vendorX, productY:
    v = \"t\"
";
    let (resolved, diags) = compose(LIN_SCHEMA, src, "thing", "tenant");
    assert!(resolved.is_none());
    assert!(codes_of(&diags).contains(&codes::INCONSISTENT_LINEARIZATION));
}

#[test]
fn diamond_composes_shared_base_once() {
    let src = "\
thing a:
    v = \"a\"

thing b uses a:
    v = \"b\"

thing d uses a:
    v = \"d\"

thing c uses b, d:
    v = \"c\"
";
    let (resolved, diags) = compose(LIN_SCHEMA, src, "thing", "c");
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(
        scalar(&resolved.unwrap().body, "v"),
        Some(&Value::String("c".into()))
    );
}

#[test]
fn cycle_is_2061() {
    let src = "\
thing a uses b:
    v = \"a\"

thing b uses a:
    v = \"b\"
";
    let (resolved, diags) = compose(LIN_SCHEMA, src, "thing", "a");
    assert!(resolved.is_none());
    assert!(codes_of(&diags).contains(&codes::LAYER_CYCLE));
}

#[test]
fn unresolved_ref_is_2059_with_hint() {
    let src = "\
thing base:
    v = \"b\"

thing t uses bsae:
    v = \"t\"
";
    let _ = src;
    let index = index_from(LIN_SCHEMA);
    // A transitive layer's unresolved ref reports NML2059 inside the
    // engine (the declaring clause's own unresolved refs are the
    // caller's to report before calling in):
    let src2 = "\
thing base:
    v = \"b\"

thing mid uses bsae:
    v = \"m\"

thing t uses mid:
    v = \"t\"
";
    let file2 = file_of(src2);
    let instances2 = InstanceIndex::from_file("main.nml", &file2);
    let declaring = instances2.resolve_ref("t").unwrap();
    let block = instances2.get(declaring).unwrap();
    let refs: Vec<InstanceId> = block
        .uses
        .iter()
        .filter_map(|r| instances2.resolve_ref(&r.name))
        .collect();
    let (resolved, diags) = resolve_layers(
        &index,
        &instances2,
        declaring,
        "thing",
        &refs,
        &block.body,
        &OpenContext,
    );
    assert!(resolved.is_none());
    assert!(codes_of(&diags).contains(&codes::UNRESOLVED_LAYER_REF));
    assert!(
        diags.iter().any(|d| d.message.contains("base")),
        "did-you-mean"
    );
}

#[test]
fn keyword_mismatch_is_2062() {
    let schema = "\
model thing:
    v string

model other:
    w string
";
    let src = "\
other base:
    w = \"b\"

thing t uses base:
    v = \"t\"
";
    let (resolved, diags) = compose(schema, src, "thing", "t");
    assert!(resolved.is_none());
    assert!(codes_of(&diags).contains(&codes::LAYER_KEYWORD_MISMATCH));
}

#[test]
fn deep_chain_fails_at_discovery_without_deep_recursion() {
    // A generated 40-link chain must fail at the 16-frame discovery
    // bound — NML2066 from the linearizer, never a stack overflow and
    // never 40 frames of recursion.
    let mut src = String::from("thing l0:\n    v = \"0\"\n");
    for i in 1..=40 {
        src.push_str(&format!("\nthing l{i} uses l{}:\n    v = \"{i}\"\n", i - 1));
    }
    let (resolved, diags) = compose(LIN_SCHEMA, &src, "thing", "l40");
    assert!(resolved.is_none());
    assert!(codes_of(&diags).contains(&codes::LAYER_BOUND_EXCEEDED));
}

#[test]
fn depth_cap_16_is_2066() {
    let mut src = String::from("thing l0:\n    v = \"0\"\n");
    for i in 1..=16 {
        src.push_str(&format!("\nthing l{i} uses l{}:\n    v = \"{i}\"\n", i - 1));
    }
    let (resolved, diags) = compose(LIN_SCHEMA, &src, "thing", "l16");
    assert!(resolved.is_none());
    assert!(codes_of(&diags).contains(&codes::LAYER_BOUND_EXCEEDED));
}

// ── grants ───────────────────────────────────────────────────────────

#[test]
fn duplicate_listed_ref_is_redundant_but_legal() {
    let src = "\
thing base:
    v = \"b\"

thing t uses base, base:
    v = \"t\"
";
    let (resolved, diags) = compose(LIN_SCHEMA, src, "thing", "t");
    assert!(diags.is_empty(), "redundant duplicate is silent: {diags:?}");
    assert_eq!(
        scalar(&resolved.unwrap().body, "v"),
        Some(&Value::String("t".into()))
    );
}

#[test]
fn cycle_message_renders_the_full_path() {
    let src = "\
thing a uses b:
    v = \"a\"

thing b uses a:
    v = \"b\"
";
    let (_, diags) = compose(LIN_SCHEMA, src, "thing", "a");
    assert!(
        diags.iter().any(|d| d.message.contains("->")),
        "cycle path rendered: {diags:?}"
    );
}

// ── compose_file: the shared orchestration contract ──────────────────

#[test]
fn wide_clause_is_bounded_before_the_c3_merge() {
    let schema = "model thing:\n    v string\n";
    let mut src = String::from("thing a:\n    v = \"a\"\n\nthing b:\n    v = \"b\"\n\n");
    for i in 0..20 {
        src.push_str(&format!("thing base{i} uses a, b:\n    v = \"x\"\n\n"));
    }
    src.push_str("thing top uses ");
    src.push_str(
        &(0..20)
            .map(|i| format!("base{i}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    src.push_str(":\n    v = \"t\"\n");
    let (resolved, diags) = compose(schema, &src, "thing", "top");
    assert!(resolved.is_none());
    assert!(
        codes_of(&diags).contains(&codes::LAYER_BOUND_EXCEEDED),
        "breadth rejects as NML2066 without running the merge: {diags:?}"
    );
}

#[test]
fn nml2077_names_a_transitive_base_listed_after_its_dependent() {
    let schema = "model thing:\n    v string\n";
    let src = "\
thing b:
    v = \"b\"

thing d uses b:
    v = \"d\"

thing c uses d, b:
    v = \"c\"
";
    let (_, diags) = compose(schema, src, "thing", "c");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::INCONSISTENT_LINEARIZATION))
        .expect("NML2077 fires");
    assert!(
        d.message
            .contains("'b' is already a transitive base of 'd'"),
        "names the pair and the cause: {}",
        d.message
    );
    assert!(
        !d.suggestions.is_empty(),
        "carries the machine-applicable remove-the-ref fix"
    );
}

#[test]
fn nml2077_names_an_opposed_shared_pair() {
    let schema = "model thing:\n    v string\n";
    let src = "\
thing slow:
    v = \"s\"

thing fast:
    v = \"f\"

thing vendorX uses slow, fast:
    v = \"x\"

thing productY uses fast, slow:
    v = \"y\"

thing tenant uses vendorX, productY:
    v = \"t\"
";
    let (_, diags) = compose(schema, src, "thing", "tenant");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::INCONSISTENT_LINEARIZATION))
        .expect("NML2077 fires");
    assert!(
        d.message.contains("order the shared pair"),
        "names the sibling contradiction: {}",
        d.message
    );
    assert!(
        d.message.contains("'fast'") && d.message.contains("'slow'"),
        "names the pair itself: {}",
        d.message
    );
}

#[test]
fn validate_side_uses_refs_share_check_wording() {
    let file = file_of("flow t uses missingLayer:\n    entrypoint = \"x\"\n");
    let diags = check_uses_refs("main.nml", &file);
    assert_eq!(codes_of(&diags), vec![codes::UNRESOLVED_LAYER_REF]);
    assert!(
        diags[0].message.contains("does not resolve"),
        "same wording owner as the composing path: {}",
        diags[0].message
    );
    // A schema definition's clause is definition-intrinsic: `validate`
    // owns it too, with the composing path's exact NML2062 wording.
    let schema_def = file_of("model m uses other:\n    x string\n");
    let diags = check_uses_refs("main.nml", &schema_def);
    assert_eq!(codes_of(&diags), vec![codes::LAYER_KEYWORD_MISMATCH]);
    assert!(
        diags[0].message.contains("delete the clause"),
        "same wording owner as compose_file: {}",
        diags[0].message
    );
}

// ── round-5 review pins ──────────────────────────────────────────────

#[test]
fn multi_root_cycle_reports_once() {
    let schema = "model thing:\n    v string\n";
    let src = "\
thing a uses b:
    v = \"a\"

thing b uses a:
    v = \"b\"
";
    let index = index_from(schema);
    let file = file_of(src);
    let out = compose_file(&index, "main.nml", &file, &OpenContext);
    let cycles = out
        .diagnostics
        .iter()
        .filter(|d| d.code == Some(codes::LAYER_CYCLE))
        .count();
    assert_eq!(
        cycles, 1,
        "one cycle, one finding — rotations canonicalize: {:?}",
        out.diagnostics
    );
}

#[test]
fn nml2077_sibling_offers_both_reorderings() {
    let schema = "model thing:\n    v string\n";
    let src = "\
thing slow:
    v = \"s\"

thing fast:
    v = \"f\"

thing vendorX uses slow, fast:
    v = \"x\"

thing productY uses fast, slow:
    v = \"y\"

thing tenant uses vendorX, productY:
    v = \"t\"
";
    let (_, diags) = compose(schema, src, "thing", "tenant");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::INCONSISTENT_LINEARIZATION))
        .expect("NML2077 fires");
    assert!(
        d.message.contains("align them: order"),
        "offers the two reorderings: {}",
        d.message
    );
}

#[test]
fn nml2077_deletion_targets_the_refs_own_span() {
    let schema = "model thing:\n    v string\n";
    let src = "\
thing b:
    v = \"b\"

thing d uses b:
    v = \"d\"

thing c uses d, b:
    v = \"c\"
";
    let (_, diags) = compose(schema, src, "thing", "c");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::INCONSISTENT_LINEARIZATION))
        .expect("NML2077 fires");
    let sugg = d.suggestions.first().expect("carries the machine fix");
    assert_eq!(
        sugg.kind,
        crate::diagnostic::SuggestionKind::Delete,
        "structural, not verbatim"
    );
    assert_eq!(
        &src[sugg.span.start..sugg.span.end],
        "b",
        "the ref's own span — the resolver owns the separator bytes"
    );
}

#[test]
fn nml2062_offers_a_same_keyword_did_you_mean() {
    let schema = "model thing:\n    v string\n\nmodel other:\n    v string\n";
    let src = "\
thing basePlan:
    v = \"b\"

other basePlot:
    v = \"o\"

thing t uses basePlot:
    v = \"t\"
";
    let (_, diags) = compose(schema, src, "thing", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::LAYER_KEYWORD_MISMATCH))
        .expect("cross-keyword ref is NML2062");
    assert!(
        d.message.contains("did you mean 'basePlan'"),
        "near-named same-keyword hint: {}",
        d.message
    );
}

#[test]
fn nml2077_duplicated_ref_carries_no_suggestion() {
    // One span cannot remove every occurrence of a duplicated name,
    // and a machine fix that doesn't fix stalls `nml fix`.
    let schema = "model thing:\n    v string\n";
    let src = "\
thing b:
    v = \"b\"

thing d uses b:
    v = \"d\"

thing c uses d, b, b:
    v = \"c\"
";
    let (_, diags) = compose(schema, src, "thing", "c");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::INCONSISTENT_LINEARIZATION))
        .expect("NML2077 fires");
    assert!(
        d.suggestions.is_empty(),
        "hint-only when the name is duplicated: {d:?}"
    );
}

#[test]
fn unindexed_refs_never_fail_silently() {
    // `resolve_layers` is the documented embedder entry point: every
    // failure carries a diagnostic — a bare (None, []) is a vanishing
    // instance with no explanation.
    let schema = "model thing:\n    v string\n";
    let index = index_from(schema);
    let file = file_of("thing t:\n    v = \"t\"\n");
    let instances = InstanceIndex::from_file("main.nml", &file);
    let declaring = instances.resolve_ref("t").unwrap();
    let ghost = InstanceId {
        source_path: "main.nml",
        name: "doesNotExist",
    };
    let (resolved, diags) = resolve_layers(
        &index,
        &instances,
        declaring,
        "thing",
        &[ghost],
        &instances.get(declaring).unwrap().body,
        &OpenContext,
    );
    assert!(resolved.is_none());
    assert!(
        codes_of(&diags).contains(&codes::UNRESOLVED_LAYER_REF),
        "the failure explains itself: {diags:?}"
    );
}

#[test]
fn nml2077_names_a_rotation_across_three_clauses() {
    // Neither a transitive-base pair nor an opposed shared pair — the
    // orders rotate. The fallback used to assert exactly the two
    // causes that were just ruled out; now it names the cycle.
    let schema = "model thing:\n    v string\n";
    let src = "\
thing p:
    v = \"p\"

thing q:
    v = \"q\"

thing r:
    v = \"r\"

thing a uses q, p:
    v = \"a\"

thing b uses r, q:
    v = \"b\"

thing c uses p, r:
    v = \"c\"

thing top uses a, b, c:
    v = \"t\"
";
    let (_, diags) = compose(schema, src, "thing", "top");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::INCONSISTENT_LINEARIZATION))
        .expect("NML2077 fires");
    assert!(
        d.message.contains("orders rotate"),
        "the rotation shape is named, not the ruled-out patterns: {}",
        d.message
    );
    assert!(
        d.message.contains("above"),
        "renders the cycle's pairwise steps: {}",
        d.message
    );
}
