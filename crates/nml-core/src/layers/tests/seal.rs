use super::super::*;
use super::*;

#[test]
fn sealed_field_violation_differing_value() {
    let src = "\
flow base:
    entrypoint = \"search\"

flow hijacked uses base:
    entrypoint = \"adminPanel\"
";
    let (resolved, diags) = compose(FLOW_SCHEMA, src, "flow", "hijacked");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    // Best-effort body keeps the sealed base value.
    let body = resolved.unwrap().body;
    assert_eq!(
        scalar(&body, "entrypoint"),
        Some(&Value::String("search".into()))
    );
}

#[test]
fn sealed_equal_value_restatement_is_2060_with_deletion_fix() {
    let src = "\
flow base:
    entrypoint = \"search\"

flow copy uses base:
    entrypoint = \"search\"
";
    let (_, diags) = compose(FLOW_SCHEMA, src, "flow", "copy");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    assert!(diags[0].message.contains("same value"));
    let sug = diags[0].suggestions.first().expect("deletion suggestion");
    assert!(sug.replacement.is_empty(), "sole-candidate deletion");
}

#[test]
fn sealed_inside_identity_item_stays_sealed() {
    let src = "\
flow base:
    entrypoint = \"search\"
    steps:
        - search:
            action = \"type\"

flow evil uses base:
    steps:
        - search:
            action = \"transfer\"
";
    let (_, diags) = compose(FLOW_SCHEMA, src, "flow", "evil");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
}

// ── identity / append ────────────────────────────────────────────────

#[test]
fn sealed_in_invalid_combo_still_seals() {
    // Fail-closed policy_of: `#sealed #append` composes as Sealed even
    // if a caller skipped validate_merge_policies (which errors it).
    let schema = "\
model m:
    entrypoint string #sealed #append
";
    let src = "\
m base:
    entrypoint = \"search\"

m evil uses base:
    entrypoint = \"adminPanel\"
";
    let (_, diags) = compose(schema, src, "m", "evil");
    assert!(
        codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "a schema typo must never widen a seal: {diags:?}"
    );
}

