use super::super::*;
use super::*;

#[test]
fn union_establishment_and_unannotated_merge() {
    // The lowest supplying layer establishes (authored `as`); an
    // un-annotated upper body NEVER switches, whatever its shape —
    // it deep-merges into the effective variant.
    let src = "\
holder base:
    slot as ua:
        x = \"1\"
    label = \"a\"

holder t uses base:
    slot:
        y = \"2\"
";
    let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
    assert!(diags.is_empty(), "clean merge: {diags:?}");
    let body = resolved.unwrap().body;
    assert_eq!(
        slot_annotation(&body).as_deref(),
        Some("ua"),
        "the composed body carries the effective variant explicitly"
    );
    assert_eq!(
        nested_scalar(&body, "slot", "x"),
        Some(&Value::String("1".into())),
        "base survives"
    );
    assert_eq!(
        nested_scalar(&body, "slot", "y"),
        Some(&Value::String("2".into())),
        "the un-annotated upper deep-merges (its mis-typed fields are \
             the validator's business, never a silent switch)"
    );
}

#[test]
fn union_shape_establishment_synthesizes_only_where_d2_would_allow() {
    // The D2 oracle calls an un-annotated keyed body under a
    // ≥2-nameable union AMBIGUOUS — compose must not guess a variant
    // and synthesize an annotation that would silence the
    // validator's fail-closed NML2052. The composed body stays
    // un-annotated, exactly as ambiguous as the authored one.
    let src = "\
holder base:
    slot:
        x = \"1\"

holder t uses base:
    slot:
        x = \"2\"
";
    let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
    assert!(
        diags.is_empty(),
        "compose is silent; D2 is the validator's: {diags:?}"
    );
    let body = resolved.unwrap().body;
    assert_eq!(
        slot_annotation(&body),
        None,
        "no guessed variant, no synthesized annotation — NML2052 \
             fires on the composed view"
    );
    assert_eq!(
        nested_scalar(&body, "slot", "x"),
        Some(&Value::String("2".into())),
        "the ambiguous group still deep-merges model-less"
    );

    // A DISJOINT union (one nameable variant) is never ambiguous:
    // shape establishment synthesizes there, so the merged shape can
    // never re-infer a different variant.
    const DISJOINT: &str = "\
model ua:
    x string

model holder6:
    slot (ua | string)
";
    let src2 = "\
holder6 base:
    slot:
        x = \"1\"

holder6 t uses base:
    slot:
        x = \"2\"
";
    let (resolved, diags) = compose(DISJOINT, src2, "holder6", "t");
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ua"),
        "synthesized annotation pins the unambiguously inferred variant"
    );
}

#[test]
fn union_authored_switch_replaces_wholesale() {
    let src = "\
holder base:
    slot as ub:
        y = \"1\"

holder t uses base:
    slot as ua:
        x = \"2\"
";
    let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
    assert!(diags.is_empty(), "legal switch: {diags:?}");
    let body = resolved.unwrap().body;
    assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
    assert_eq!(
        nested_scalar(&body, "slot", "y"),
        None,
        "wholesale: nothing of the displaced arm survives"
    );
    assert_eq!(
        nested_scalar(&body, "slot", "x"),
        Some(&Value::String("2".into()))
    );
}

#[test]
fn union_switch_discarding_a_seal_is_backstopped() {
    let src = "\
holder base:
    slot as ua:
        secret = \"locked\"

holder t uses base:
    slot as ub:
        y = \"2\"
";
    let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("the union switch is backstopped like a oneof switch");
    assert!(
        d.message.contains("variant switch to `as ub`") && d.message.contains("secret"),
        "names the switch and the seal: {}",
        d.message
    );
    let body = resolved.unwrap().body;
    assert_eq!(
        nested_scalar(&body, "slot", "secret"),
        Some(&Value::String("locked".into())),
        "the sealed variant survives the rejected switch"
    );
    assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
}

// ── arm-set compose (RFC 0007) ───────────────────────────────────────

#[test]
fn union_restated_effective_variant_joins() {
    // Restating the effective variant (authored over authored, or
    // authored over shape-established) is a Join, never a switch.
    let src = "\
holder base:
    slot:
        x = \"1\"

holder t uses base:
    slot as ua:
        x = \"2\"
";
    let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
    assert!(diags.is_empty(), "restatement joins: {diags:?}");
    let body = resolved.unwrap().body;
    assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
    assert_eq!(
        nested_scalar(&body, "slot", "x"),
        Some(&Value::String("2".into()))
    );
}

#[test]
fn union_switch_after_merge_displaces_the_whole_group() {
    // establish → merge → switch: the switch displaces the whole
    // accumulated group, not just the establishing layer.
    let src = "\
holder base:
    slot as ua:
        x = \"1\"

holder mid uses base:
    slot:
        x = \"2\"

holder t uses mid:
    slot as ub:
        y = \"3\"
";
    let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
    assert!(diags.is_empty(), "legal switch: {diags:?}");
    let body = resolved.unwrap().body;
    assert_eq!(slot_annotation(&body).as_deref(), Some("ub"));
    assert_eq!(nested_scalar(&body, "slot", "x"), None, "wholesale");
    assert_eq!(
        nested_scalar(&body, "slot", "y"),
        Some(&Value::String("3".into()))
    );
}

#[test]
fn union_merge_after_rejected_switch_targets_the_original_variant() {
    // A rejected switch contributes NOTHING (wholesale), and a later
    // un-annotated layer merges into the ORIGINAL variant.
    let src = "\
holder base:
    slot as ua:
        secret = \"locked\"

holder mid uses base:
    slot as ub:
        y = \"2\"

holder t uses mid:
    slot:
        x = \"3\"
";
    let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    let body = resolved.unwrap().body;
    assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
    assert_eq!(
        nested_scalar(&body, "slot", "secret"),
        Some(&Value::String("locked".into()))
    );
    assert_eq!(
        nested_scalar(&body, "slot", "x"),
        Some(&Value::String("3".into())),
        "the third layer merges into the surviving variant"
    );
    assert_eq!(
        nested_scalar(&body, "slot", "y"),
        None,
        "the rejected layer's whole body is discarded"
    );
}

