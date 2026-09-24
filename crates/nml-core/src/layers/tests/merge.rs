use super::super::*;
use super::*;

#[test]
fn summary_example_composes() {
    let (resolved, diags) = compose(FLOW_SCHEMA, SUMMARY, "flow", "cuXyz");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    assert_eq!(
        scalar(&body, "entrypoint"),
        Some(&Value::String("search".into()))
    );
    assert_eq!(list_names(&body, "steps"), ["search", "submitSearch"]);
    // The overlay re-targeted submitSearch's locator; action stayed.
    let steps = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "steps" => Some(&nb.body),
            _ => None,
        })
        .unwrap();
    let submit = steps
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::ListItem(ListItem {
                kind: ListItemKind::Named { name, body },
                ..
            }) if name.name == "submitSearch" => Some(body),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        scalar(submit, "locator"),
        Some(&Value::String("#search-button".into()))
    );
    assert_eq!(
        scalar(submit, "action"),
        Some(&Value::String("click".into()))
    );
}

// ── sealed ───────────────────────────────────────────────────────────

#[test]
fn unmatched_overlay_item_is_2067_with_named_hint() {
    let src = "\
flow base:
    entrypoint = \"a\"
    steps:
        - submitSearch:
            action = \"click\"

flow t uses base:
    steps:
        - submitSaerch:
            locator = \"#x\"
";
    let (resolved, diags) = compose(FLOW_SCHEMA, src, "flow", "t");
    assert_eq!(codes_of(&diags), [codes::UNMATCHED_OVERLAY_ITEM]);
    assert!(
        diags[0].message.contains("submitSearch"),
        "{}",
        diags[0].message
    );
    // Best-effort: the item was skipped, base list survives.
    assert_eq!(
        list_names(&resolved.unwrap().body, "steps"),
        ["submitSearch"]
    );
}

#[test]
fn append_alone_rejects_redefinition_and_adds_new() {
    let src = "\
flow base:
    steps:
        - audit:
            action = \"log\"

flow t uses base:
    steps:
        - audit:
            action = \"noop\"
        - extra:
            action = \"click\"
";
    let (resolved, diags) = compose(APPEND_SCHEMA, src, "flow", "t");
    assert_eq!(codes_of(&diags), [codes::IDENTITY_REDEFINITION]);
    assert_eq!(
        list_names(&resolved.unwrap().body, "steps"),
        ["audit", "extra"],
        "additions at the back; base immutable"
    );
}

#[test]
fn dead_delta_warns_on_overlay_restatement() {
    let src = "\
policy base:
    label = \"x\"

policy t uses base:
    label = \"x\"
";
    let (_, diags) = compose(DENY_SCHEMA, src, "policy", "t");
    assert_eq!(codes_of(&diags), [codes::DEAD_DELTA]);
}

// ── linearization ────────────────────────────────────────────────────

#[test]
fn provenance_origin_points_at_winning_layer_span() {
    let (resolved, _) = compose(FLOW_SCHEMA, SUMMARY, "flow", "cuXyz");
    let r = resolved.unwrap();
    let (_, origin) = r
        .origins
        .iter()
        .find(|(p, _)| p == "entrypoint")
        .expect("entrypoint recorded");
    let Origin::File { file, span } = origin else {
        panic!("schema defaults don't run at compose");
    };
    assert_eq!(file.to_str().unwrap(), "main.nml");
    // The base's `entrypoint = "search"` assignment — its span must
    // enclose that text in the source.
    assert!(span.start < span.end);
}

#[test]
fn modifier_append_merges_inline_and_block_spellings() {
    let src = "\
policy base:
    label = \"x\"
    |deny = [\"a\"]

policy t uses base:
    |deny:
        - \"b\"
";
    let (resolved, diags) = compose(MODIFIER_SCHEMA, src, "policy", "t");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    let items: Vec<String> = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::Modifier(m) => match &m.value {
                ModifierValue::Block(items) => Some(
                    items
                        .iter()
                        .filter_map(|i| match &i.kind {
                            ListItemKind::Shorthand { value, .. } => {
                                value.value.as_str().map(str::to_string)
                            }
                            _ => None,
                        })
                        .collect(),
                ),
                _ => None,
            },
            _ => None,
        })
        .expect("merged modifier present as Block");
    assert_eq!(items, ["a", "b"], "deny list grew upward across spellings");
}

