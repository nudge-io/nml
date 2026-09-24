use super::super::*;
use super::*;

/// K — FLIPPED by RFC 0025 Phase 3: the fold precedes
/// materialization, so the token materializes under the COMPOSED
/// arm (stepB, which has no `+` field — nothing injects). Before,
/// the positionalizer materialized under the item's own DEFAULT arm
/// and the composed item silently carried the foreign
/// `name = \"h2\"` (the pinned defect; NML2043 short-circuits the
/// validation that would have named it).
#[test]
fn item_token_materializes_under_the_composed_arm() {
    let src = "\
appK base:
    steps:
        - \"h2\":
            kind = \"b\"
            id = \"x\"

appK top uses base:
    steps:
        - \"h2\"
";
    let (resolved, diags) = compose(K_SCHEMA, src, "appK", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let resolved = resolved.unwrap();
    let items = composed_items(&resolved.body, "steps");
    assert_eq!(items.len(), 1);
    let ListItemKind::Shorthand {
        body: Some(body), ..
    } = &items[0].kind
    else {
        panic!("shorthand with body: {:?}", items[0]);
    };
    assert!(item_prop(body, "kind").unwrap().contains("\"b\""));
    assert!(item_prop(body, "id").unwrap().contains("\"x\""));
    assert!(
        item_prop(body, "name").is_none(),
        "no foreign token field under the composed arm: {body:?}"
    );
}

/// K3 — FLIPPED by RFC 0025 Phase 3: `xs = []` on the top item is
/// judged under the COMPOSED arm (stepB, where `xs` is not a list —
/// no verdict; the validator owns the unknown field). Before, the
/// item's own default arm (stepA) verdicted it NML2079 while the
/// group composed under stepB.
#[test]
fn item_zero_item_verdict_under_the_composed_arm() {
    let src = "\
appK base:
    steps:
        - \"h2\":
            kind = \"b\"
            id = \"x\"

appK top uses base:
    steps:
        - \"h2\":
            xs = []
";
    let (resolved, diags) = compose(K_SCHEMA, src, "appK", "top");
    assert!(
        !diags
            .iter()
            .any(|d| d.code == Some(codes::ZERO_ITEM_LAYER_ENTRY)),
        "no zero-item verdict under a foreign arm's reading: {diags:?}"
    );
    let resolved = resolved.unwrap();
    let items = composed_items(&resolved.body, "steps");
    let ListItemKind::Shorthand {
        body: Some(body), ..
    } = &items[0].kind
    else {
        panic!("shorthand with body");
    };
    assert!(item_prop(body, "kind").unwrap().contains("\"b\""));
    assert!(
        item_prop(body, "name").is_none(),
        "no foreign token field: {body:?}"
    );
}

/// B2 — FLIPPED by RFC 0025 Phase 3: the fold precedes
/// materialization, so a `+` token never doubles as a STATED
/// discriminator, even on NML2043-invalid input (shorthand items on
/// a oneof-element list — the validator's verdict either way).
/// Before, the bare top `- \"b\"` injected `kind = \"b\"` ahead of
/// the fold, switched the group's arm and drew NML2060 for the
/// sealed field.
#[test]
fn an_invalid_token_discriminator_never_switches() {
    const S: &str = "\
model armA:
    secret string #sealed

model armB:
    kind string+
    other string

oneof sw by kind = \"b\":
    \"a\" -> armA
    \"b\" -> armB

model appB:
    xs []sw #identity
";
    let src = "\
appB base:
    xs:
        - \"b\":
            kind = \"a\"
            secret = \"s\"

appB top uses base:
    xs:
        - \"b\"
";
    let (resolved, diags) = compose(S, src, "appB", "top");
    assert!(
        !diags
            .iter()
            .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)),
        "the token never switches — no backstop, nothing displaced: {diags:?}"
    );
    let resolved = resolved.unwrap();
    let items = composed_items(&resolved.body, "xs");
    let ListItemKind::Shorthand {
        body: Some(body), ..
    } = &items[0].kind
    else {
        panic!("shorthand with body");
    };
    assert!(
        item_prop(body, "kind").unwrap().contains("\"a\""),
        "the base's stated arm holds: {body:?}"
    );
    assert!(item_prop(body, "secret").unwrap().contains("\"s\""));
}