#[test]
fn union_switch_is_backstopped_through_a_nested_union() {
    // RFC 0019: \"at any depth, recursively\" — a seal INSIDE a
    // union-typed field of the displaced variant must reject the
    // outer switch (the scan's ModelRef-only child vocabulary was a
    // laundering hole one union deep).
    let src = "\
holder2 base:
    slot as mid:
        inner as leafA:
            s = \"locked\"

holder2 t uses base:
    slot as other:
        z = \"1\"
";
    let (resolved, diags) = compose(NESTED_UNION_SCHEMA, src, "holder2", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("the outer switch is backstopped through the inner union");
    assert!(
        d.message.contains("slot.inner.s") && d.message.contains("unseal the field in the schema"),
        "full path + teaching tail: {}",
        d.message
    );
    let body = resolved.unwrap().body;
    assert_eq!(slot_annotation(&body).as_deref(), Some("mid"));
}

#[test]
fn nested_union_inner_switch_is_backstopped_and_clean_when_unsealed() {
    // The inner union position runs the same authority: a sealed
    // inner switch rejects with the full nested path; an unsealed
    // one switches cleanly.
    let sealed = "\
holder2 base:
    slot as mid:
        inner as leafA:
            s = \"locked\"

holder2 t uses base:
    slot:
        inner as leafB:
            q = \"2\"
";
    let (resolved, diags) = compose(NESTED_UNION_SCHEMA, sealed, "holder2", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("inner switch is backstopped");
    assert!(d.message.contains("slot.inner.s"), "{}", d.message);
    let body = resolved.unwrap().body;
    let inner = sub_block(&body, "slot").and_then(|b| sub_block(b, "inner"));
    assert_eq!(
        inner
            .and_then(|b| b.type_annotation.as_ref())
            .map(|i| &i.name[..]),
        Some("leafA"),
        "the sealed inner variant survives"
    );

    let clean = "\
holder2 base:
    slot as mid:
        inner as leafA:
            p = \"1\"

holder2 t uses base:
    slot:
        inner as leafB:
            q = \"2\"
";
    let (resolved, diags) = compose(NESTED_UNION_SCHEMA, clean, "holder2", "t");
    assert!(
        diags.is_empty(),
        "unsealed inner switch is legal: {diags:?}"
    );
    let body = resolved.unwrap().body;
    let inner = sub_block(&body, "slot").and_then(|b| sub_block(b, "inner"));
    assert_eq!(
        inner
            .and_then(|b| b.type_annotation.as_ref())
            .map(|i| &i.name[..]),
        Some("leafB")
    );
}

#[test]
fn list_of_union_items_are_guarded_by_the_union_authority() {
    // A union list ELEMENT routes each identity-matched item group
    // through the union authority — merging model-less skipped seal
    // enforcement, establishment, and annotation synthesis entirely.
    // (`#identity` on a union list is itself NML2068 at schema load;
    // the engine still guards, defense in depth.)
    let src = "\
holder3 base:
    xs:
        - w as ua:
            x = \"1\"
            secret = \"locked\"

holder3 t uses base:
    xs:
        - w:
            secret = \"stomped\"
";
    let (resolved, diags) = compose(LIST_UNION_SCHEMA, src, "holder3", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("item seals are enforced through the union element");
    assert!(d.message.contains("xs[w].secret"), "{}", d.message);
    let body = resolved.unwrap().body;
    let item_annotation = sub_block(&body, "xs").and_then(|b| {
        b.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::ListItem(ListItem {
                kind: ListItemKind::Named { body, .. },
                ..
            }) => body.type_annotation.as_ref().map(|i| i.name.clone()),
            _ => None,
        })
    });
    assert_eq!(
        item_annotation.as_deref(),
        Some("ua"),
        "the merged item body carries its variant explicitly"
    );
}

#[test]
fn dependent_bogus_as_is_reported_not_swallowed() {
    // A bogus `as` on a dependent layer never switches (fail-safe) —
    // but the composed view replaces the annotation before the
    // validator sees it, so the MERGE must report NML2051 or the
    // typo composes silently into the wrong variant.
    let src = "\
holder base:
    slot as ua:
        x = \"1\"

holder t uses base:
    slot as zz:
        y = \"2\"
";
    let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::UNKNOWN_UNION_VARIANT))
        .expect("the swallowed annotation is reported");
    assert!(d.message.contains("`zz` is not a variant"), "{}", d.message);
    let body = resolved.unwrap().body;
    assert_eq!(
        slot_annotation(&body).as_deref(),
        Some("ua"),
        "the bogus name joined, never switched"
    );
}

#[test]
fn structural_value_over_named_establishment_is_discarded_loudly() {
    // RFC 0015: scalar variants are structurally unambiguous and not
    // nameable — a whole-value spelling can neither merge into a
    // named variant nor switch it. Silence here was data loss; the
    // discard is NML2085 and the established value survives.
    let src = "\
holder4 base:
    slot as ua:
        x = \"1\"

holder4 t uses base:
    slot = \"replacement\"
";
    let (resolved, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::DISCARDED_UNION_CONTRIBUTION))
        .expect("the dropped scalar is loud");
    assert!(d.message.contains("established `as ua`"), "{}", d.message);
    let body = resolved.unwrap().body;
    assert_eq!(
        nested_scalar(&body, "slot", "x"),
        Some(&Value::String("1".into()))
    );
}

#[test]
fn unannotated_body_over_structural_establishment_is_discarded_loudly() {
    // The reverse hijack: the lowest supplying layer established the
    // STRUCTURAL variant; an un-annotated upper body never switches
    // (its shape notwithstanding) — discarding it silently while its
    // shape \"won\" the position violated both halves of the rule.
    let src = "\
holder4 base:
    slot = \"the-base-value\"

holder4 t uses base:
    slot:
        x = \"1\"
";
    let (resolved, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::DISCARDED_UNION_CONTRIBUTION))
        .expect("the shape hijack is loud");
    assert!(
        d.message.contains("author `as ua` to switch"),
        "{}",
        d.message
    );
    let body = resolved.unwrap().body;
    assert_eq!(
        scalar(&body, "slot"),
        Some(&Value::String("the-base-value".into())),
        "the structural establishment survives"
    );
}

#[test]
fn authored_as_switches_away_from_a_structural_value() {
    // An authored `as` IS the switch spelling — from a structural
    // establishment too (a displaced scalar carries no seals, so the
    // backstop always admits it).
    let src = "\
holder4 base:
    slot = \"money\"

holder4 t uses base:
    slot as ua:
        x = \"1\"
";
    let (resolved, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "t");
    assert!(diags.is_empty(), "authored switch is legal: {diags:?}");
    let body = resolved.unwrap().body;
    assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
    assert_eq!(
        nested_scalar(&body, "slot", "x"),
        Some(&Value::String("1".into()))
    );
}

#[test]
fn optional_union_field_composes_with_the_backstop() {
    // Optionality is a FieldDef flag, not a type wrapper — `?` must
    // not cost a union position its compose authority.
    const OPT: &str = "\
model ua:
    secret string #sealed

model ub:
    y string

model holder5:
    slot (ua | ub)?
";
    let src = "\
holder5 base:
    slot as ua:
        secret = \"locked\"

holder5 t uses base:
    slot as ub:
        y = \"2\"
";
    let (resolved, diags) = compose(OPT, src, "holder5", "t");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    let body = resolved.unwrap().body;
    assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
}

// ── union compose: round-13 battery (list-variant establishments,
//    per-shape structural buckets, ambiguity discipline, depth) ────

#[test]
fn union_switch_off_a_list_variant_establishment_is_backstopped() {
    // \"A displaced structural group has no seals\" is true only for
    // scalars: block-form list items ARE bodies, and a switch away
    // from a list-variant establishment is judged over them under
    // the list variants' element models — assuming otherwise was a
    // laundering hole.
    let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"locked\"

holder7 t uses base:
    slot as ua:
        x = \"2\"
";
    let (resolved, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("the switch off the list establishment is backstopped");
    assert!(
        d.message.contains("slot[w].secret"),
        "item-prefixed seal path: {}",
        d.message
    );
    let body = resolved.unwrap().body;
    assert_eq!(
        slot_annotation(&body),
        None,
        "the structural list establishment survives, un-annotated"
    );
}

#[test]
fn unsealed_list_variant_establishment_switches_cleanly() {
    // The same shape without an assigned seal admits the switch —
    // the backstop rejects laundering, not switching.
    let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"

holder7 t uses base:
    slot as ua:
        x = \"2\"
";
    let (resolved, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "t");
    assert!(diags.is_empty(), "unsealed switch is legal: {diags:?}");
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ua")
    );
}

#[test]
fn union_switch_is_backstopped_through_a_union_list_element() {
    // \"At any depth\" binds union-typed LIST ELEMENTS of a displaced
    // variant too — the scan's ModelRef-only element read was a
    // laundering hole through `[](a|b)` items.
    let src = "\
holder8 base:
    slot as bigv:
        ys:
            - w as leafA:
                s = \"locked\"

holder8 t uses base:
    slot as other:
        z = \"1\"
";
    let (resolved, diags) = compose(UNION_ELEMENT_SCHEMA, src, "holder8", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("the switch is backstopped through the union element");
    assert!(
        d.message.contains("slot.ys[w].s"),
        "full element path: {}",
        d.message
    );
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("bigv")
    );
}