#[test]
fn overlay_modifier_collision_replaces_wholesale_no_panic() {
    // Regression: bare (overlay-policy) modifiers with a colliding item
    // used to hit merge_items' list-policies-only unreachable!.
    let schema = "\
model policy:
    label string
    |deny []string
";
    let src = "\
policy base:
    label = \"x\"
    |deny = [\"a\"]

policy t uses base:
    |deny = [\"b\"]
";
    let (resolved, diags) = compose(schema, src, "policy", "t");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    let items: Vec<String> = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::Modifier(m) => match &m.value {
                ModifierValue::Block(items) => Some(
                    items
                        .iter()
                        .filter_map(|i| match &i.kind {
                            ListItemKind::Shorthand { value, .. } => {
                                value.value.as_str().map(str::to_string)
                            }
                            _ => None,
                        })
                        .collect(),
                ),
                _ => None,
            },
            _ => None,
        })
        .expect("modifier present");
    assert_eq!(items, ["b"], "bare modifier replaces wholesale");
}

#[test]
fn compose_file_substitutes_resolved_and_removes_failed() {
    let index = index_from(LIN_SCHEMA);
    let src = "\
thing base:
    v = \"b\"

thing good uses base:
    v = \"g\"

thing bad uses missing:
    v = \"x\"
";
    let file = file_of(src);
    let out = compose_file(&index, "main.nml", &file, &OpenContext);
    assert!(codes_of(&out.diagnostics).contains(&codes::UNRESOLVED_LAYER_REF));
    let vf = out.validation_file.expect("uses present");
    // `bad` removed (failed compose must not cascade); `good` and
    // `base` remain, `good` carrying its RESOLVED body.
    assert_eq!(vf.declarations.len(), 2);
    let good = vf
        .declarations
        .iter()
        .find_map(|d| match &d.kind {
            crate::ast::DeclarationKind::Block(b) if b.name.name == "good" => Some(b),
            _ => None,
        })
        .expect("good survives");
    assert_eq!(
        scalar(&good.body, "v"),
        Some(&Value::String("g".into())),
        "resolved body substituted"
    );
}

#[test]
fn compose_file_none_when_no_uses() {
    let index = index_from(LIN_SCHEMA);
    let file = file_of("thing base:\n    v = \"b\"\n");
    let out = compose_file(&index, "main.nml", &file, &OpenContext);
    assert!(out.validation_file.is_none());
    assert!(out.diagnostics.is_empty());
}

#[test]
fn compose_file_dedups_shared_substack_findings() {
    let index = index_from(DENY_SCHEMA);
    let src = "\
policy base:
    label = \"x\"
    denyHosts = [\"a\"]

policy mid uses base:
    denyHosts = []

policy top uses mid:
    label = \"y\"
";
    let file = file_of(src);
    let out = compose_file(&index, "main.nml", &file, &OpenContext);
    let zero_items: Vec<_> = out
        .diagnostics
        .iter()
        .filter(|d| d.code == Some(codes::ZERO_ITEM_LAYER_ENTRY))
        .collect();
    assert_eq!(
        zero_items.len(),
        1,
        "mid's zero-item entry reports once, not per composing root: {:?}",
        out.diagnostics
    );
}

// ── provenance ───────────────────────────────────────────────────────

#[test]
fn provenance_records_winning_layers() {
    let (resolved, _) = compose(FLOW_SCHEMA, SUMMARY, "flow", "cuXyz");
    let origins = resolved.unwrap().origins;
    assert!(
        origins.iter().any(|(p, _)| p == "entrypoint"),
        "sealed base assignment recorded: {origins:?}"
    );
    assert!(
        origins.iter().any(|(p, _)| p.starts_with("steps[")),
        "item identities recorded: {origins:?}"
    );
}

// ── round-4 review pins ──────────────────────────────────────────────

#[test]
fn one_layer_stating_a_field_twice_is_one_layer() {
    let schema = "\
model item:
    name string+
    v string

model m:
    xs []item #identity
";
    // Two spellings in ONE body: the layer's own later entry is not an
    // unmatched overlay, and a duplicated identity across the two
    // entries is the within-layer duplicate error.
    let src_ok = "\
m base:
    xs = [\"a\"]
    xs:
        - \"b\"

m t uses base:
    xs:
        - \"a\":
            v = \"x\"
";
    let (resolved, diags) = compose(schema, src_ok, "m", "t");
    assert!(
        !codes_of(&diags).contains(&codes::UNMATCHED_OVERLAY_ITEM),
        "the base's own second entry is base, not overlay: {diags:?}"
    );
    assert_eq!(list_names(&resolved.unwrap().body, "xs").len(), 2);

    let src_dup = "\
m base:
    xs = [\"a\"]
    xs:
        - \"a\"

m t uses base:
    xs:
        - \"a\":
            v = \"x\"
";
    let (_, diags) = compose(schema, src_dup, "m", "t");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::IDENTITY_REDEFINITION)
                && d.message.contains("duplicate identity in one layer")),
        "cross-spelling within-layer duplicate is caught: {diags:?}"
    );
}