/// P2 — survives Phase 3 unchanged: a list-wide `.shared` naming a
/// scalar item's `+` token field neither reaches the composed item
/// nor draws NML2060 (RFC 0005 §10 — an item's own token beats a
/// shared property).
#[test]
fn an_items_token_beats_a_list_wide_shared() {
    const S: &str = "\
model it:
    name string+
    v string

model appP:
    xs []it #identity
";
    let src = "\
appP base:
    xs:
        - \"a\":
            v = \"1\"

appP top uses base:
    xs:
        .name = \"other\"
        - \"a\":
            v = \"2\"
";
    let (resolved, diags) = compose(S, src, "appP", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let resolved = resolved.unwrap();
    let items = composed_items(&resolved.body, "xs");
    let ListItemKind::Shorthand {
        body: Some(body),
        value,
    } = &items[0].kind
    else {
        panic!("shorthand with body");
    };
    assert!(format!("{:?}", value.value).contains("\"a\""));
    assert!(
        item_prop(body, "name").unwrap().contains("\"a\""),
        "the token wins over the shared write: {body:?}"
    );
    assert!(item_prop(body, "v").unwrap().contains("\"2\""));

    // P3 — a shared value naming a NON-token field reaches the
    // composed item (only the identity token is beyond `.shared`).
    const S3: &str = "\
model it3:
    name string+
    tag string

model appP3:
    xs []it3 #identity
";
    let src = "\
appP3 base:
    xs:
        - \"a\":
            tag = \"low\"

appP3 top uses base:
    xs:
        .tag = \"t\"
        - \"a\"
";
    let (resolved, diags) = compose(S3, src, "appP3", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let resolved = resolved.unwrap();
    let items = composed_items(&resolved.body, "xs");
    let ListItemKind::Shorthand {
        body: Some(body), ..
    } = &items[0].kind
    else {
        panic!("shorthand with body");
    };
    assert!(
        item_prop(body, "tag").unwrap().contains("\"t\""),
        "the shared write reaches the non-token field: {body:?}"
    );
    assert!(item_prop(body, "name").unwrap().contains("\"a\""));
}

/// P6 — FLIPPED by RFC 0025 Phase 3: the token-restatement strip is
/// deleted (the token materializes once, into the lowest surviving
/// body — no per-layer copies to strip), so an AUTHORED equal-value
/// restatement of a sealed `+` token field is what it says: a
/// second write to a sealed field, NML2060 equal-value with the
/// deletion fix.
#[test]
fn an_authored_restatement_of_a_sealed_token_is_nml2060() {
    const S: &str = "\
model it6:
    name string+ #sealed
    v string

model appP6:
    xs []it6 #identity
";
    let src = "\
appP6 base:
    xs:
        - \"a\":
            v = \"1\"

appP6 top uses base:
    xs:
        - \"a\":
            name = \"a\"
";
    let (resolved, diags) = compose(S, src, "appP6", "top");
    assert_eq!(
        codes_of(&diags),
        [codes::SEALED_FIELD_VIOLATION],
        "{diags:?}"
    );
    assert!(
        diags[0]
            .message
            .contains("already sealed to this same value")
            && diags[0]
                .suggestions
                .iter()
                .any(|sg| sg.replacement.is_empty()),
        "the equal-value form, with its deletion fix: {:?}",
        diags[0]
    );
    let resolved = resolved.unwrap();
    let items = composed_items(&resolved.body, "xs");
    let ListItemKind::Shorthand {
        body: Some(body), ..
    } = &items[0].kind
    else {
        panic!("shorthand with body");
    };
    assert!(item_prop(body, "name").unwrap().contains("\"a\""));
}

/// Q2 — survives Phase 3 unchanged: a Named key's `name` is NOT a
/// token (composition never materializes it), so a list-wide
/// `.name` reaches a Named item.
#[test]
fn a_list_wide_shared_reaches_a_named_items_name() {
    const S: &str = "\
model itq:
    name string
    v string

model appQ:
    xs []itq #identity
";
    let src = "\
appQ base:
    xs:
        - w:
            v = \"1\"

appQ top uses base:
    xs:
        .name = \"n2\"
        - w:
            v = \"2\"
";
    let (resolved, diags) = compose(S, src, "appQ", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let resolved = resolved.unwrap();
    let items = composed_items(&resolved.body, "xs");
    let ListItemKind::Named { body, .. } = &items[0].kind else {
        panic!("named item");
    };
    assert!(
        item_prop(body, "name").unwrap().contains("\"n2\""),
        "the shared write reaches a Named item's name: {body:?}"
    );
}

/// C2 — survives Phase 3 unchanged: item bodies are
/// shared-distributed BEFORE the fold, so a top layer's list-wide
/// `.kind = \"b\"` legitimately switches an identity item's arm.
#[test]
fn a_shared_discriminator_switches_an_items_arm() {
    const S: &str = "\
model stepCA:
    x string

model stepCB:
    y string

oneof stepc by kind = \"a\":
    \"a\" -> stepCA
    \"b\" -> stepCB

model appC:
    steps []stepc #identity
";
    let src = "\
appC base:
    steps:
        - w:
            kind = \"a\"
            x = \"1\"

appC top uses base:
    steps:
        .kind = \"b\"
        - w:
            y = \"2\"
";
    let (resolved, _diags) = compose(S, src, "appC", "top");
    let resolved = resolved.unwrap();
    let items = composed_items(&resolved.body, "steps");
    let ListItemKind::Named { body, .. } = &items[0].kind else {
        panic!("named item");
    };
    assert!(
        item_prop(body, "kind").unwrap().contains("\"b\""),
        "the shared discriminator switches the group: {body:?}"
    );
    assert!(
        item_prop(body, "y").unwrap().contains("\"2\""),
        "the new arm's field composes: {body:?}"
    );
}

/// U1 — survives Phase 3 (the subtraction must own it): a LOSING
/// bare-list's interior NML2079 is emitted today by per-layer
/// normalization; the overlay winner replaces the list wholesale.
#[test]
fn a_losing_lists_items_are_still_diagnosed() {
    const S: &str = "\
model itu:
    v string
    ys []string

model appU:
    xs []itu
";
    let src = "\
appU base:
    xs:
        - w:
            ys = []

appU top uses base:
    xs:
        - z:
            v = \"2\"
";
    let (resolved, diags) = compose(S, src, "appU", "top");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::ZERO_ITEM_LAYER_ENTRY) && d.message.contains("ys")),
        "the losing list's interior verdict is emitted: {diags:?}"
    );
    let resolved = resolved.unwrap();
    let items = composed_items(&resolved.body, "xs");
    assert_eq!(items.len(), 1, "the overlay winner replaced the list");
    let ListItemKind::Named { name, .. } = &items[0].kind else {
        panic!("named item");
    };
    assert_eq!(name.name, "z");
}