#[test]
fn structural_cross_shape_supplies_are_discarded_loudly() {
    // Scalar↔list inside the structural bucket is a variant change
    // with no `as` spelling to authorize it — one collapsed bucket
    // let the winner flip with the base's SPELLING and discarded a
    // later value silently.
    const S: &str = "\
model ua:
    x string

model holder9:
    slot (ua | string | []string)
";
    let scalar_over_list = "\
holder9 base:
    slot:
        - \"a\"

holder9 t uses base:
    slot = \"s\"
";
    let (resolved, diags) = compose(S, scalar_over_list, "holder9", "t");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    assert!(
        diags[0].message.contains("established as a list value"),
        "{}",
        diags[0].message
    );
    assert!(
        diags[0]
            .related
            .iter()
            .any(|r| r.message == "in force here"),
        "points at the list in force"
    );
    assert!(resolved.is_some(), "the established list survives");

    let list_over_scalar = "\
holder9 base:
    slot = \"s\"

holder9 t uses base:
    slot = [\"a\", \"b\"]
";
    let (resolved, diags) = compose(S, list_over_scalar, "holder9", "t");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    assert!(
        diags[0].message.contains("established as a scalar value"),
        "{}",
        diags[0].message
    );
    let body = resolved.unwrap().body;
    assert_eq!(
        scalar(&body, "slot"),
        Some(&Value::String("s".into())),
        "layer order wins, not spelling"
    );
}

#[test]
fn structural_supply_after_a_rejected_switch_is_discarded_against_the_original() {
    // Two backstops in one fold: the rejected switch (NML2060) does
    // not disturb the establishment, and a later structural supply
    // is judged against the ORIGINAL variant (NML2085).
    const S: &str = "\
model ua:
    x string
    secret string #sealed

model ub:
    y string

model holder10:
    slot (ua | ub | string)
";
    let src = "\
holder10 base:
    slot as ua:
        secret = \"locked\"

holder10 mid uses base:
    slot as ub:
        y = \"2\"

holder10 t uses mid:
    slot = \"cash\"
";
    let (resolved, diags) = compose(S, src, "holder10", "t");
    assert_eq!(
        codes_of(&diags),
        [
            codes::SEALED_FIELD_VIOLATION,
            codes::DISCARDED_UNION_CONTRIBUTION
        ]
    );
    assert!(diags[1].message.contains("established `as ua`"));
    let body = resolved.unwrap().body;
    assert_eq!(
        nested_scalar(&body, "slot", "secret"),
        Some(&Value::String("locked".into()))
    );
}

#[test]
fn structural_supply_after_an_authored_switch_from_structural_is_discarded() {
    // structural → authored switch → structural: the trailing value
    // is judged against the NEW named establishment, loudly.
    let src = "\
holder4 base:
    slot = \"money\"

holder4 mid uses base:
    slot as ua:
        x = \"2\"

holder4 t uses mid:
    slot = \"cash\"
";
    let (resolved, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "t");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    assert!(diags[0].message.contains("established `as ua`"));
    assert_eq!(
        nested_scalar(&resolved.unwrap().body, "slot", "x"),
        Some(&Value::String("2".into()))
    );
}

#[test]
fn structural_union_restatement_is_a_dead_delta() {
    // The structural_overlay scalar route carries the same NML2084
    // dead-delta contract as the plain scalar overlay — and NML2085
    // and NML2084 coexist in one position.
    const S: &str = "\
model ua:
    x string

model holder11:
    slot (ua | string)
";
    let src = "\
holder11 base:
    slot = \"v\"

holder11 mid uses base:
    slot:
        x = \"1\"

holder11 t uses mid:
    slot = \"v\"
";
    let (resolved, diags) = compose(S, src, "holder11", "t");
    assert_eq!(
        codes_of(&diags),
        [codes::DISCARDED_UNION_CONTRIBUTION, codes::DEAD_DELTA]
    );
    assert_eq!(
        scalar(&resolved.unwrap().body, "slot"),
        Some(&Value::String("v".into()))
    );
}

#[test]
fn item_scope_union_faces_fire_with_item_paths() {
    // The item-scope face: a rejected switch inside one identity
    // item names the ITEM path; a bogus `as` inside an item is
    // reported, never swallowed; a legal item switch after a merge
    // displaces the whole item group.
    let rejected = "\
holder3 base:
    xs:
        - w as ua:
            secret = \"locked\"

holder3 t uses base:
    xs:
        - w as ub:
            y = \"2\"
";
    let (_, diags) = compose(LIST_UNION_SCHEMA, rejected, "holder3", "t");
    let d = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("item switch is backstopped");
    assert!(
        d.message.contains("'xs[w]'") && d.message.contains("xs[w].secret"),
        "item path spelling: {}",
        d.message
    );

    let bogus = "\
holder3 base:
    xs:
        - w as ua:
            x = \"1\"

holder3 t uses base:
    xs:
        - w as zz:
            y = \"2\"
";
    let (_, diags) = compose(LIST_UNION_SCHEMA, bogus, "holder3", "t");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::UNKNOWN_UNION_VARIANT)),
        "item bogus `as` is reported: {diags:?}"
    );

    let switch = "\
holder3 base:
    xs:
        - w as ua:
            x = \"1\"

holder3 mid uses base:
    xs:
        - w:
            x = \"2\"

holder3 t uses mid:
    xs:
        - w as ub:
            y = \"3\"
";
    let (resolved, diags) = compose(LIST_UNION_SCHEMA, switch, "holder3", "t");
    assert!(diags.is_empty(), "legal item switch: {diags:?}");
    let body = resolved.unwrap().body;
    let item_body = sub_block(&body, "xs").and_then(|b| {
        b.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::ListItem(ListItem {
                kind: ListItemKind::Named { body, .. },
                ..
            }) => Some(body),
            _ => None,
        })
    });
    let item_body = item_body.expect("merged item survives");
    assert_eq!(
        item_body.type_annotation.as_ref().map(|i| &i.name[..]),
        Some("ub")
    );
    assert!(
        !item_body.entries.iter().any(|e| matches!(&e.kind,
                BodyEntryKind::Property(p) if p.name.name == "x")),
        "wholesale item switch"
    );
}

#[test]
fn discarded_contributions_report_once_across_dependent_composes() {
    // The one-home dedup binds the new codes too: a transitive
    // dependent's re-compose must not re-report t's discard.
    let schema = "\
model ua:
    x string

model holder12:
    slot (ua | string)
";
    let src = "\
holder12 base:
    slot as ua:
        x = \"1\"

holder12 t uses base:
    slot = \"cash\"

holder12 t2 uses t:
    slot as ua:
        x = \"3\"
";
    let index = index_from(schema);
    let file = file_of(src);
    let out = compose_file(&index, "main.nml", &file, &OpenContext);
    let n = out
        .diagnostics
        .iter()
        .filter(|d| d.code == Some(codes::DISCARDED_UNION_CONTRIBUTION))
        .count();
    assert_eq!(n, 1, "one discard, one finding: {:?}", out.diagnostics);
}

#[test]
fn ambiguous_group_is_pinned_by_an_authored_as() {
    // An `as` above an ambiguous group RESOLVES it — nothing was
    // chosen to switch from, so nothing is discarded: the group
    // deep-merges under the named variant and the output carries it.
    let src = "\
holder base:
    slot:
        x = \"1\"

holder t uses base:
    slot as ua:
        x = \"2\"
";
    let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
    assert!(diags.is_empty(), "a pin is not a switch: {diags:?}");
    let body = resolved.unwrap().body;
    assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
    assert_eq!(
        nested_scalar(&body, "slot", "x"),
        Some(&Value::String("2".into())),
        "the ambiguous base joined the pinned group"
    );
}