#[test]
fn schemaless_nested_groups_still_deep_merge() {
    // Structural (no-schema) composition is a documented,
    // fixture-pinned capability: all-nested groups with no resolvable
    // object target — no schema, an undeclared field, a dangling
    // type — deep-merge name-keyed. The target-routed object path
    // must not drop them to wholesale replacement (silently
    // discarding every lower layer's nested data).
    let src = "\
box base:
    cfg:
        x = \"1\"
        sub:
            deep = \"d\"

box t uses base:
    cfg:
        y = \"2\"
";
    // No schema at all: compose structurally.
    let (resolved, diags) = compose("", src, "box", "t");
    assert!(diags.is_empty(), "structural compose is clean: {diags:?}");
    let body = resolved.unwrap().body;
    assert_eq!(
        nested_scalar(&body, "cfg", "x"),
        Some(&Value::String("1".into())),
        "the base's nested data survives"
    );
    assert_eq!(
        nested_scalar(&body, "cfg", "y"),
        Some(&Value::String("2".into())),
        "the overlay deep-merges in"
    );
    let sub_deep = body.entries.iter().find_map(|e| match &e.kind {
        BodyEntryKind::NestedBlock(nb) if nb.name.name == "cfg" => {
            nested_scalar(&nb.body, "sub", "deep")
        }
        _ => None,
    });
    assert_eq!(
        sub_deep,
        Some(&Value::String("d".into())),
        "recursively, not just one level"
    );
    // Dangling type name: same contract.
    let schema = "model box:\n    cfg ghost\n";
    let (resolved, _) = compose(schema, src, "box", "t");
    let body = resolved.unwrap().body;
    assert_eq!(
        nested_scalar(&body, "cfg", "x"),
        Some(&Value::String("1".into()))
    );
    assert_eq!(
        nested_scalar(&body, "cfg", "y"),
        Some(&Value::String("2".into()))
    );
}

// ── union compose (RFC 0015) ─────────────────────────────────────────

#[test]
fn declarations_pass_through_beside_the_value_everywhere() {
    // A type-annotation modifier is a declaration on EVERY route:
    // it never deletes a scalar value (the old last-wins), never
    // desynchronizes the plan (a fabricated refusal), never writes a
    // seal — and it survives into the composed view so the validator
    // still checks it.
    const BOX: &str = "\
model box:
    label string
";
    for src in [
        "box base:\n    label = \"a\"\n\nbox t uses base:\n    |label string\n",
        "box base:\n    label = \"a\"\n\nbox t uses base:\n    |label string\n    label = \"b\"\n",
    ] {
        let (resolved, diags) = compose(BOX, src, "box", "t");
        assert!(diags.is_empty(), "{src}: {diags:?}");
        let body = resolved.unwrap().body;
        assert!(
            scalar(&body, "label").is_some(),
            "the value survives: {src}"
        );
        assert!(
                body.entries.iter().any(|e| matches!(&e.kind,
                    BodyEntryKind::Modifier(m) if matches!(m.value, ModifierValue::TypeAnnotation { .. }))),
                "the declaration passes through: {src}"
            );
    }

    // Plan parity: one annotation line above a positional-token stack
    // used to misalign the plan and refold over final-variant
    // bodies (the r15 fabricated-refusal shape).
    const S: &str = "\
model ita:
    name string+
    key string? #sealed

model itb:
    key string+ #sealed

model ua:
    items []ita

model ub:
    items []itb

model holder25:
    slot (ua | ub)
";
    let src = "\
holder25 base:
    slot as ua:
        items:
            - \"w\"

holder25 top uses base:
    |slot (ua | ub)
    slot as ub:
        items:
            - \"k\"
";
    let (resolved, diags) = compose(S, src, "holder25", "top");
    assert!(diags.is_empty(), "no fabricated refusal: {diags:?}");
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ub")
    );

    // Sealed union: the annotation above the establishment is not a
    // write.
    const SEALED: &str = "\
model ua:
    x string