/// Loser-table row 1 — FLIPPED by RFC 0025 Phase 3 (§4): a REJECTED
/// switch's body diagnoses under its OWN stated arm — `extras = []`
/// is a list there, and the verdict appears beside the rejection.
/// Before, the loser was read under the SURVIVOR's vocabulary,
/// where `extras` is not a list, and the verdict vanished.
#[test]
fn a_rejected_switch_loser_diagnoses_under_its_own_arm() {
    const S: &str = "\
model ra:
    kind string
    s string #sealed

model rb:
    kind string
    extras []string

oneof ro by kind = \"a\":
    \"a\" -> ra
    \"b\" -> rb

model appR:
    slot ro
";
    let src = "\
appR base:
    slot:
        kind = \"a\"
        s = \"x\"

appR top uses base:
    slot:
        kind = \"b\"
        extras = []
";
    let (_, diags) = compose(S, src, "appR", "top");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)),
        "the switch is rejected: {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::ZERO_ITEM_LAYER_ENTRY) && d.message.contains("extras")),
        "the loser diagnoses under its own arm, where `extras` IS a \
             list: {diags:?}"
    );
}

/// R1 — the `compose_root` home's pin (RFC 0025 §4): an oneof-ROOT
/// rejected switch's whole LAYER diagnoses under its own stated
/// arm — its `extras = []` (a list only there) is NML2079 beside
/// the rejection.
#[test]
fn a_rejected_root_switch_loser_diagnoses_under_its_own_arm() {
    const S: &str = "\
model rra:
    kind string
    s string #sealed

model rrb:
    kind string
    extras []string

oneof appRoot by kind = \"a\":
    \"a\" -> rra
    \"b\" -> rrb
";
    let src = "\
appRoot base:
    kind = \"a\"
    s = \"x\"

appRoot top uses base:
    kind = \"b\"
    extras = []
";
    let (resolved, diags) = compose(S, src, "appRoot", "top");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)),
        "the root switch is rejected: {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::ZERO_ITEM_LAYER_ENTRY) && d.message.contains("extras")),
        "the losing layer diagnoses under its own arm: {diags:?}"
    );
    assert_eq!(
        scalar(&resolved.unwrap().body, "kind"),
        Some(&Value::String("a".into())),
        "the base's arm survives"
    );
}