#[test]
fn ambiguous_interiors_are_scanned_under_every_oracle_candidate() {
    // A oneof arm switch displacing an arm whose union field holds an
    // AMBIGUOUS body: the seal lives in the SECOND candidate. The scan
    // must judge under every oracle candidate — the resolver's
    // first-wins pick made the verdict depend on variant source
    // order (both orders pinned).
    for (order, schema_variants) in [
        ("a-first", "(leafA | leafB)"),
        ("b-first", "(leafB | leafA)"),
    ] {
        let schema = format!(
            "\
model leafA:
    p string
    q string

model leafB:
    p string
    s string #sealed

model armX:
    kind string
    inner {schema_variants}

model armY:
    kind string
    w string

oneof pay4 by kind:
    \"x\" -> armX
    \"y\" -> armY
"
        );
        let src = "\
pay4 base:
    kind = \"x\"
    inner:
        s = \"locked\"

pay4 top uses base:
    kind = \"y\"
    w = \"1\"
";
        let (_, diags) = compose(&schema, src, "pay4", "top");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .unwrap_or_else(|| panic!("{order}: ambiguous interior seal must reject: {diags:?}"));
        assert!(d.message.contains("inner.s"), "{order}: {}", d.message);
    }
}

#[test]
fn union_switch_off_a_list_variant_judges_shared_and_positional_writes() {
    // The displaced LIST is judged as a list: a list-level `.shared`
    // sealed write and a bodiless item's positional `+` token are
    // both assigned seals — scanning item bodies in isolation saw
    // neither.
    let shared = "\
holder13 base:
    slot:
        .secret = \"locked\"
        - w:
            note = \"n\"

holder13 t uses base:
    slot as ua:
        x = \"2\"
";
    let (_, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, shared, "holder13", "t");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    assert!(
        diags[0].message.contains("slot[w].secret"),
        "{}",
        diags[0].message
    );

    let positional = "\
holder13 base:
    slot:
        - \"w\"

holder13 t uses base:
    slot as ua:
        x = \"2\"
";
    let (_, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, positional, "holder13", "t");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    assert!(
        diags[0].message.contains("slot[string].name"),
        "{}",
        diags[0].message
    );
}

#[test]
fn list_variant_with_a_oneof_element_is_backstopped() {
    // `(ua | []uo)` — a oneof ELEMENT: each displaced item is judged
    // under the arm its own discriminator selects (NML2076 promises
    // exactly this backstop for the shape).
    const S: &str = "\
model ua:
    x string

model arma:
    kind string
    secret string #sealed

model armb:
    kind string
    q string

oneof uo by kind:
    \"a\" -> arma
    \"b\" -> armb

model holder14:
    slot (ua | []uo)
";
    let src = "\
holder14 base:
    slot:
        - w:
            kind = \"a\"
            secret = \"s\"

holder14 t uses base:
    slot as ua:
        x = \"1\"
";
    let (_, diags) = compose(S, src, "holder14", "t");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    assert!(
        diags[0].message.contains("slot[w].secret"),
        "{}",
        diags[0].message
    );
}

#[test]
fn modifier_spelled_union_positions_take_the_union_route() {
    // Every spelling reaches the union authority: a modifier-spelled
    // item block is a scannable list body, and an all-modifier
    // scalar↔list cross is as loud as the property spelling.
    let launder = "\
holder13 base:
    |slot:
        - w:
            secret = \"s\"

holder13 t uses base:
    slot as ua:
        x = \"1\"
";
    let (_, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, launder, "holder13", "t");
    assert_eq!(
        codes_of(&diags),
        [codes::SEALED_FIELD_VIOLATION],
        "modifier-spelled items are judged, and nothing else fires"
    );

    const S: &str = "\
model holder15:
    slot ([]string | string)
";
    let cross = "\
holder15 base:
    |slot = [\"a\"]

holder15 t uses base:
    |slot = \"v\"
";
    let (_, diags) = compose(S, cross, "holder15", "t");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
}

#[test]
fn zero_item_entries_at_union_positions_are_warned_and_never_establish() {
    // NML2079's contract holds at union positions: `= []` and an
    // empty block warn and are no-ops — as the lowest supply they
    // establish nothing (a valid upper is not a false NML2085), over
    // a list they are inert.
    let lowest = "\
holder13 base:
    slot = []

holder13 t uses base:
    slot:
        x = \"1\"
";
    let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, lowest, "holder13", "t");
    assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ua"),
        "the first REAL supply establishes"
    );

    let over_items = "\
holder13 base:
    slot:
        - w:
            note = \"n\"

holder13 t uses base:
    slot:
";
    let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, over_items, "holder13", "t");
    assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
    let body = resolved.unwrap().body;
    let items = sub_block(&body, "slot")
        .map(|b| {
            b.entries
                .iter()
                .filter(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(items, 1, "the base list survives the zero-item no-op");
}

#[test]
fn discard_faces_follow_the_recorded_context() {
    // The face is keyed on the (establishment, supply) pair the FOLD
    // recorded: a list over a scalar is the cross-shape face; a
    // scalar over an ambiguous body names the candidates; a discard
    // followed by a switch is reported once, against the
    // establishment in force at the time.
    const S: &str = "\
model card:
    last4 string

model debit:
    last4 string
    note string

model wallet2:
    payment (card | debit | string | []string)
";
    let cross = "\
wallet2 base:
    payment = \"cash\"

wallet2 t uses base:
    payment:
        - \"a\"
";
    let (_, diags) = compose(S, cross, "wallet2", "t");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    assert!(
        diags[0]
            .message
            .contains("a list value cannot merge into it"),
        "cross-shape face: {}",
        diags[0].message
    );

    let over_ambiguous = "\
wallet2 base:
    payment:
        last4 = \"1\"

wallet2 t uses base:
    payment = \"cash\"
";
    let (_, diags) = compose(S, over_ambiguous, "wallet2", "t");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    assert!(
        diags[0]
            .message
            .contains("un-annotated body (ambiguous between card | debit)"),
        "ambiguous establishment names its candidates: {}",
        diags[0].message
    );

    let discard_then_switch = "\
wallet2 base:
    payment = \"cash\"

wallet2 mid uses base:
    payment:
        last4 = \"1\"

wallet2 t uses mid:
    payment as card:
        last4 = \"2\"
";
    let (resolved, diags) = compose(S, discard_then_switch, "wallet2", "t");
    assert_eq!(
        codes_of(&diags),
        [codes::DISCARDED_UNION_CONTRIBUTION],
        "one discard, one finding, judged against the scalar establishment"
    );
    assert!(
        diags[0].message.contains("established as a scalar value"),
        "{}",
        diags[0].message
    );
    assert_eq!(
        slot_annotation_named(&resolved.unwrap().body, "payment").as_deref(),
        Some("card")
    );
}

#[test]
fn dependent_as_naming_a_list_element_gets_the_honest_form() {
    // `as ub` where `ub` is only a list variant's ELEMENT: not a
    // nameable variant, and "did you mean ua" would mislead.
    let src = "\
holder13 base:
    slot as ua:
        x = \"1\"

holder13 t uses base:
    slot as ub:
        x = \"2\"
";
    let (_, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, src, "holder13", "t");
    assert_eq!(
        codes_of(&diags),
        [codes::UNKNOWN_UNION_VARIANT],
        "one defect, one finding (no NML2085 riding along): {diags:?}"
    );
    assert!(
        diags[0].message.contains("names a list variant's element")
            && diags[0].suggestions.is_empty(),
        "honest form, no did-you-mean: {}",
        diags[0].message
    );
}

#[test]
fn all_structural_union_groups_take_the_union_route() {
    // All-scalar and all-array groups compose by the same rules as
    // before the routing widen — scalar overlay with its dead delta,
    // the bare-list winner — now through one owner each.
    const S: &str = "\
model holder16:
    a (string | number)
    xs ([]string | string)
";
    let src = "\
holder16 base:
    a = \"v\"
    xs = [\"a\"]

holder16 t uses base:
    a = \"v\"
    xs = [\"b\", \"c\"]
";
    let (resolved, diags) = compose(S, src, "holder16", "t");
    assert_eq!(codes_of(&diags), [codes::DEAD_DELTA]);
    let body = resolved.unwrap().body;
    assert_eq!(scalar(&body, "a"), Some(&Value::String("v".into())));
    let xs_items = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::Property(p) if p.name.name == "xs" => match &p.value.value {
                Value::Array(v) => Some(v.len()),
                _ => None,
            },
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "xs" => Some(
                nb.body
                    .entries
                    .iter()
                    .filter(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
                    .count(),
            ),
            _ => None,
        })
        .unwrap_or(0);
    assert_eq!(xs_items, 2, "the higher item supplier wins");
}

#[test]
fn discard_notes_point_at_the_establishing_entry_for_every_face() {
    let src = "\
holder4 base:
    slot as ua:
        x = \"1\"

holder4 t uses base:
    slot = \"cash\"
";
    let (_, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "t");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    assert!(
        diags[0]
            .related
            .iter()
            .any(|r| r.message == "established here"),
        "the named face carries the note too"
    );
    for src in [
        // ambiguous establishment, scalar supply
        "holder base:\n    slot:\n        x = \"1\"\n\nholder t uses base:\n    slot = \"v\"\n",
        // list establishment, scalar supply
        "holder13 base:\n    slot:\n        - w:\n            note = \"n\"\n\nholder13 t uses base:\n    slot = \"v\"\n",
    ] {
        let (schema, kw, name) = if src.starts_with("holder13") {
            (LIST_VARIANT_SHARED_SCHEMA, "holder13", "t")
        } else {
            (UNION_SCHEMA, "holder", "t")
        };
        let (_, diags) = compose(schema, src, kw, name);
        assert_eq!(
            codes_of(&diags),
            [codes::DISCARDED_UNION_CONTRIBUTION],
            "{src}"
        );
        let expected = if src.starts_with("holder13") {
            "in force here"
        } else {
            "established here"
        };
        assert!(
            diags[0].related.iter().any(|r| r.message == expected),
            "every face carries its note: {src}"
        );
    }
}

#[test]
fn nested_union_discard_agrees_between_planned_and_refolded_routes() {
    // Discarded rides the PLAN at an all-nested inner position; a
    // whole-value sibling breaks alignment and forces the local
    // refold — both routes must say the same thing.
    const S: &str = "\
model leafA:
    s string #sealed

model outer:
    inner (leafA | []string)

model holder17:
    slot (outer | string)
";
    let planned = "\
holder17 base:
    slot as outer:
        inner as leafA:
            s = \"locked\"

holder17 t uses base:
    slot:
        inner:
            - \"v\"
";
    let (resolved, planned_diags) = compose(S, planned, "holder17", "t");
    assert_eq!(
        codes_of(&planned_diags),
        [codes::DISCARDED_UNION_CONTRIBUTION]
    );
    let inner_s = sub_block(&resolved.unwrap().body, "slot")
        .and_then(|b| sub_block(b, "inner"))
        .and_then(|b| scalar(b, "s").cloned());
    assert_eq!(inner_s, Some(Value::String("locked".into())));

    let refolded = "\
holder17 base:
    slot as outer:
        inner as leafA:
            s = \"locked\"

holder17 mid uses base:
    slot:
        inner:
            - \"v\"

holder17 t uses mid:
    slot:
        inner = \"w\"
";
    let (_, refold_diags) = compose(S, refolded, "holder17", "t");
    assert_eq!(
        codes_of(&refold_diags),
        [
            codes::DISCARDED_UNION_CONTRIBUTION,
            codes::DISCARDED_UNION_CONTRIBUTION
        ]
    );
    assert_eq!(
        planned_diags[0].message, refold_diags[0].message,
        "planned and refolded routes agree"
    );
}

// ── union compose: round-15 battery (the rule table itself, every
//    spelling through the authority, list judgment as the bare-list
//    winner, plan/merge supply parity, loud fail-safes) ──────────────

#[test]
fn union_verdict_table_enumerates_every_cell() {
    // The rule table, cell by cell — RFC 0019's rules (and the
    // documented errata) as one readable matrix. A regression in
    // any cell shows up as a named (row, column).
    fn body() -> Cow<'static, Body> {
        Cow::Owned(Body::fresh(Vec::new()))
    }
    let named = || Establishment::Named {
        variant: "ua".into(),
        synthesized: false,
    };
    let ambiguous = || Establishment::Ambiguous {
        candidates: vec!["ua".into(), "ub".into()],
    };
    let rows: [(&str, Option<Establishment>); 5] = [
        ("none", None),
        ("named ua", Some(named())),
        ("ambiguous", Some(ambiguous())),
        ("value", Some(Establishment::Value)),
        ("items", Some(Establishment::Items)),
    ];
    type Make = fn() -> UnionSupply<'static>;
    let supplies: [(&str, Make); 7] = [
        ("authored ua", || UnionSupply::Authored {
            variant: "ua".into(),
            body: body(),
        }),
        ("authored ub", || UnionSupply::Authored {
            variant: "ub".into(),
            body: body(),
        }),
        ("inferred", || UnionSupply::Inferred {
            variant: "ua".into(),
            body: body(),
        }),
        ("ambiguous", || UnionSupply::Ambiguous {
            candidates: vec!["ua".into(), "ub".into()],
            body: body(),
        }),
        ("items", || UnionSupply::Items { body: body() }),
        ("empty", || UnionSupply::Empty),
        ("value", || UnionSupply::Value),
    ];
    // Columns: authored-same, authored-other, inferred, ambiguous,
    // items, empty, value.
    use Verdict as V;
    let expected: [(&str, [V; 7]); 5] = [
        // RFC 0019 "the lowest supplying layer establishes"; a
        // zero-item entry never supplies (NML2079's contract, E7).
        (
            "none",
            [
                V::Establish,
                V::Establish,
                V::Establish,
                V::Establish,
                V::Establish,
                V::Join,
                V::Establish,
            ],
        ),
        // RFC 0019: restatement joins, a different `as` switches
        // (judged), an un-annotated body never switches; a whole
        // value cannot merge into a named variant (E2).
        (
            "named ua",
            [
                V::Join,
                V::JudgeSwitch("ub".into()),
                V::Join,
                V::Join,
                V::Discard,
                V::Join,
                V::Discard,
            ],
        ),
        // E5/E6: an `as` above an ambiguous group pins it (nothing
        // was chosen to switch from); bodies join; whole values
        // cannot merge into a body (E2).
        (
            "ambiguous",
            [
                V::Pin("ua".into()),
                V::Pin("ub".into()),
                V::Join,
                V::Join,
                V::Discard,
                V::Join,
                V::Discard,
            ],
        ),
        // E4: per-shape structural establishments — an authored `as`
        // switches (a displaced scalar has no seals; a list is
        // judged, E9); bodies cannot merge (E3); the same shape joins
        // its overlay; a cross shape is discarded.
        (
            "value",
            [
                V::JudgeSwitch("ua".into()),
                V::JudgeSwitch("ub".into()),
                V::Discard,
                V::Discard,
                V::Discard,
                V::Join,
                V::Join,
            ],
        ),
        (
            "items",
            [
                V::JudgeSwitch("ua".into()),
                V::JudgeSwitch("ub".into()),
                V::Discard,
                V::Discard,
                V::Join,
                V::Join,
                V::Discard,
            ],
        ),
    ];
    for ((row, est), (_, want)) in rows.iter().zip(expected.iter()) {
        for ((col, make), w) in supplies.iter().zip(want.iter()) {
            assert_eq!(
                &union_verdict(est.as_ref(), &make()),
                w,
                "cell ({row}, {col})"
            );
        }
    }
}

#[test]
fn type_annotation_modifiers_never_bypass_the_union_authority() {
    // A `|slot (ua | ub)` declaration inside an instance body is a
    // declaration, not a value: it must neither route the group
    // around the authority (a debug panic; in release a last-wins
    // that laundered seals or deleted the value) nor count as a
    // sealing write.
    const S: &str = "\
model ua:
    x string #sealed

model ub:
    y string

model holder18:
    slot (ua | ub)
";
    let launder = "\
holder18 base:
    slot as ua:
        x = \"1\"

holder18 top uses base:
    |slot (ua | ub)
    slot as ub:
        y = \"2\"
";
    let (resolved, diags) = compose(S, launder, "holder18", "top");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ua"),
        "the sealed base survives the annotated switch"
    );

    let alone = "\
holder18 base:
    slot as ua:
        x = \"1\"

holder18 top uses base:
    |slot (ua | ub)
";
    let (resolved, diags) = compose(S, alone, "holder18", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    assert_eq!(
        nested_scalar(&body, "slot", "x"),
        Some(&Value::String("1".into())),
        "an annotation-only upper never deletes the established value"
    );

    const SEALED: &str = "\
model ua:
    x string

model ub:
    y string

model holder19:
    slot (ua | ub) #sealed
";
    let sealed = "\
holder19 base:
    |slot (ua | ub)
    slot as ua:
        x = \"1\"

holder19 top uses base:
    slot as ub:
        y = \"2\"
";
    let (resolved, diags) = compose(SEALED, sealed, "holder19", "top");
    assert_eq!(
        codes_of(&diags),
        [codes::SEALED_FIELD_VIOLATION],
        "the real assignment seals; the declaration never does"
    );
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ua")
    );
}

#[test]
fn plan_and_merge_fold_the_same_supply_set() {
    // A whole-value sibling used to break plan alignment, and the
    // local refold then judged bodies already normalized under the
    // FINAL planned variant — a fabricated refusal (itb's `key`
    // token injected into the base, then scanned as ua's sealed
    // `key`). Same supply set on both sides: the trace aligns.
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

model holder20:
    slot (ua | ub | string)
";
    let src = "\
holder20 base:
    slot as ua:
        items:
            - \"w\"

holder20 mid uses base:
    slot = \"x\"

holder20 top uses mid:
    slot as ub:
        items:
            - \"k\"
";
    let (resolved, diags) = compose(S, src, "holder20", "top");
    assert_eq!(
        codes_of(&diags),
        [codes::DISCARDED_UNION_CONTRIBUTION],
        "mid's scalar is discarded; the switch is clean (no seal was assigned): {diags:?}"
    );
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ub")
    );
}