model ub:
    y string

model holder26:
    slot (ua | ub) #sealed
";
    let src = "\
holder26 base:
    slot as ua:
        x = \"1\"

holder26 top uses base:
    |slot (ua | ub)
";
    let (resolved, diags) = compose(SEALED, src, "holder26", "top");
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ua")
    );
}

#[test]
fn internal_invariant_wording_elides_the_root() {
    let root = internal_invariant_diag("", "a probe", InvariantOutcome::Dropped);
    assert!(
        root.message
            .starts_with("internal composition invariant violated (a probe)"),
        "{}",
        root.message
    );
    assert_eq!(root.code, Some(codes::INTERNAL_COMPOSE_INVARIANT));
    let nested = internal_invariant_diag("a.b", "a probe", InvariantOutcome::Dropped);
    assert!(
        nested.message.contains("violated at 'a.b' (a probe)"),
        "{}",
        nested.message
    );
}

#[test]
fn declarations_pass_through_on_every_policy() {
    // Beyond the scalar/union cells: identity lists, append
    // modifiers, oneof fields, sealed fields with a value — the
    // declaration passes through beside the composed value, and the
    // LAST declaration wins.
    const S: &str = "\
model step:
    name string+
    action string

model flow2:
    steps []step #identity
    |deny []string #append
    label string #sealed
";
    let src = "\
flow2 base:
    steps:
        - a:
            action = \"x\"
    |deny = [\"a\"]
    label = \"l\"
    |steps []step

flow2 t uses base:
    steps:
        - a:
            action = \"y\"
    |deny:
        - \"b\"
    |steps []step
    |deny []string
";
    let (resolved, diags) = compose(S, src, "flow2", "t");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    let decls: Vec<&BodyEntry> = body
        .entries
        .iter()
        .filter(|e| {
            matches!(&e.kind, BodyEntryKind::Modifier(m)
                if matches!(m.value, ModifierValue::TypeAnnotation { .. }))
        })
        .collect();
    assert_eq!(
        decls.len(),
        2,
        "one declaration per declared field: {body:?}"
    );
    let t_at = src.find("flow2 t uses").unwrap();
    assert!(
        decls.iter().all(|d| d.span.start > t_at),
        "the LAST declaration wins"
    );
    assert!(
        sub_block(&body, "steps").is_some(),
        "the identity list composed"
    );
    assert_eq!(scalar(&body, "label"), Some(&Value::String("l".into())));
}

#[test]
fn field_route_table_enumerates_every_cell() {
    // Ownership, cell by cell — a literal table, not a re-derivation:
    // seal beats union beats all-modifier beats policy.
    let index = index_from(UNION_SCHEMA);
    let union_ty = index
        .model("holder")
        .unwrap()
        .fields
        .iter()
        .find(|f| f.name == "slot")
        .map(|f| f.field_type.clone())
        .unwrap();
    use FieldRoute as R;
    use MergePolicy as P;
    type Is = fn(&FieldRoute<'_>) -> bool;
    let sealed: Is = |r| matches!(r, R::Sealed);
    let union: Is = |r| matches!(r, R::Union(_));
    let modifier: Is = |r| matches!(r, R::Modifier);
    let list: Is = |r| matches!(r, R::List);
    let overlay: Is = |r| matches!(r, R::Overlay);
    let table: [(&str, P, bool, bool, Is); 20] = [
        ("overlay", P::Overlay, false, false, overlay),
        ("overlay", P::Overlay, false, true, modifier),
        ("overlay", P::Overlay, true, false, union),
        ("overlay", P::Overlay, true, true, union),
        ("sealed", P::Sealed, false, false, sealed),
        ("sealed", P::Sealed, false, true, sealed),
        ("sealed", P::Sealed, true, false, sealed),
        ("sealed", P::Sealed, true, true, sealed),
        ("identity", P::Identity, false, false, list),
        ("identity", P::Identity, false, true, modifier),
        ("identity", P::Identity, true, false, union),
        ("identity", P::Identity, true, true, union),
        ("append", P::Append, false, false, list),
        ("append", P::Append, false, true, modifier),
        ("append", P::Append, true, false, union),
        ("append", P::Append, true, true, union),
        ("identity+append", P::IdentityAppend, false, false, list),
        ("identity+append", P::IdentityAppend, false, true, modifier),
        ("identity+append", P::IdentityAppend, true, false, union),
        ("identity+append", P::IdentityAppend, true, true, union),
    ];
    for (name, policy, has_union, all_modifiers, is) in table {
        let ty = has_union.then_some(&union_ty);
        let route = route_of(policy, ty, all_modifiers);
        assert!(
            is(&route),
            "cell ({name}, union={has_union}, all_modifiers={all_modifiers}): {route:?}"
        );
    }
    // The union route carries the position's own type.
    assert!(matches!(
        route_of(P::Overlay, Some(&union_ty), false),
        R::Union(t) if std::ptr::eq(t, &union_ty)
    ));
}

#[test]
fn declarations_precede_their_value_and_the_last_one_wins_within_a_layer() {
    // Declare, then assign — as authored; two declarations in ONE
    // layer keep the last; an undeclared name's declaration passes
    // through ahead of its value too.
    const S: &str = "\
model box2:
    |label string
";
    let src = "\
box2 base:
    |label = \"a\"

box2 t uses base:
    |label string
    |label string?
    |label = \"b\"
";
    let (resolved, diags) = compose(S, src, "box2", "t");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    let is_decl = |e: &BodyEntry| {
        matches!(&e.kind, BodyEntryKind::Modifier(m)
                if matches!(m.value, ModifierValue::TypeAnnotation { .. }))
    };
    let decl_at = body.entries.iter().position(is_decl);
    let value_at = body
        .entries
        .iter()
        .position(|e| matches!(&e.kind, BodyEntryKind::Modifier(_)) && !is_decl(e));
    let (Some(d), Some(v)) = (decl_at, value_at) else {
        panic!("{body:?}");
    };
    assert!(d < v, "declare, then assign: {body:?}");
    assert_eq!(
        body.entries.iter().filter(|e| is_decl(e)).count(),
        1,
        "one declaration survives: {body:?}"
    );
    let BodyEntryKind::Modifier(m) = &body.entries[d].kind else {
        panic!("{body:?}");
    };
    let ModifierValue::TypeAnnotation { optional, .. } = &m.value else {
        panic!("{body:?}");
    };
    assert!(*optional, "the LAST declaration (`string?`) wins: {body:?}");

    let src = "\
box2 base:
    |label = \"a\"

box2 t uses base:
    |zz string
    |zz = \"v\"
";
    let (resolved, diags) = compose(S, src, "box2", "t");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    let zz: Vec<usize> = body
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(&e.kind, BodyEntryKind::Modifier(m) if m.name.name == "zz"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(zz.len(), 2, "declaration and value: {body:?}");
    assert!(is_decl(&body.entries[zz[0]]), "declaration first: {body:?}");
}

#[test]
fn the_annotation_source_follows_pins_and_switches() {
    // Pin then switch: the switching layer's token; switch then a
    // restated `as`: still the switch's token.
    const S: &str = "\
model ua:
    note string

model ub:
    note string

model uc:
    note string

model holder41:
    slot (ua | ub | uc)
";
    let src = "\
holder41 base:
    slot:
        note = \"1\"

holder41 mid uses base:
    slot as ua:
        note = \"2\"

holder41 top uses mid:
    slot as uc:
        note = \"3\"
";
    let (resolved, diags) = compose(S, src, "holder41", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    let ann = sub_block(&body, "slot")
        .and_then(|b| b.type_annotation.clone())
        .expect("annotated");
    assert_eq!(ann.name, "uc");
    assert_eq!(
        ann.span.start,
        src.find("slot as uc").unwrap() + "slot as ".len()
    );

    let src = "\
holder41 base:
    slot as ua:
        note = \"1\"

holder41 mid uses base:
    slot as ub:
        note = \"2\"

holder41 top uses mid:
    slot as ub:
        note = \"3\"
";
    let (resolved, diags) = compose(S, src, "holder41", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    let ann = sub_block(&body, "slot")
        .and_then(|b| b.type_annotation.clone())
        .expect("annotated");
    assert_eq!(ann.name, "ub");
    assert_eq!(
        ann.span.start,
        src.find("slot as ub").unwrap() + "slot as ".len(),
        "the switch's token, not the restatement's"
    );
}

#[test]
fn three_layer_identity_item_provenance_is_the_base_span() {
    // A merged identity item keeps the HEAD item's position and
    // layer in the provenance table — the base, when nothing
    // switches (as here; its fields carry their own writers' rows)
    // — like a deep-merged object.
    const S: &str = "\
model step:
    name string+
    action string

model flow5:
    steps []step #identity
";
    let src = "\
flow5 base:
    steps:
        - a:
            action = \"x\"

flow5 mid uses base:
    steps:
        - a:
            action = \"y\"

flow5 top uses mid:
    steps:
        - a:
            action = \"z\"
";
    let (resolved, diags) = compose(S, src, "flow5", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let origins = resolved.unwrap().origins;
    let item_rows: Vec<&Origin> = origins
        .iter()
        .filter(|(p, _)| p == "steps[a]")
        .map(|(_, o)| o)
        .collect();
    assert!(!item_rows.is_empty(), "{origins:?}");
    let base_item = src.find("- a:").unwrap();
    for o in item_rows {
        let Origin::File { span, .. } = o else {
            panic!("{o:?}");
        };
        assert_eq!(span.start, base_item, "the base item's span: {origins:?}");
    }
    let action = origins
        .iter()
        .filter(|(p, _)| p == "steps[a].action")
        .map(|(_, o)| o)
        .next_back()
        .expect("the field's row");
    let Origin::File { span, .. } = action else {
        panic!("{action:?}");
    };
    assert_eq!(
        span.start,
        src.rfind("action = \"z\"").unwrap(),
        "the field's own writer"
    );
}

#[test]
fn a_switched_oneof_field_entry_follows_the_switching_layer() {
    // The head rule (RFC 0019 E15): after an accepted arm switch the
    // composed entry carries the SWITCHING layer's span, name and
    // provenance row — a finding on the composed block (the switched
    // arm's missing required field) then anchors at the layer that
    // produced the body, not at the displaced base, and two
    // switching dependents keep two distinct anchors instead of
    // collapsing onto the base's under the one-home dedup key.
    const S: &str = "\
model ga2:
    kind string
    path string

model gb2:
    kind string
    url string

oneof gcf2 by kind = \"a\":
    \"a\" -> ga2
    \"b\" -> gb2

model wrap2:
    cfg gcf2
";
    let src = "\
wrap2 base:
    cfg:
        kind = \"a\"
        path = \"p\"

wrap2 top uses base:
    cfg:
        kind = \"b\"
        url = \"u\"
";
    let (resolved, diags) = compose(S, src, "wrap2", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let resolved = resolved.unwrap();
    let top_cfg = src.rfind("cfg:").unwrap();
    let entry = resolved
        .body
        .entries
        .iter()
        .find(|e| matches!(&e.kind, BodyEntryKind::NestedBlock(nb) if nb.name.name == "cfg"))
        .expect("composed cfg entry");
    assert_eq!(entry.span.start, top_cfg, "the switching layer's entry");
    let row = resolved
        .origins
        .iter()
        .find(|(p, _)| p == "cfg")
        .expect("cfg provenance row");
    let Origin::File { span, .. } = &row.1 else {
        panic!("{row:?}");
    };
    assert_eq!(span.start, top_cfg, "provenance follows the head");
}

#[test]
fn an_arm_switch_inside_a_joined_union_variant_follows_the_switcher() {
    // Route 2 of the head rule (RFC 0019 E15): the base establishes
    // a union's ONEOF variant; the top joins the union (an
    // un-annotated body over a named establishment joins) but
    // switches the arm INSIDE it — the composed entry carries the
    // joining-then-switching layer's span, threaded through
    // `merge_variant_group`'s group-relative head and the `members`
    // index. The union annotation stays the establishment's.
    const S: &str = "\
model ua:
    x string

model va3:
    a string

model vb3:
    b string

oneof oo3 by kind = \"va\":
    \"va\" -> va3
    \"vb\" -> vb3

model holder50:
    slot (ua | oo3)
";
    let src = "\
holder50 base:
    slot as oo3:
        kind = \"va\"
        a = \"1\"

holder50 top uses base:
    slot:
        kind = \"vb\"
        b = \"2\"
";
    let (resolved, diags) = compose(S, src, "holder50", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let resolved = resolved.unwrap();
    let top_slot = src.rfind("slot:").unwrap();
    let entry = resolved
        .body
        .entries
        .iter()
        .find(|e| matches!(&e.kind, BodyEntryKind::NestedBlock(nb) if nb.name.name == "slot"))
        .expect("composed slot entry");
    assert_eq!(entry.span.start, top_slot, "the switching layer's entry");
    let row = resolved
        .origins
        .iter()
        .find(|(p, _)| p == "slot")
        .expect("slot provenance row");
    let Origin::File { span, .. } = &row.1 else {
        panic!("{row:?}");
    };
    assert_eq!(span.start, top_slot, "provenance follows the head");
}

#[test]
fn a_switched_identity_item_follows_the_switching_layers_span_and_owner() {
    // The head rule at ITEM scope — the switching twin of
    // `three_layer_identity_item_provenance_is_the_base_span`: an
    // identity item whose body switches arms carries the switching
    // item's span and provenance row.
    const S: &str = "\
model va2:
    kind string
    a string

model vb2:
    kind string
    b string

oneof oo2 by kind = \"va\":
    \"va\" -> va2
    \"vb\" -> vb2

model holder42:
    xs []oo2 #identity
";
    let src = "\
holder42 base:
    xs:
        - w:
            kind = \"va\"
            a = \"1\"

holder42 top uses base:
    xs:
        - w:
            kind = \"vb\"
            b = \"2\"
";
    let (resolved, diags) = compose(S, src, "holder42", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let resolved = resolved.unwrap();
    let top_item = src.rfind("- w:").unwrap();
    let rows: Vec<&Origin> = resolved
        .origins
        .iter()
        .filter(|(p, _)| p == "xs[w]")
        .map(|(_, o)| o)
        .collect();
    assert!(!rows.is_empty(), "{:?}", resolved.origins);
    for o in rows {
        let Origin::File { span, .. } = o else {
            panic!("{o:?}");
        };
        assert_eq!(span.start, top_item, "the switching item's span");
    }
}

#[test]
fn a_declaration_from_a_lower_layer_still_precedes_the_upper_value() {
    const S: &str = "\
model box3:
    |label string
";
    for src in [
        "box3 base:\n    |label string\n\nbox3 t uses base:\n    |label = \"b\"\n",
        "box3 base:\n    |label = \"a\"\n\nbox3 t uses base:\n    |label string\n",
        "box3 base:\n    |zz string\n\nbox3 t uses base:\n    |zz = \"v\"\n",
        "box3 base:\n    |label string\n\nbox3 mid uses base:\n    |label = \"a\"\n\nbox3 t uses mid:\n    |label string?\n",
    ] {
        let (resolved, diags) = compose(S, src, "box3", "t");
        assert!(diags.is_empty(), "{src}: {diags:?}");
        let body = resolved.unwrap().body;
        let is_decl = |e: &BodyEntry| {
            matches!(&e.kind, BodyEntryKind::Modifier(m)
                    if matches!(m.value, ModifierValue::TypeAnnotation { .. }))
        };
        let decls: Vec<usize> = body
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| is_decl(e))
            .map(|(i, _)| i)
            .collect();
        let value = body
            .entries
            .iter()
            .position(|e| matches!(&e.kind, BodyEntryKind::Modifier(_)) && !is_decl(e))
            .unwrap_or_else(|| panic!("{src}: {body:?}"));
        assert_eq!(decls.len(), 1, "{src}: one declaration: {body:?}");
        assert!(decls[0] < value, "{src}: declare, then assign: {body:?}");
    }
}

#[test]
fn dependent_composes_do_not_rereport_a_clause_finding() {
    // The declaring-clause NML2059 goes through the one-home dedup
    // like every other finding — a dependent block's compose
    // re-encounters the same clause and must not re-report it.
    let schema = "model thing:\n    v string\n";
    let src = "\
thing bad uses missing:
    v = \"b\"

thing good2 uses bad:
    v = \"g\"
";
    let index = index_from(schema);
    let file = file_of(src);
    let out = compose_file(&index, "main.nml", &file, &OpenContext);
    let n = out
        .diagnostics
        .iter()
        .filter(|d| d.code == Some(codes::UNRESOLVED_LAYER_REF))
        .count();
    assert_eq!(n, 1, "one defect, one finding: {:?}", out.diagnostics);
}

#[test]
fn type_annotation_modifiers_survive_composition() {
    // An all-annotation group is the field's authored declaration —
    // deleting the entry from the composed body was silent data loss.
    let src = "\
box base:
    |x []string
    label = \"a\"

box t uses base:
    label = \"b\"
";
    let (resolved, _) = compose("", src, "box", "t");
    let body = resolved.unwrap().body;
    assert!(
        body.entries.iter().any(|e| matches!(&e.kind,
                BodyEntryKind::Modifier(m) if m.name.name == "x")),
        "the annotation entry survives: {body:?}"
    );
}

#[test]
fn duplicate_field_names_are_coherent_everywhere() {
    // ONE policy governs a duplicate field name — the FIRST
    // declaration's — in the merge AND the backstop scan. Previously
    // the scan read any-sealed: the engine refused switches to
    // protect a seal it did not itself enforce.
    let schema = "\
model ara:
    kind string
    v string
    v string #sealed

model arb:
    kind string
    other string

oneof cf by kind = \"a\":
    \"a\" -> ara
    \"b\" -> arb

model box3:
    cfg cf
";
    // Open-first duplicate: restating v composes (first-wins, open)…
    let src = "\
box3 base:
    cfg:
        v = \"x\"

box3 t uses base:
    cfg:
        kind = \"b\"
        other = \"y\"
";
    let (resolved, diags) = compose(schema, src, "box3", "t");
    assert!(
        !codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "…so the switch over it must be legal too: {diags:?}"
    );
    assert_eq!(
        nested_scalar(&resolved.unwrap().body, "cfg", "kind"),
        Some(&Value::String("b".into()))
    );
    // Sealed-first duplicate: both sides enforce.
    let schema_sealed_first = schema.replace(
        "    v string\n    v string #sealed",
        "    v string #sealed\n    v string",
    );
    let (_, diags) = compose(&schema_sealed_first, src, "box3", "t");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)
                && d.message.contains("cannot launder")),
        "sealed-first governs in the backstop too: {diags:?}"
    );
}

#[test]
fn duplicate_field_names_keep_plan_and_merge_aligned() {
    // The plan must key each path by the SAME field the merge's
    // first-wins map resolves — a trace folded under the wrong
    // duplicate replays against a body the merge composed under the
    // other, laundering a seal or fabricating a refusal.
    let schema = "\
model subA:
    k string
    v string #sealed

model subB:
    k string
    w string

oneof so by k = \"sa\":
    \"sa\" -> subA

oneof so2 by k = \"sb\":
    \"sb\" -> subB

model app:
    cfg subA
    cfg subB
";
    let src = "\
app base:
    cfg:
        v = \"secret\"

app t uses base:
    cfg:
        k = \"sb\"
";
    let (_, diags) = compose(schema, src, "app", "t");
    // First-wins (subA) governs everywhere: no fabricated refusal on
    // a discriminator subA doesn't have, and subA's seal is judged
    // consistently. The point is coherence, not a specific verdict.
    assert!(
        !diags
            .iter()
            .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION) && d.message.contains("k2")),
        "no fabricated refusal from a desynced plan: {diags:?}"
    );
}

#[test]
fn duplicate_schema_field_names_are_first_wins() {
    // The field maps must match the linear scans they replaced —
    // last-wins would let a duplicate declaration silently swap which
    // `#sealed` governs (fail-open on a broken schema).
    let schema = "\
model dup:
    v string #sealed
    v string
";
    let src = "\
dup base:
    v = \"one\"

dup t uses base:
    v = \"two\"
";
    let (resolved, diags) = compose(schema, src, "dup", "t");
    assert!(
        codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "the FIRST duplicate's #sealed governs: {diags:?}"
    );
    assert_eq!(
        scalar(&resolved.unwrap().body, "v"),
        Some(&Value::String("one".into()))
    );
}