#[test]
fn zero_item_entry_cannot_seal() {
    // Regression: a base `xs = []` on a `#sealed` list counted as the
    // sealing write, rejecting the next tier's legitimate first
    // assignment — contradicting both NML2079's no-op contract and
    // sealed's stays-open rule.
    let schema = "model m:\n    xs []string #sealed\n";
    let src = "\
m base:
    xs = []

m t uses base:
    xs = [\"a\"]
";
    let (resolved, diags) = compose(schema, src, "m", "t");
    assert!(
        !codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "a zero-item entry is not a write: {diags:?}"
    );
    let body = resolved.unwrap().body;
    let items = list_names(&body, "xs");
    assert_eq!(items.len(), 1, "t's first real assignment seals: {items:?}");
}

#[test]
fn seal_backstop_reaches_list_item_seals() {
    // Regression: an arm switch silently discarded a sealed field
    // assigned INSIDE a list item ("at any depth" was ModelRef-only).
    let schema = "\
model step:
    name string+
    action string #sealed

model az:
    kind string
    steps []step #identity

model sns:
    kind string
    topicArn string

oneof notify by kind:
    \"az\" -> az
    \"sns\" -> sns
";
    let src = "\
notify base:
    kind = \"az\"
    steps:
        - search:
            action = \"type\"

notify top uses base:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
    let (_, diags) = compose(schema, src, "notify", "top");
    assert!(
        codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "backstop must see item-level seals: {diags:?}"
    );
}

#[test]
fn sealed_object_field_is_write_once() {
    let schema = "\
model inner:
    x string

model outer:
    label string
    cfg inner #sealed
";
    let src = "\
outer base:
    label = \"a\"
    cfg:
        x = \"secret\"

outer evil uses base:
    label = \"b\"
    cfg:
        x = \"hijacked\"
";
    let (resolved, diags) = compose(schema, src, "outer", "evil");
    assert!(
        codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "an object body is a write — the seal must fire: {diags:?}"
    );
    let body = resolved.unwrap().body;
    assert_eq!(
        nested_scalar(&body, "cfg", "x"),
        Some(&Value::String("secret".into())),
        "the base's sealed object survives"
    );
}

#[test]
fn arm_switch_backstop_sees_seals_inside_nested_oneofs() {
    let src = "\
notify base:
    kind = \"az\"
    cred:
        kind = \"gcp\"
        keyPath = \"/secret/key\"

notify top uses base:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
    let (resolved, diags) = compose(NESTED_ONEOF_SCHEMA, src, "notify", "top");
    let seal = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("nested-oneof seal blocks the switch");
    assert!(
        seal.message.contains("cred.keyPath"),
        "names the buried seal: {}",
        seal.message
    );
    // Rejected switch: the sealed arm survives.
    assert_eq!(
        scalar(&resolved.unwrap().body, "kind"),
        Some(&Value::String("az".into()))
    );
}

#[test]
fn zero_item_sealed_list_entry_does_not_block_a_switch() {
    let schema = "\
model azR:
    kind string
    hosts []string #sealed

model snsR:
    kind string
    topicArn string

oneof relay by kind = \"az\":
    \"az\" -> azR
    \"sns\" -> snsR
";
    let src = "\
relay base:
    kind = \"az\"
    hosts = []

relay t uses base:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
    let (resolved, diags) = compose(schema, src, "relay", "t");
    assert!(
        !codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "a zero-item entry is not an assigned seal: {diags:?}"
    );
    assert_eq!(
        scalar(&resolved.unwrap().body, "kind"),
        Some(&Value::String("sns".into())),
        "the legal switch proceeds"
    );
}

#[test]
fn arm_switch_reports_multi_seal_count() {
    let schema = "\
model azR:
    kind string
    key1 string #sealed
    key2 string #sealed

model snsR:
    kind string
    topicArn string

oneof relay by kind = \"az\":
    \"az\" -> azR
    \"sns\" -> snsR
";
    let src = "\
relay base:
    kind = \"az\"
    key1 = \"a\"
    key2 = \"b\"

relay t uses base:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
    let (_, diags) = compose(schema, src, "relay", "t");
    let seal = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("backstop fires");
    assert!(
        seal.message.contains("(and 1 more field)"),
        "states the field count: {}",
        seal.message
    );
}

#[test]
fn spelling_is_authoring_not_identity_for_seals() {
    // A modifier-declared sealed field written back in property
    // spelling is the SAME field: the two spellings must meet in one
    // seal check, or each spelling silently dodges the other's seal
    // and the composed body carries two entries for one field.
    let src = "\
policy base:
    label = \"x\"
    |deny = [\"a\"]

policy t uses base:
    deny = [\"b\"]
";
    let (resolved, diags) = compose(MODIFIER_SEAL_SCHEMA, src, "policy", "t");
    assert!(
        codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "cross-spelling write hits the seal: {diags:?}"
    );
    let body = resolved.unwrap().body;
    let deny_entries = body
        .entries
        .iter()
        .filter(|e| match &e.kind {
            BodyEntryKind::Modifier(m) => m.name.name == "deny",
            BodyEntryKind::NestedBlock(nb) => nb.name.name == "deny",
            BodyEntryKind::Property(p) => p.name.name == "deny",
            _ => false,
        })
        .count();
    assert_eq!(deny_entries, 1, "one field, one composed entry");
}

#[test]
fn zero_item_entry_never_violates_a_seal() {
    let schema = "\
model m:
    xs []string #sealed
";
    let src = "\
m base:
    xs = [\"a\"]

m t uses base:
    xs = []
";
    let (resolved, diags) = compose(schema, src, "m", "t");
    assert!(
        !codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "a zero-item entry is the same warned no-op above a seal as \
             everywhere else: {diags:?}"
    );
    assert!(
        codes_of(&diags).contains(&codes::ZERO_ITEM_LAYER_ENTRY),
        "still warned as NML2079: {diags:?}"
    );
    assert_eq!(
        list_names(&resolved.unwrap().body, "xs").len(),
        1,
        "the sealed base data survives"
    );
}

#[test]
fn seal_scan_is_bounded_on_recursive_oneofs() {
    // DoS regression: a per-candidate-arm re-scan of the same child
    // body was exponential in nesting depth over a recursive oneof —
    // the union-vocabulary scan visits each node once. Depth 40 with
    // two candidate arms per level would be ~2^40 re-scans under the
    // old scheme; it must compose instantly.
    let schema = "\
model deepA:
    k string
    child rec

model deepB:
    k string
    child rec

oneof rec by k = \"a\":
    \"a\" -> deepA
    \"b\" -> deepB

model azArm:
    kind string
    secret string #sealed
    root rec

model snsArm:
    kind string
    topicArn string

oneof notify by kind = \"az\":
    \"az\" -> azArm
    \"sns\" -> snsArm
";
    let mut nest = String::new();
    let depth = 40;
    for i in 0..depth {
        let pad = "    ".repeat(i + 2);
        nest.push_str(&format!("{}child:\n{}    k = \"b\"\n", pad, pad));
    }
    let src = format!(
        "notify base:\n    kind = \"az\"\n    secret = \"s\"\n    root:\n        k = \"b\"\n{nest}\nnotify t uses base:\n    kind = \"sns\"\n    topicArn = \"arn:x\"\n"
    );
    let start = std::time::Instant::now();
    let (_, diags) = compose(schema, &src, "notify", "t");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "union-vocabulary scan is linear, took {:?}",
        start.elapsed()
    );
    assert!(
        codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "the assigned seal still blocks the switch: {diags:?}"
    );
}

#[test]
fn backstop_counts_item_seals_distinctly() {
    let schema = "\
model cred:
    name string+
    key string #sealed

model azArm:
    kind string
    creds []cred #identity

model snsArm:
    kind string
    topicArn string

oneof relay by kind = \"az\":
    \"az\" -> azArm
    \"sns\" -> snsArm
";
    let src = "\
relay base:
    kind = \"az\"
    creds:
        - alpha:
            key = \"k1\"
        - beta:
            key = \"k2\"

relay t uses base:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
    let (_, diags) = compose(schema, src, "relay", "t");
    let seal = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("backstop fires");
    assert!(
        seal.message.contains("(and 1 more field)"),
        "two items' seals are two fields, not one masked path: {}",
        seal.message
    );
    assert!(
        seal.message.contains("creds[alpha].key"),
        "item segment names the identity: {}",
        seal.message
    );
}

#[test]
fn positional_injection_never_fabricates_a_seal() {
    // The fold and the merge share ONE decision trace judged over
    // displaced-arm-normalized bodies: machinery materialized under
    // the surviving arm must never read as an authored write to the
    // displaced arm's same-named sealed field.
    let schema = "\
model stepA:
    action string #sealed
    note string

model stepB:
    action string+
    note string

model armA:
    steps []stepA #identity

model armB:
    steps []stepB #identity

oneof job by kind = \"a\":
    \"a\" -> armA
    \"b\" -> armB
";
    let src = "\
job base:
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
        !codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "no layer assigned the sealed field — the switch is legal: {diags:?}"
    );
    assert_eq!(
        scalar(&resolved.unwrap().body, "kind"),
        Some(&Value::String("b".into())),
        "the accepted switch holds"
    );
}

#[test]
fn seal_scan_reaches_arms_in_arms_and_root_positions() {
    // Depth: an arms-typed field INSIDE an arm's inline body still
    // scans (two levels of arm sets); and a rejection at an
    // instance ROOT renders without a position clause.
    const S: &str = "\
model deep:
    pan string #sealed

model hop:
    route2 (string -> deep)

model armx:
    kind string
    route (string -> hop)

model army:
    kind string
    w string

oneof pay3 by kind:
    \"x\" -> armx
    \"y\" -> army
";
    let src = "\
pay3 base:
    kind = \"x\"
    route:
        \"a\" -> H:
            route2:
                \"b\" -> D:
                    pan = \"locked\"

pay3 top uses base:
    kind = \"y\"
    w = \"1\"
";
    let (_, diags) = compose(S, src, "pay3", "top");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("two arm-set levels deep still scans");
    assert!(
        d.message.contains("route.route2.pan")
            && d.message.contains("arm switch to `kind = \"y\"` would"),
        "depth + root-elided position: {}",
        d.message
    );
}

// ── union compose: round-14 battery (list-variant judgment as a
//    LIST, oracle candidates everywhere, zero-item non-supplies,
//    every spelling through the authority, recorded discard faces) ──

#[test]
fn rejection_after_a_new_winner_list_points_at_its_seal() {
    let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"a\"

holder7 mid uses base:
    slot:
        - v:
            kind = \"k\"
            secret = \"b\"

holder7 top uses mid:
    slot as ua:
        x = \"1\"
";
    let (_, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "top");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    assert!(
        diags[0].message.contains("slot[v].secret"),
        "{}",
        diags[0].message
    );
    let mid_at = src.find("holder7 mid").unwrap();
    let top_at = src.find("holder7 top").unwrap();
    let note = diags[0].related[0].span.start;
    assert!(
        note > mid_at && note < top_at,
        "sealed here points at the winner's seal"
    );
}

#[test]
fn nested_sealed_list_items_count_separately_across_items() {
    // Two items, each with a nested sealed list item at the same
    // relative path: distinct paths, two hits.
    const S: &str = "\
model leaf:
    name string+
    a string #sealed

model ub:
    kind string
    steps []leaf #identity

model ua:
    x string

model holder42:
    slot (ua | []ub)
";
    let src = "\
holder42 base:
    slot:
        - w:
            kind = \"k\"
            steps:
                - x:
                    a = \"1\"
        - v:
            kind = \"k\"
            steps:
                - x:
                    a = \"2\"

holder42 top uses base:
    slot as ua:
        x = \"1\"
";
    let (_, diags) = compose(S, src, "holder42", "top");
    assert_eq!(
        codes_of(&diags),
        [codes::SEALED_FIELD_VIOLATION],
        "{diags:?}"
    );
    assert!(
        diags[0].message.contains("slot[w].steps[x].a")
            && diags[0].message.contains("(and 1 more field)"),
        "{}",
        diags[0].message
    );
}

#[test]
fn two_files_with_identical_spans_count_their_seals_separately() {
    // The sink dedups by (path, FILE, span): byte-identical files
    // assigning one sealed field are two assignments, two hits.
    let index = index_from(
        "model ua:\n    x string\n\nmodel ub:\n    y string #sealed\n\n\
             model holder46:\n    slot (ua | ub)\n",
    );
    let same = "holder46 base:\n    slot as ub:\n        y = \"1\"\n";
    let fa = file_of(same);
    let fb = file_of(same);
    let fc = file_of("holder46 top:\n    slot as ua:\n        x = \"2\"\n");
    let ia = InstanceIndex::from_file("a.nml", &fa);
    let ib = InstanceIndex::from_file("b.nml", &fb);
    let ic = InstanceIndex::from_file("c.nml", &fc);
    let mut layers = layers_of(&ia, &["base"]);
    layers.extend(layers_of(&ib, &["base"]));
    layers.extend(layers_of(&ic, &["top"]));
    let mut sink = ComposeSink::new(layers.iter().map(|(id, _)| *id).collect());
    Merger {
        index: &index,
        sink: &mut sink,
        origins: Vec::new(),
    }
    .compose_root("holder46", &layers);
    let diags = sink.finish();
    // The middle file's restatement is itself NML2060; the rejection
    // is ONE field assigned TWICE — `(2 assignments)`, a note per
    // assignment, each carrying its own file structurally
    // (`Related.source`, E17 — the parenthetical left the message).
    assert_eq!(
        codes_of(&diags),
        [codes::SEALED_FIELD_VIOLATION, codes::SEALED_FIELD_VIOLATION],
        "{diags:?}"
    );
    let rejection = diags
        .iter()
        .find(|d| d.message.contains("(2 assignments)"))
        .unwrap_or_else(|| panic!("one field, two files: {diags:?}"));
    assert_eq!(rejection.related.len(), 2, "{rejection:?}");
    for r in &rejection.related {
        assert_eq!(r.message, "sealed here", "{rejection:?}");
    }
    let sources: Vec<&str> = rejection
        .related
        .iter()
        .filter_map(|r| rejection.related_source(r))
        .collect();
    assert!(
        sources.contains(&"a.nml") && sources.contains(&"b.nml"),
        "{rejection:?}"
    );
}

#[test]
fn arm_switch_backstop_combines_field_and_assignment_counts() {
    // The fourth suffix arm — more fields than one AND more
    // assignments than fields: base assigns secret+token, mid
    // RESTATES secret (the scan is value-blind: an equal restatement
    // is still its own (file, span) site), so the displaced group
    // [base, mid] carries 3 sites over 2 identities. The arms
    // declare no `kind` — a declared discriminator-named field is
    // the NML2054 shape, not this fixture's.
    let schema = "\
model arma:
    secret string #sealed
    token string #sealed

model armb:
    note string?

oneof svc by kind = \"a\":
    \"a\" -> arma
    \"b\" -> armb
";
    let src = "\
svc base:
    kind = \"a\"
    secret = \"s\"
    token = \"t\"

svc mid uses base:
    secret = \"s\"

svc top uses mid:
    kind = \"b\"
";
    let (_, diags) = compose(schema, src, "svc", "top");
    // Exactly two NML2060s: the backstop rejection plus mid's own
    // equal-value restatement (merge_sealed). Unit-level order is
    // NOT the CLI's span-sorted print — locate by message, never by
    // index.
    assert_eq!(
        codes_of(&diags),
        [codes::SEALED_FIELD_VIOLATION, codes::SEALED_FIELD_VIOLATION],
        "{diags:?}"
    );
    let rejection = diags
        .iter()
        .find(|d| d.message.starts_with("arm switch to `kind = \"b\"`"))
        .unwrap_or_else(|| panic!("backstop rejection: {diags:?}"));
    assert!(
        rejection.message.contains(
            "would discard the assigned `#sealed` field 'secret' \
                 (and 1 more field; 3 assignments)"
        ),
        "{}",
        rejection.message
    );
    assert_eq!(
        rejection.related.len(),
        3,
        "one note per site: {rejection:?}"
    );
    assert!(
        rejection.related.iter().all(|r| r.message == "sealed here"),
        "{rejection:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("already sealed to this same value")),
        "the incidental restatement is the deletion-fix flavor: {diags:?}"
    );
}

#[test]
fn a_wide_backstop_judgment_counts_in_linear_time() {
    // Regression (DoS class): the emitter's field/assignment dedup
    // was a `Vec::contains` per hit — quadratic, measured near a
    // minute for 64k hits from a 1.1 MB input. Sets keep it linear;
    // the bound below is ~500x the linear cost and far under the
    // quadratic one, so it pins the complexity class, not the
    // machine.
    let file = file_of("spec base:\n    x = \"1\"\n");
    let ia = InstanceIndex::from_file("a.nml", &file);
    let id = ia.resolve_ref("base").unwrap();
    let seals: SealHits<'_> = (0..60_000usize)
        .map(|i| SealHit {
            id: FieldIdentity::default()
                .child(Seg::Item(ItemKey::Scalar(Value::number(i as i64))))
                .child(Seg::Field("secret".into())),
            span: Span::new(i, i + 1),
            layer: id,
        })
        .collect();
    let t0 = std::time::Instant::now();
    let d = seal_backstop_rejection(
        BackstopFace::ArmSetReplacement,
        "xs",
        &seals,
        Span::new(0, 1),
        id,
    );
    assert!(
        d.message.contains("(and 59999 more fields)"),
        "{}",
        d.message
    );
    assert_eq!(d.related.len(), RELATED_SEALS, "notes stay capped");
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(5),
        "quadratic emitter: {:?}",
        t0.elapsed()
    );
}

#[test]
fn equal_value_seal_detection_spans_spellings() {
    let schema = "\
model m:
    a string #sealed
";
    let src = "\
m base:
    a = \"x\"

m t uses base:
    |a = \"x\"
";
    let (_, diags) = compose(schema, src, "m", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("seal fires");
    assert!(
        d.message.contains("delete this assignment"),
        "the equal-value form (and its machine fix) survives the \
             modifier spelling: {}",
        d.message
    );
    assert!(!d.suggestions.is_empty());
}

#[test]
fn same_layer_sealed_duplicates_read_correctly() {
    let schema = "\
model m:
    a string #sealed
";
    let src = "\
m base:
    v0 = \"x\"

m t uses base:
    a = \"x\"
    a = \"y\"
";
    let (_, diags) = compose(schema, src, "m", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("seal fires");
    assert!(
        d.message.contains("in this same layer"),
        "same-body duplicates name the real relationship: {}",
        d.message
    );
}

#[test]
fn scalar_spelling_cannot_launder_an_object_seal() {
    // An object-typed field always deep-merges its nested
    // contributions — a scalar/modifier spelling (invalid for an
    // object field) must never win and discard a sealed nested body.
    let schema = "\
model inner:
    path string #sealed

model outer:
    cfg inner
    label string
";
    for top in [
        "outer t uses base:\n    cfg = \"gone\"\n    label = \"x\"\n",
        "outer t uses base:\n    |cfg = \"gone\"\n    label = \"x\"\n",
    ] {
        let src = format!("outer base:\n    cfg:\n        path = \"secret\"\n\n{top}");
        let (resolved, _) = compose(schema, &src, "outer", "t");
        assert_eq!(
            nested_scalar(&resolved.unwrap().body, "cfg", "path"),
            Some(&Value::String("secret".into())),
            "the sealed nested body survives the scalar spelling: {top}"
        );
    }
}

#[test]
fn modifier_backstop_uses_the_shared_write_predicate() {
    // The seal-scan Modifier arm must judge a WRITE the same way
    // `merge_sealed` does (`seal_write`) — a non-list sealed field
    // writes with every entry, so a modifier-spelled restatement in
    // a displaced arm must block the switch.
    let schema = "\
model ara:
    kind string
    v string #sealed

model arb:
    kind string
    other string

oneof cf by kind = \"a\":
    \"a\" -> ara
    \"b\" -> arb

model box2:
    cfg cf
";
    let src = "\
box2 base:
    cfg:
        |v = \"x\"

box2 t uses base:
    cfg:
        kind = \"b\"
        other = \"y\"
";
    let (_, diags) = compose(schema, src, "box2", "t");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)
                && d.message.contains("cannot launder")),
        "modifier-spelled non-list seal blocks the switch: {diags:?}"
    );
}