#[test]
fn shared_only_union_blocks_survive_authored_empty() {
    // A `.shared`-only block owns no entries: a zero-item entry raw
    // AND normalized (the plan and the merge agree), and an
    // all-zero-item position survives in the `= []` spelling rather
    // than dropping (a phantom "missing required field 'slot'").
    let src = "\
holder13 base:
    slot:
        .note = \"n\"

holder13 t uses base:
    slot:
        .note = \"m\"
";
    let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, src, "holder13", "t");
    assert_eq!(
        codes_of(&diags),
        [codes::ZERO_ITEM_LAYER_ENTRY, codes::ZERO_ITEM_LAYER_ENTRY]
    );
    let body = resolved.unwrap().body;
    let respelled = body.entries.iter().any(|e| {
            matches!(&e.kind, BodyEntryKind::Property(p)
                if p.name.name == "slot" && matches!(&p.value.value, Value::Array(v) if v.is_empty()))
        });
    assert!(respelled, "survives as `slot = []`: {body:?}");
}

#[test]
fn replaced_lists_are_not_judged_on_a_later_switch() {
    // The displaced compose of a list establishment is the bare-list
    // WINNER; a lower list the engine itself replaced wholesale
    // (its seals never engaged, as NML2076 warns) must not refuse a
    // later switch.
    let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"locked\"

holder7 mid uses base:
    slot:
        - v:
            kind = \"k\"

holder7 top uses mid:
    slot as ua:
        x = \"1\"
";
    let (resolved, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "top");
    assert!(diags.is_empty(), "no phantom refusal: {diags:?}");
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ua")
    );
}

#[test]
fn zero_item_entry_never_seals_a_sealed_union_position() {
    const S: &str = "\
model ua:
    x string

model ub:
    kind string

model holder21:
    slot (ua | []ub) #sealed
";
    let src = "\
holder21 base:
    slot = []

holder21 t uses base:
    slot as ua:
        x = \"1\"
";
    let (resolved, diags) = compose(S, src, "holder21", "t");
    assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ua"),
        "the first REAL assignment seals"
    );
}

#[test]
fn empty_array_under_a_listless_union_is_a_loud_whole_value() {
    // `= []` under a union with no list variant is an (invalid)
    // whole value — never a phantom empty object that classifies as
    // a body and swallows silently.
    const S: &str = "\
model ua:
    x string #sealed

model uc:
    z string

model holder22:
    slot (ua | uc)
";
    let src = "\
holder22 base:
    slot as ua:
        x = \"s\"

holder22 t uses base:
    slot = []
";
    let (resolved, diags) = compose(S, src, "holder22", "t");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    assert_eq!(
        nested_scalar(&resolved.unwrap().body, "slot", "x"),
        Some(&Value::String("s".into()))
    );
}

#[test]
fn pin_carries_the_authored_identifier() {
    // The pinning layer's `as` is authored: the composed annotation
    // is that identifier (span inside the pinning layer), not one
    // synthesized at the ambiguous base.
    let src = "\
holder base:
    slot:
        x = \"1\"

holder t uses base:
    slot as ua:
        x = \"2\"
";
    let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    let ann = sub_block(&body, "slot")
        .and_then(|b| b.type_annotation.clone())
        .expect("annotated");
    assert_eq!(ann.name, "ua");
    let tok = src.rfind("as ua").unwrap() + 3;
    assert_eq!(
        (ann.span.start, ann.span.end),
        (tok, tok + 2),
        "the annotation is the pinning layer's own `ua` token"
    );
}

#[test]
fn pin_then_switch_is_judged_under_the_pinned_vocabulary() {
    // Once pinned, the group IS the pinned variant: a later
    // different `as` is judged over it under that vocabulary only
    // (a write meaningful in an un-pinned candidate is not a seal
    // there). Both orders pinned.
    const S: &str = "\
model ua:
    x string
    s string

model uc:
    x string
    s string #sealed

model holder23:
    slot (ua | uc)
";
    let pin_ua = "\
holder23 base:
    slot:
        s = \"locked\"

holder23 mid uses base:
    slot as ua:
        x = \"1\"

holder23 top uses mid:
    slot as uc:
        x = \"2\"
";
    let (resolved, diags) = compose(S, pin_ua, "holder23", "top");
    assert!(diags.is_empty(), "under ua, `s` is unsealed: {diags:?}");
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("uc")
    );

    let pin_uc = "\
holder23 base:
    slot:
        s = \"locked\"

holder23 mid uses base:
    slot as uc:
        x = \"1\"

holder23 top uses mid:
    slot as ua:
        x = \"2\"
";
    let (resolved, diags) = compose(S, pin_uc, "holder23", "top");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("uc")
    );
}

#[test]
fn same_layer_discard_names_the_earlier_entry() {
    let src = "\
holder4 base:
    slot as ua:
        x = \"1\"
    slot = \"cash\"
";
    let (_, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "base");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    assert!(
        diags[0]
            .message
            .contains("by an earlier entry in this same layer"),
        "{}",
        diags[0].message
    );
}

#[test]
fn ambiguous_establishment_discard_advises_resolving_the_lower_body() {
    let src = "\
holder base:
    slot:
        x = \"1\"

holder t uses base:
    slot = \"cash\"
";
    let (_, diags) = compose(UNION_SCHEMA, src, "holder", "t");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    assert!(
        diags[0]
            .message
            .contains("resolve the establishing body with `as <ua | ub>`"),
        "{}",
        diags[0].message
    );
}