/// Loser-table row 2 — FLIPPED by RFC 0025 Phase 3 (§4): a
/// switch-DISPLACED body diagnoses under its own arm; its
/// `opts = []` (a list only there) is NML2079 even though the
/// accepted switch discarded the body.
#[test]
fn a_switch_displaced_loser_diagnoses_under_its_own_arm() {
    const S: &str = "\
model sa:
    kind string
    opts []string

model sb:
    kind string
    y string

oneof so by kind = \"a\":
    \"a\" -> sa
    \"b\" -> sb

model appS:
    slot so
";
    let src = "\
appS base:
    slot:
        kind = \"a\"
        opts = []

appS top uses base:
    slot:
        kind = \"b\"
        y = \"1\"
";
    let (resolved, diags) = compose(S, src, "appS", "top");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::ZERO_ITEM_LAYER_ENTRY) && d.message.contains("opts")),
        "the displaced base diagnoses under its own arm: {diags:?}"
    );
    assert_eq!(
        nested_scalar(&resolved.unwrap().body, "slot", "kind"),
        Some(&Value::String("b".into())),
        "the accepted switch holds"
    );
}

/// M — FLIPPED by RFC 0025 Phase 3 (§4): a loser's NESTED positions
/// use its own readings all the way down — the rejected top's
/// `sub.zs = []` verdicts under the top's own arm's nested model.
#[test]
fn a_losers_nested_positions_use_its_own_readings() {
    const S: &str = "\
model mSub:
    zs []string

model ma:
    kind string
    s string #sealed

model mb:
    kind string
    sub mSub

oneof mo by kind = \"a\":
    \"a\" -> ma
    \"b\" -> mb

model appM:
    slot mo
";
    let src = "\
appM base:
    slot:
        kind = \"a\"
        s = \"x\"

appM top uses base:
    slot:
        kind = \"b\"
        sub:
            zs = []
";
    let (_, diags) = compose(S, src, "appM", "top");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)),
        "the switch is rejected: {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::ZERO_ITEM_LAYER_ENTRY) && d.message.contains("zs")),
        "the loser's nested interior reads its own arm: {diags:?}"
    );
}

/// The DISCARDS seam (RFC 0025 §6): every (path, layer) the loser
/// subtraction diagnosed — U1's losing bare list is the observable.
#[test]
fn the_subtraction_records_every_discarded_body() {
    const S: &str = "\
model itu2:
    v string
    ys []string

model appU2:
    xs []itu2
";
    let src = "\
appU2 base:
    xs:
        - w:
            ys = []

appU2 top uses base:
    xs:
        - z:
            v = \"2\"
";
    DISCARDS.with(|d| d.borrow_mut().clear());
    let (_, diags) = compose(S, src, "appU2", "top");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::ZERO_ITEM_LAYER_ENTRY) && d.message.contains("ys")),
        "the losing list's interior verdict is emitted: {diags:?}"
    );
    let recorded = DISCARDS.with(|d| d.borrow().clone());
    assert!(
        recorded
            .iter()
            .any(|(path, layer)| path == "xs" && layer == "base"),
        "the subtraction recorded the losing list: {recorded:?}"
    );
}