#[test]
fn modifier_spelled_items_enforce_and_backstop_seals() {
    // The modifier spelling is the same list: its items' seals bind
    // in the ordinary merge...
    let src = "\
box base:
    cfg:
        |steps:
            - s1:
                act = \"x\"

box t uses base:
    cfg:
        |steps:
            - s1:
                act = \"y\"
";
    let (_, diags) = compose(MODIFIER_ITEM_SEAL_SCHEMA, src, "box", "t");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)
                && d.message.contains("steps[s1].act")),
        "modifier-spelled item seal enforced: {diags:?}"
    );
    // ...and in the arm-switch backstop.
    let src2 = "\
box base:
    cfg:
        |steps:
            - s1:
                act = \"x\"

box t uses base:
    cfg:
        skind = \"b\"
        other = \"y\"
";
    let (_, diags) = compose(MODIFIER_ITEM_SEAL_SCHEMA, src2, "box", "t");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)
                && d.message.contains("cannot launder")),
        "modifier-spelled item seal blocks the switch: {diags:?}"
    );
}

/// F2's loser side (RFC 0025 §4, the sealed-position row): the upper
/// write on a sealed OBJECT field is rejected (NML2060) AND the
/// rejected body's interior is diagnosed under its OWN reading —
/// `zs = []` is a list there, so its zero-item verdict (NML2079)
/// surfaces from inside the loser.
#[test]
fn a_sealed_loser_diagnoses_its_interior_under_its_own_reading() {
    const S: &str = "\
model innerf:
    zs []string
    v string

model outerf:
    sub innerf #sealed
";
    let src = "\
outerf base:
    sub:
        v = \"1\"

outerf t uses base:
    sub:
        zs = []
";
    let (_, diags) = compose(S, src, "outerf", "t");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)),
        "the upper write is rejected: {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::ZERO_ITEM_LAYER_ENTRY) && d.message.contains("zs")),
        "the loser's interior verdict surfaces under its own reading: {diags:?}"
    );
}