// ── union compose: round-16 battery (sealed union bodies are writes,
//    declarations pass through everywhere, plan authority, the sink) ──

#[test]
fn keyed_bodies_at_a_sealed_item_admitting_union_are_writes() {
    // Regression: `admits_items` made every keyed body at
    // `(ua | []ub) #sealed` a "zero-item" non-write — an upper layer
    // replaced the sealed value silently, and both backstops missed
    // it. Field face (annotated and inferred) and backstop face.
    const S: &str = "\
model ua:
    x string

model ub:
    kind string

model holder24:
    slot (ua | []ub) #sealed
";
    for src in [
        "holder24 base:\n    slot as ua:\n        x = \"1\"\n\nholder24 top uses base:\n    slot as ua:\n        x = \"2\"\n",
        "holder24 base:\n    slot:\n        x = \"1\"\n\nholder24 top uses base:\n    slot:\n        x = \"2\"\n",
        "holder24 base:\n    slot as ua:\n        x = \"1\"\n\nholder24 top uses base:\n    slot = \"gone\"\n",
    ] {
        let (resolved, diags) = compose(S, src, "holder24", "top");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION], "{src}");
        assert_eq!(
            nested_scalar(&resolved.unwrap().body, "slot", "x"),
            Some(&Value::String("1".into())),
            "the sealed body survives: {src}"
        );
    }

    const ONEOF: &str = "\
model ua:
    x string

model ub:
    kind string

model armA:
    kind string
    inner (ua | []ub) #sealed

model armB:
    kind string
    z string

oneof cfg by kind:
    \"a\" -> armA
    \"b\" -> armB
";
    let src = "\
cfg base:
    kind = \"a\"
    inner as ua:
        x = \"1\"

cfg top uses base:
    kind = \"b\"
    z = \"2\"
";
    let (_, diags) = compose(ONEOF, src, "cfg", "top");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    assert!(diags[0].message.contains("'inner'"), "{}", diags[0].message);
}

#[test]
fn sealed_union_positions_still_report_a_bogus_as() {
    const SEALED: &str = "\
model ua:
    x string

model ub:
    y string

model holder27:
    slot (ua | ub) #sealed
";
    let src = "\
holder27 base:
    slot as ua:
        x = \"1\"

holder27 top uses base:
    slot as zz:
        y = \"2\"
";
    let (_, diags) = compose(SEALED, src, "holder27", "top");
    // Order per the RFC 0025 §5 contract (Phase 0): both findings
    // are the top layer's, so they sort by SPAN within the layer —
    // the seal rejection anchors at the annotation, ahead of the
    // unknown-variant finding. (Same-layer interleavings are
    // span-ordered; cross-layer order stays stack order.)
    assert_eq!(
        codes_of(&diags),
        [codes::SEALED_FIELD_VIOLATION, codes::UNKNOWN_UNION_VARIANT]
    );
}

#[test]
fn later_list_variants_and_set_variants_are_unreachable_by_shape() {
    // The resolver selects the FIRST `List` variant for a list body
    // and never a set variant; judgment, lint and validator agree.
    const S: &str = "\
model ua:
    x string

model ub:
    kind string

model uc:
    kind string
    secret string #sealed

model holder28:
    slot (ua | []ub | []uc)
";
    let src = "\
holder28 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"s\"

holder28 top uses base:
    slot as ua:
        x = \"1\"
";
    let (resolved, diags) = compose(S, src, "holder28", "top");
    assert!(
        diags.is_empty(),
        "uc is unreachable, its seal is not judged: {diags:?}"
    );
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ua")
    );
    let lint = validate_merge_policies(&index_from(S));
    assert!(
        lint.is_empty(),
        "no promise for an unreachable variant: {lint:?}"
    );

    const SET: &str = "\
model ua:
    x string

model ub:
    secret string #sealed

model holder29:
    slot (ua | set<ub>)
";
    let lint = validate_merge_policies(&index_from(SET));
    assert!(
        lint.is_empty(),
        "a set variant is unreachable by shape: {lint:?}"
    );
}

#[test]
fn pin_bookkeeping_survives_empty_discard_and_restatement() {
    const S: &str = "\
model ua:
    x string

model uc:
    x string

model holder30:
    slot (ua | uc | []ub | string)

model ub:
    kind string
";
    // pin, then a zero-item entry: the pin and both values survive
    let src = "\
holder30 base:
    slot:
        x = \"1\"

holder30 mid uses base:
    slot as ua:
        x = \"2\"

holder30 top uses mid:
    slot = []
";
    let (resolved, diags) = compose(S, src, "holder30", "top");
    assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
    let body = resolved.unwrap().body;
    assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
    assert_eq!(
        nested_scalar(&body, "slot", "x"),
        Some(&Value::String("2".into()))
    );

    // pin, then a discard: "established here" points at the PIN
    let src = "\
holder30 base:
    slot:
        x = \"1\"

holder30 mid uses base:
    slot as ua:
        x = \"2\"

holder30 top uses mid:
    slot = \"v\"
";
    let (_, diags) = compose(S, src, "holder30", "top");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    let pin_entry = src.find("slot as ua:").unwrap();
    let note = &diags[0].related[0];
    assert_eq!(
        note.span.start, pin_entry,
        "note at the pin entry, not the ambiguous base"
    );

    // a restated `as ua` above a pin keeps the FIRST pin's identifier
    let src = "\
holder30 base:
    slot:
        x = \"1\"

holder30 mid uses base:
    slot as ua:
        x = \"2\"

holder30 top uses mid:
    slot as ua:
        x = \"3\"
";
    let (resolved, diags) = compose(S, src, "holder30", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let ann = sub_block(&resolved.unwrap().body, "slot")
        .and_then(|b| b.type_annotation.clone())
        .expect("annotated");
    let tok = src.find("as ua").unwrap() + 3;
    assert_eq!(
        (ann.span.start, ann.span.end),
        (tok, tok + 2),
        "the FIRST pin's `ua` token"
    );
}

#[test]
fn shared_only_block_over_an_establishment_is_a_warned_no_op() {
    for src in [
        "holder13 base:\n    slot as ua:\n        x = \"1\"\n\nholder13 t uses base:\n    slot:\n        .note = \"m\"\n",
        "holder13 base:\n    slot:\n        - w:\n            note = \"n\"\n\nholder13 t uses base:\n    slot:\n        .note = \"m\"\n",
    ] {
        let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, src, "holder13", "t");
        assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY], "{src}");
        assert!(resolved.is_some());
    }
}

#[test]
fn a_union_field_named_like_the_discriminator_composes_beside_it() {
    // The merge strips a oneof's string discriminator before merging
    // an arm body; a union FIELD named like it (an advisory NML2054
    // shape) still composes beside the canonical entry — the strip
    // is by discriminator entries, never the same-named field group.
    const S: &str = "\
model va:
    p string

model vb:
    q string

model arma:
    kind (va | vb)

model armb:
    z string

oneof oo2 by kind = \"a\":
    \"a\" -> arma
    \"b\" -> armb

model holder33:
    cfg oo2
";
    let src = "\
holder33 base:
    cfg:
        kind = \"a\"

holder33 top uses base:
    cfg:
        kind as va:
            p = \"1\"
";
    let (resolved, diags) = compose(S, src, "holder33", "top");
    assert!(diags.is_empty(), "aligned, no NML2086: {diags:?}");
    let body = resolved.unwrap().body;
    let cfg = sub_block(&body, "cfg").expect("cfg");
    let kinds = cfg
        .entries
        .iter()
        .filter(|e| match &e.kind {
            BodyEntryKind::Property(p) => p.name.name == "kind",
            BodyEntryKind::NestedBlock(nb) => nb.name.name == "kind",
            _ => false,
        })
        .count();
    assert_eq!(kinds, 2, "the discriminator and the union field, once each");
}

