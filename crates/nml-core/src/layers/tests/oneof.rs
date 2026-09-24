use super::super::*;
use super::*;

#[test]
fn non_string_discriminator_survives_to_validation() {
    let src = "\
notify base:
    kind = \"az\"
    azureUrl = \"https://a\"
    azureKey = \"k\"

notify top uses base:
    kind = sns
";
    let (resolved, diags) = compose(ONEOF_SCHEMA, src, "notify", "top");
    // Not recognized as a switch (no backstop), and NOT silently
    // stripped: the bad assignment must reach the resolved body so
    // validation can flag it at its authored span.
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    let kinds: Vec<&Value> = body
        .entries
        .iter()
        .filter_map(|e| match &e.kind {
            BodyEntryKind::Property(p) if p.name.name == "kind" => Some(&p.value.value),
            _ => None,
        })
        .collect();
    assert!(
        kinds
            .iter()
            .any(|v| matches!(v, Value::Reference(r) if r == "sns")),
        "authored non-string discriminator preserved: {kinds:?}"
    );
}

#[test]
fn non_string_discriminators_pass_through_at_the_front() {
    // Part C (RFC 0019 E16): stripping is by NAME, so non-string
    // discriminator entries never compose over each other — the
    // surviving group's non-string entries pass through in layer
    // order, FIRST when no survivor states a string discriminator,
    // and carry no provenance row (validator-facing, never
    // effective).
    let src = "\
notify base:
    kind = 5
    path = \"p\"

notify top uses base:
    kind = 6
";
    let (resolved, diags) = compose(ONEOF_SCHEMA, src, "notify", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let resolved = resolved.unwrap();
    let kind_of = |e: &BodyEntry| match &e.kind {
        BodyEntryKind::Property(p) if p.name.name == "kind" => Some(p.value.value.clone()),
        _ => None,
    };
    let front: Vec<Option<Value>> = resolved.body.entries.iter().take(2).map(kind_of).collect();
    assert_eq!(
        front,
        vec![Some(Value::number(5)), Some(Value::number(6))],
        "passthroughs first, in layer order: {:?}",
        resolved.body.entries
    );
    assert!(
        !resolved.origins.iter().any(|(p, _)| p == "kind"),
        "no provenance row for a passthrough: {:?}",
        resolved.origins
    );
}

#[test]
fn a_restated_string_discriminator_composes_to_one_canonical_entry() {
    // First-wins for STRING entries — a later restatement is neither
    // canonical nor passed through — while a non-string sibling
    // passes through beside the canonical entry.
    let src = "\
notify base:
    kind = \"az\"
    azureUrl = \"https://a\"
    azureKey = \"k\"

notify top uses base:
    kind = \"az\"
    kind = 7
";
    let (resolved, diags) = compose(ONEOF_SCHEMA, src, "notify", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    let kinds: Vec<&Value> = body
        .entries
        .iter()
        .filter_map(|e| match &e.kind {
            BodyEntryKind::Property(p) if p.name.name == "kind" => Some(&p.value.value),
            _ => None,
        })
        .collect();
    let strings = kinds
        .iter()
        .filter(|v| matches!(v, Value::String(_)))
        .count();
    assert_eq!(strings, 1, "one canonical string entry: {kinds:?}");
    assert_eq!(kinds.len(), 2, "plus the non-string passthrough: {kinds:?}");
}

// ── oneof accumulator + backstop ─────────────────────────────────────

#[test]
fn oneof_switch_discarding_seal_is_backstopped() {
    let src = "\
notify base:
    kind = \"az\"
    azureUrl = \"https://a\"
    azureKey = \"k\"

notify mid uses base:
    azureUrl = \"https://b\"

notify top uses mid:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
    let (resolved, diags) = compose(ONEOF_SCHEMA, src, "notify", "top");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    assert!(diags[0].message.contains("azureKey"));
    let body = resolved.unwrap().body;
    assert_eq!(scalar(&body, "kind"), Some(&Value::String("az".into())));
    assert_eq!(
        scalar(&body, "azureUrl"),
        Some(&Value::String("https://b".into())),
        "middle layer's deep-merge landed"
    );
    assert_eq!(scalar(&body, "azureKey"), Some(&Value::String("k".into())));
}

#[test]
fn oneof_default_arm_switch_drops_base_fields_cleanly() {
    let src = "\
notify base:
    path = \"/var/log/x\"

notify top uses base:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
    let (resolved, diags) = compose(ONEOF_SCHEMA, src, "notify", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    assert_eq!(scalar(&body, "kind"), Some(&Value::String("sns".into())));
    assert!(scalar(&body, "path").is_none(), "base arm fields dropped");
    assert_eq!(
        scalar(&body, "topicArn"),
        Some(&Value::String("arn:x".into()))
    );
}

// ── schema-load policy validation + lints ────────────────────────────

#[test]
fn oneof_root_array_spelled_lists_merge() {
    let src = "\
relay base:
    kind = \"az\"
    hosts = [\"a\"]

relay t uses base:
    hosts = [\"b\"]
";
    let (resolved, diags) = compose(ONEOF_ROOT_LIST_SCHEMA, src, "relay", "t");
    assert!(diags.is_empty(), "clean merge: {diags:?}");
    let hosts = list_names(&resolved.unwrap().body, "hosts");
    assert_eq!(
        hosts.len(),
        2,
        "oneof-root lists normalize and merge: {hosts:?}"
    );
}

#[test]
fn rejected_switch_normalizes_against_the_surviving_arm() {
    // The fold mirrors the backstop: a rejected switch must not
    // have already materialized lower layers against the rejected
    // arm's element models (injecting its positional fields into the
    // composed body at the author's spans).
    let schema = "\
model stepA:
    cmd string+
    note string

model stepB:
    url string+
    note string

model armA:
    kind string
    token string #sealed
    steps []stepA #identity

model armB:
    kind string
    steps []stepB #identity

oneof job by kind = \"a\":
    \"a\" -> armA
    \"b\" -> armB
";
    let src = "\
job base:
    token = \"sekrit\"
    steps:
        - \"build\":
            note = \"n1\"

job t uses base:
    kind = \"b\"
    steps:
        - \"deploy\"
";
    let (resolved, diags) = compose(schema, src, "job", "t");
    assert!(
        codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "the switch is rejected: {diags:?}"
    );
    let body = resolved.unwrap().body;
    let item_fields: Vec<String> = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "steps" => Some(&nb.body),
            _ => None,
        })
        .expect("steps present")
        .entries
        .iter()
        .filter_map(|e| match &e.kind {
            BodyEntryKind::ListItem(ListItem {
                kind: ListItemKind::Shorthand { body: Some(b), .. },
                ..
            }) => Some(b),
            _ => None,
        })
        .flat_map(|b| {
            b.entries.iter().filter_map(|e| match &e.kind {
                BodyEntryKind::Property(p) => Some(p.name.name.clone()),
                _ => None,
            })
        })
        .collect();
    assert!(
        item_fields.contains(&"cmd".to_string()),
        "materialized against the SURVIVING arm: {item_fields:?}"
    );
    assert!(
        !item_fields.contains(&"url".to_string()),
        "no rejected-arm injection: {item_fields:?}"
    );
}

#[test]
fn arm_set_replacement_is_wholesale_and_backstopped() {
    // v1: a layer that states the field replaces the WHOLE set.
    let src = "\
router base:
    route:
        \"a\" -> One:
            note = \"n1\"

router t uses base:
    route:
        \"b\" -> Two:
            note = \"n2\"
";
    let (resolved, diags) = compose(ARM_SET_SCHEMA, src, "router", "t");
    assert!(diags.is_empty(), "clean replacement: {diags:?}");
    let body = resolved.unwrap().body;
    let arm_count = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "route" => Some(
                nb.body
                    .entries
                    .iter()
                    .filter(|e| matches!(e.kind, BodyEntryKind::Arm(_)))
                    .count(),
            ),
            _ => None,
        })
        .unwrap_or(0);
    assert_eq!(arm_count, 1, "whole-set replacement, no accumulation");

    // ...subject to the seal backstop: a displaced set whose inline
    // bodies carry an assigned sealed field refuses the replacement.
    let src2 = "\
router base:
    route:
        \"a\" -> One:
            token = \"locked\"

router t uses base:
    route:
        \"b\" -> Two:
            note = \"n2\"
";
    let (resolved, diags) = compose(ARM_SET_SCHEMA, src2, "router", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("arm-set replacement is backstopped");
    assert!(
        d.message.contains("arm-set replacement") && d.message.contains("token"),
        "names the replacement and the seal: {}",
        d.message
    );
    let body = resolved.unwrap().body;
    let kept: Vec<String> = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "route" => Some(
                nb.body
                    .entries
                    .iter()
                    .filter_map(|e| match &e.kind {
                        BodyEntryKind::Arm(a) => match &a.target {
                            ArmTarget::Inline { name, .. } => Some(name.name.clone()),
                            _ => None,
                        },
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default();
    assert_eq!(kept, vec!["One"], "the sealed set survives: {kept:?}");
}

// ── union compose: round-12 battery (backstop depth, structural
//    variants, oneof-target arm sets, list-of-union items) ──────────

#[test]
fn oneof_switch_is_backstopped_through_a_union_field() {
    // The dual of the nested-union pin: a oneof ARM switch must see
    // seals hiding inside the displaced arm's union-typed field.
    let src = "\
pay base:
    kind = \"x\"
    inner as leafA:
        s = \"locked\"

pay top uses base:
    kind = \"y\"
    w = \"1\"
";
    let (resolved, diags) = compose(ONEOF_WITH_UNION_SCHEMA, src, "pay", "top");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("arm switch is backstopped through the union interior");
    assert!(
        d.message.contains("arm switch to `kind = \"y\"`") && d.message.contains("inner.s"),
        "names the switch and the buried seal: {}",
        d.message
    );
    let body = resolved.unwrap().body;
    assert_eq!(scalar(&body, "kind"), Some(&Value::String("x".into())));
}

#[test]
fn arm_set_replacement_with_a_oneof_target_is_backstopped() {
    // RFC 0019 binds the backstop to all three variant forms
    // \"equally\": a oneof-TARGETED arm set judges each displaced
    // inline body under the arm its own discriminator selects
    // (resolving only `index.model` here was a laundering hole —
    // and NML2076 explicitly promises this backstop).
    let src = "\
router2 base:
    route:
        \"a\" -> H:
            kind = \"card\"
            pan = \"4111\"

router2 t uses base:
    route:
        \"b\" -> H2:
            kind = \"cash\"
            amount = \"5\"
";
    let (resolved, diags) = compose(ONEOF_ARM_SET_SCHEMA, src, "router2", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("oneof-target arm-set replacement is backstopped");
    assert!(
        d.message.contains("arm-set replacement") && d.message.contains("route.pan"),
        "{}",
        d.message
    );
    let body = resolved.unwrap().body;
    let kept: Vec<String> = sub_block(&body, "route")
        .map(|b| {
            b.entries
                .iter()
                .filter_map(|e| match &e.kind {
                    BodyEntryKind::Arm(a) => match &a.target {
                        ArmTarget::Inline { name, .. } => Some(name.name.clone()),
                        _ => None,
                    },
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(kept, vec!["H"], "the sealed set survives");
}

#[test]
fn discriminator_strip_parity_holds_at_every_plan_site() {
    // The plan hides a oneof's stated string discriminator from its
    // supply gather exactly where the merge strips it: at a oneof
    // ROOT, under a oneof-typed field, and under a oneof VARIANT of a
    // union — with a union field named like the discriminator at
    // each, a non-string `kind`, a twice-stated one, and a `.shared`
    // one. None misaligns (no NML2086 — a debug crash at the
    // boundary).
    const S: &str = "\
model va:
    p string

model vb:
    q string

model arma:
    kind (va | vb)

model armb:
    z string

oneof oo3 by kind = \"a\":
    \"a\" -> arma
    \"b\" -> armb

model ua:
    x string

model holder43:
    cfg oo3
    slot (oo3 | ua)
";
    for src in [
        "oo3 base:\n    kind = \"a\"\n\noo3 top uses base:\n    kind as va:\n        p = \"1\"\n",
        "holder43 base:\n    cfg:\n        kind = \"a\"\n\n\
             holder43 top uses base:\n    cfg:\n        kind as va:\n            p = \"1\"\n",
        "holder43 base:\n    slot as oo3:\n        kind = \"a\"\n\n\
             holder43 top uses base:\n    slot as oo3:\n        kind as va:\n            p = \"1\"\n",
        "holder43 base:\n    cfg:\n        kind = \"a\"\n\n\
             holder43 top uses base:\n    cfg:\n        kind = 5\n",
        "holder43 base:\n    cfg:\n        kind = \"a\"\n        kind = \"b\"\n\n\
             holder43 top uses base:\n    cfg:\n        kind as va:\n            p = \"1\"\n",
        "holder43 base:\n    cfg:\n        .kind = \"a\"\n\n\
             holder43 top uses base:\n    cfg:\n        kind as va:\n            p = \"1\"\n",
    ] {
        let root = if src.starts_with("oo3") {
            "oo3"
        } else {
            "holder43"
        };
        let (resolved, diags) = compose(S, src, root, "top");
        assert!(
            !codes_of(&diags).contains(&codes::INTERNAL_COMPOSE_INVARIANT),
            "{src}: {diags:?}"
        );
        assert!(resolved.is_some(), "{src}");
    }
}

#[test]
fn a_model_typed_discriminator_named_field_keeps_block_and_string_apart() {
    // The plan hides only the STRING discriminator entries; a `kind:`
    // block under a model-typed `kind` field is a value on both
    // sides — no misalignment.
    const S: &str = "\
model km:
    z string

model arma:
    kind km

model armb:
    y string

oneof oo4 by kind = \"a\":
    \"a\" -> arma
    \"b\" -> armb

model h61:
    cfg oo4
";
    let src = "\
h61 base:
    cfg:
        kind = \"a\"

h61 top uses base:
    cfg:
        kind = \"a\"
        kind:
            z = \"2\"
";
    let (resolved, diags) = compose(S, src, "h61", "top");
    assert!(
        !codes_of(&diags).contains(&codes::INTERNAL_COMPOSE_INVARIANT),
        "{diags:?}"
    );
    let body = resolved.unwrap().body;
    let cfg = sub_block(&body, "cfg").expect("cfg");
    assert_eq!(scalar(cfg, "kind"), Some(&Value::String("a".into())));
    assert_eq!(
        nested_scalar(cfg, "kind", "z"),
        Some(&Value::String("2".into()))
    );
}

#[test]
fn rejected_parent_switch_keeps_nested_plans_aligned() {
    // A REJECTED parent switch (not just an accepted one) must leave
    // nested positions planned over the true surviving membership.
    let src = "\
po base:
    pk = \"b\"
    sub:
        sk = \"sx\"
        v = \"one\"

po mid uses base:
    pk = \"a\"
    note = \"n\"

po top uses mid:
    sub:
        sk = \"sx\"
        v = \"two\"
";
    // mid's switch a→ (from b) discards base's sub carrying the
    // sealed v — rejected. Survivors: base, top (mid contributes
    // nothing). top's restatement of v must then hit base's seal.
    let (_, diags) = compose(NESTED_UNDER_PARENT_SCHEMA, src, "po", "top");
    let seals: Vec<_> = diags
        .iter()
        .filter(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .collect();
    assert!(
        seals.iter().any(|d| d.message.contains("cannot launder")),
        "mid's switch is rejected: {diags:?}"
    );
    assert!(
        seals
            .iter()
            .any(|d| d.message.contains("sub.v") && !d.message.contains("launder")),
        "top's restatement hits base's seal through the surviving \
             group: {diags:?}"
    );
}

#[test]
fn plan_beats_default_for_omitted_nested_discriminators() {
    // The pre-pass's reason to exist: a layer that OMITS a nested
    // discriminator inherits the stack's effective arm, so its
    // zero-item entry warns against that arm's vocabulary — under the
    // layer's own default-filled consult the field would be unknown
    // and the entry silently unclassified.
    let schema = "\
model azN:
    kind string
    hosts []string

model gcpN:
    kind string
    path string

oneof notify by kind = \"gcp\":
    \"az\" -> azN
    \"gcp\" -> gcpN

model svc:
    out notify
";
    let src = "\
svc base:
    out:
        kind = \"az\"
        hosts = [\"a\"]

svc t uses base:
    out:
        hosts = []
";
    let (_, diags) = compose(schema, src, "svc", "t");
    assert!(
        codes_of(&diags).contains(&codes::ZERO_ITEM_LAYER_ENTRY),
        "the omitted-discriminator layer normalizes against the \
             stack's arm, not the schema default: {diags:?}"
    );
}

// ───────────────────────────────────────────────────────────────
// RFC 0025 behavior pins. Laid down in Phase 0 against the old
// engine and FLIPPED with Phase 3 where the RFC changes the
// reading (each flip's old behavior is recorded in its doc);
// P2/Q2/C2/U1 pin behavior both engines share. Values are
// asserted, not just codes — the K-class defect was a SILENT
// artifact (NML2043 short-circuits the validation that would have
// named it).
// ───────────────────────────────────────────────────────────────

/// AS1 (RFC 0025 test plan): the arm-set seal judgment must see the
/// INJECTED arm headers — ThisLevel materializes each inline arm's
/// `name` one hop into the field's block BEFORE `merge_arm_set` judges
/// the displaced set, so a `name string #sealed` on the arm target
/// refuses replacement exactly like an authored sealed field. The
/// injection-ordering guarantee, pinned.
#[test]
fn arm_set_replacement_is_refused_by_the_injected_header_seal() {
    const S: &str = "\
model handlerh:
    name string #sealed
    note string

model routerh:
    route (string -> handlerh)
";
    let src = "\
routerh base:
    route:
        \"a\" -> One:
            note = \"n1\"

routerh t uses base:
    route:
        \"b\" -> Two:
            note = \"n2\"
";
    let (_, diags) = compose(S, src, "routerh", "t");
    assert_eq!(diags.len(), 1, "the replacement is refused once: {diags:?}");
    assert_eq!(diags[0].code, Some(codes::SEALED_FIELD_VIOLATION));
    assert!(
        diags[0].message.contains("route.name"),
        "the INJECTED header field is the named seal: {}",
        diags[0].message
    );
}