/// The FOLD_TAMPER liveness test (RFC 0025 §6): corrupt a oneof
/// trace entry to `Pinned` — a union-only verdict there — and
/// exactly one NML2086 is emitted; the compose boundary's
/// debug_assert reads the same sink and fires in debug builds, so
/// the assertion is provably live.
#[test]
#[cfg_attr(
    debug_assertions,
    should_panic(expected = "internal composition invariant violated")
)]
fn a_tampered_fold_trace_is_loud_at_the_boundary() {
    const S: &str = "\
model ta:
    x string

model tb:
    y string

oneof appT by kind = \"a\":
    \"a\" -> ta
    \"b\" -> tb
";
    let src = "\
appT base:
    kind = \"a\"
    x = \"1\"

appT top uses base:
    x = \"2\"
";
    fn tamper(path: &str, trace: &mut DecisionTrace<'_>) {
        // Path-aware: the fixture's first folded position is the
        // oneof root.
        if path.is_empty() {
            if let Some(d) = trace.get_mut(1) {
                d.1 = ArmDecision::Pinned;
            }
        }
    }
    FOLD_TAMPER.with(|t| t.set(Some(tamper)));
    let (resolved, diags) = compose(S, src, "appT", "top");
    // Reached only without debug assertions (the boundary panics in
    // debug builds): the tampered layer is diagnosed not composed —
    // exactly once — and it is not composed.
    let invariants: Vec<&Diagnostic> = diags
        .iter()
        .filter(|d| d.code == Some(codes::INTERNAL_COMPOSE_INVARIANT))
        .collect();
    assert_eq!(invariants.len(), 1, "{diags:?}");
    assert!(
        invariants[0]
            .message
            .contains("a union-only verdict at a oneof position"),
        "{:?}",
        invariants[0]
    );
    assert_eq!(
        scalar(&resolved.unwrap().body, "x"),
        Some(&Value::String("1".into())),
        "the tampered layer's contribution is not composed"
    );
}

/// The order contract itself (RFC 0025 §5): cross-layer findings
/// keep STACK order even when spans invert; same-layer findings
/// sort by span; a full-key tie keeps emission order (stable sort).
#[test]
fn the_compose_sink_orders_by_stack_then_span() {
    let a = InstanceId {
        source_path: "main.nml",
        name: "base",
    };
    let b = InstanceId {
        source_path: "main.nml",
        name: "top",
    };
    let mut sink = ComposeSink::new(vec![a, b]);
    let d = |msg: &str, at: usize| {
        Diagnostic::warning(msg.to_string())
            .with_span(Span::new(at, at + 1))
            .with_source("main.nml".to_string())
    };
    sink.emit(b, d("top-early-span", 0));
    sink.emit(a, d("base-late-span", 100));
    sink.emit(a, d("base-early-span", 5));
    sink.emit(a, d("tie", 5));
    let out: Vec<String> = sink.finish().into_iter().map(|x| x.message).collect();
    assert_eq!(
        out,
        ["base-early-span", "tie", "base-late-span", "top-early-span"],
        "stack position is the primary key; span orders within a \
             layer; ties keep emission order"
    );
}

// ───────────────────────────────────────────────────────────────
// RFC 0025 Phase 0 — the timing gate. Coarse in-process bounds
// over the five generated stacks (tests/fixtures/layers/perf/):
// catastrophic-regression tripwires for the named complexity traps
// (per-level deep normalization, per-layer re-normalized items,
// the O(N²) identity scan). The PRECISE gate is the same-session
// two-binary ratio the `just perf-layers` recipe runs (RFC 0025
// §8); absolutes drift across machines, so these bounds are
// deliberately loose. `--ignored` only:
// `cargo test -p nml-core --release -- --ignored perf_`.
// ───────────────────────────────────────────────────────────────