#[test]
fn empty_block_modifiers_at_union_positions() {
    // Over a named establishment at a list-admitting union: a warned
    // no-op that keeps the value; at a listless union: a loud
    // whole-value discard (the modifier twin of the array case).
    let over_named = "\
holder13 base:
    slot as ua:
        x = \"1\"

holder13 t uses base:
    |slot:
";
    let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, over_named, "holder13", "t");
    assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
    assert_eq!(
        nested_scalar(&resolved.unwrap().body, "slot", "x"),
        Some(&Value::String("1".into()))
    );
    let listless = "\
holder base:
    slot as ua:
        x = \"1\"

holder t uses base:
    |slot:
";
    let (_, diags) = compose(UNION_SCHEMA, listless, "holder", "t");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
}

#[test]
fn structural_notes_follow_the_latest_supplier() {
    let src = "\
holder4 base:
    slot = \"a\"

holder4 mid uses base:
    slot = \"b\"

holder4 top uses mid:
    slot:
        x = \"1\"
";
    let (_, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "top");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    let note = &diags[0].related[0];
    assert_eq!(note.message, "in force here");
    let mid_at = src.find("holder4 mid").unwrap();
    let top_at = src.find("holder4 top").unwrap();
    assert!(note.span.start > mid_at && note.span.start < top_at);
}

#[test]
fn set_first_union_still_judges_the_first_list_variants_seals() {
    // Round-17 regression: the displaced-list judgment took its
    // element from the first list-LIKE variant — a `set<string>`
    // ahead of the list variant judged under `string` (no
    // vocabulary, no scan) and the switch discarded sealed items
    // silently. Block items resolve to the first `List` everywhere
    // else; the judgment binds there, whatever precedes it.
    const S: &str = "\
model ua:
    x string

model ub:
    kind string
    secret string #sealed

model holder34:
    slot (ua | set<string> | []ub)

model holder35:
    slot (ua | []ub | set<string>)
";
    for root in ["holder34", "holder35"] {
        let src = format!(
            "{root} base:\n    slot:\n        - w:\n            kind = \"k\"\n            secret = \"s\"\n\n\
                 {root} top uses base:\n    slot as ua:\n        x = \"1\"\n"
        );
        let (_, diags) = compose(S, &src, root, "top");
        assert_eq!(
            codes_of(&diags),
            [codes::SEALED_FIELD_VIOLATION],
            "{root}: {diags:?}"
        );
        assert!(
            diags[0].message.contains("slot[w].secret"),
            "{}",
            diags[0].message
        );
        // The `.shared` twin: a list-level write distributed to the item.
        let src = format!(
            "{root} base:\n    slot:\n        .secret = \"s\"\n        - w:\n            kind = \"k\"\n\n\
                 {root} top uses base:\n    slot as ua:\n        x = \"1\"\n"
        );
        let (_, diags) = compose(S, &src, root, "top");
        assert_eq!(
            codes_of(&diags),
            [codes::SEALED_FIELD_VIOLATION],
            "{root} shared: {diags:?}"
        );
        assert!(
            diags[0].message.contains("slot[w].secret"),
            "{}",
            diags[0].message
        );
    }
}

#[test]
fn a_positional_token_under_a_set_first_union_is_judged_non_disclosingly() {
    const S: &str = "\
model ua:
    x string

model ub:
    kind string+
    secret string #sealed

model h58:
    slot (ua | set<string> | []ub)
";
    let src = "\
h58 base:
    slot:
        - \"k\":
            secret = \"s\"

h58 top uses base:
    slot as ua:
        x = \"1\"
";
    let (_, diags) = compose(S, src, "h58", "top");
    assert_eq!(
        codes_of(&diags),
        [codes::SEALED_FIELD_VIOLATION],
        "{diags:?}"
    );
    assert!(
        diags[0].message.contains("slot[string].secret") && !diags[0].message.contains("\"k\""),
        "{}",
        diags[0].message
    );
}

#[test]
fn surviving_indexes_is_the_one_survivorship_rule() {
    use ArmDecision as D;
    let id = InstanceId {
        source_path: "m.nml",
        name: "b",
    };
    let trace = |ds: Vec<ArmDecision<'static>>| -> Vec<Decision<'static>> {
        ds.into_iter().map(|d| (id, d)).collect()
    };
    let rejected = || D::Rejected { seals: Vec::new() };
    let discarded = || D::Discarded {
        over: Establishment::Value,
        lost: Establishment::Items,
    };
    assert_eq!(
        surviving_indexes(
            &trace(vec![
                D::Join,
                D::Pinned,
                D::Switch,
                rejected(),
                discarded(),
                D::Join
            ]),
            Face::Union
        ),
        [2, 5],
        "a switch restarts; a rejection and a discard contribute nothing"
    );
    assert_eq!(
        surviving_indexes(&trace(vec![D::Join, D::Pinned]), Face::Union),
        [0, 1]
    );
    // The oneof face excludes a pin — a union-only verdict there,
    // diagnosed Dropped (RFC 0025 §6) — and keeps everything else.
    assert_eq!(
        surviving_indexes(&trace(vec![D::Join, D::Pinned]), Face::OneOf),
        [0]
    );
    assert_eq!(
        surviving_indexes(&trace(vec![D::Switch, D::Switch]), Face::Union),
        [1]
    );
    assert!(surviving_indexes(&trace(vec![rejected(), discarded()]), Face::Union).is_empty());
    assert!(surviving_indexes(&[], Face::OneOf).is_empty());
}

#[test]
fn nested_traces_fold_over_the_surviving_parent_group() {
    // A discarded lower layer must not poison a nested position's
    // decisions: its stated nested arm would leave the survivors all
    // traced Join, the replay stuck at the DEFAULT arm, and the seal
    // silently overlaid — with the discarded layer's discriminator
    // still prepended, a body merged under the wrong vocabulary.
    let src = "\
po l1:
    sub:
        sk = \"sx\"

po l2 uses l1:
    pk = \"b\"
    sub:
        sk = \"sx\"
        v = \"one\"

po top uses l2:
    sub:
        sk = \"sx\"
        v = \"two\"
";
    let (resolved, diags) = compose(NESTED_UNDER_PARENT_SCHEMA, src, "po", "top");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION) && d.message.contains("sub.v")),
        "the surviving group's nested seal holds: {diags:?}"
    );
    assert_eq!(
        nested_scalar(&resolved.unwrap().body, "sub", "v"),
        Some(&Value::String("one".into())),
        "the first surviving write wins"
    );
}

#[test]
fn duplicate_field_entries_replay_positionally() {
    // One layer may state a oneof-typed field twice — two
    // contributions, two decisions, ONE id. An id-keyed trace lookup
    // collapsed them: a switch stuck, or the pre-Join switch body's
    // sealed key silently vanished.
    let schema = "\
model gcp:
    kind string
    path string

model az:
    kind string
    azureUrl string
    azureKey string

oneof conf by kind = \"gcp\":
    \"gcp\" -> gcp
    \"az\" -> az

model svc:
    cfg conf
";
    let src = "\
svc base:
    cfg:
        path = \"p\"

svc t uses base:
    cfg:
        kind = \"az\"
        azureKey = \"k\"
    cfg:
        azureUrl = \"u\"
";
    let (resolved, _) = compose(schema, src, "svc", "t");
    let body = resolved.unwrap().body;
    assert_eq!(
        nested_scalar(&body, "cfg", "azureKey"),
        Some(&Value::String("k".into())),
        "the switching entry's own fields survive its sibling Join"
    );
    assert_eq!(
        nested_scalar(&body, "cfg", "azureUrl"),
        Some(&Value::String("u".into())),
        "the post-switch sibling deep-merges"
    );
    assert_eq!(
        nested_scalar(&body, "cfg", "path"),
        None,
        "the switch discards the pre-switch arm's fields"
    );
}
