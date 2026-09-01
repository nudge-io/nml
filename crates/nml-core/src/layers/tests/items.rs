use super::super::*;
use super::*;

#[test]
fn identity_append_pair_merges_matches_and_appends_rest() {
    let src = "\
flow base:
    steps:
        - search:
            action = \"type\"
            locator = \"#q\"

flow t uses base:
    steps:
        - search:
            locator = \"#q2\"
        - confirm:
            action = \"click\"
            locator = \"#ok\"
";
    let (resolved, diags) = compose(PAIR_SCHEMA, src, "flow", "t");
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(
        list_names(&resolved.unwrap().body, "steps"),
        ["search", "confirm"]
    );
}

#[test]
fn duplicate_identity_within_one_layer_is_2063() {
    let src = "\
flow base:
    steps:
        - search:
            action = \"a\"
        - search:
            action = \"b\"

flow t uses base:
    steps:
        - search:
            locator = \"#x\"
";
    let (_, diags) = compose(PAIR_SCHEMA, src, "flow", "t");
    assert!(codes_of(&diags).contains(&codes::IDENTITY_REDEFINITION));
}

// ── zero-item + dead delta ───────────────────────────────────────────

#[test]
fn array_spelled_lists_inside_identity_items_merge() {
    let src = "\
flow base:
    steps:
        - search:
            action = \"type\"
            tags = [\"slow\"]

flow t uses base:
    steps:
        - search:
            tags = [\"fast\"]
";
    let (resolved, diags) = compose(STEP_FLOW_SCHEMA, src, "flow", "t");
    assert!(diags.is_empty(), "clean merge: {diags:?}");
    let body = resolved.unwrap().body;
    let steps = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "steps" => Some(&nb.body),
            _ => None,
        })
        .expect("steps present");
    let item_body = steps
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::ListItem(ListItem {
                kind: ListItemKind::Named { body, .. },
                ..
            }) => Some(body),
            _ => None,
        })
        .expect("named item");
    let tags = list_names(item_body, "tags");
    assert_eq!(tags.len(), 2, "both layers' tags survive: {tags:?}");
}

#[test]
fn same_kind_identity_match_beats_cross_kind_collision() {
    let src = "\
flow base:
    steps:
        - \"a\"
        - a:
            action = \"x\"

flow t uses base:
    steps:
        - a:
            action = \"y\"
";
    let (resolved, diags) = compose(STEP_FLOW_SCHEMA, src, "flow", "t");
    assert!(
        !codes_of(&diags).contains(&codes::IDENTITY_REDEFINITION),
        "the named overlay pairs with the named base entry: {diags:?}"
    );
    let body = resolved.unwrap().body;
    let steps = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "steps" => Some(&nb.body),
            _ => None,
        })
        .expect("steps present");
    let named_action = steps.entries.iter().find_map(|e| match &e.kind {
        BodyEntryKind::ListItem(ListItem {
            kind: ListItemKind::Named { body, .. },
            ..
        }) => scalar(body, "action"),
        _ => None,
    });
    assert_eq!(
        named_action,
        Some(&Value::String("y".into())),
        "the override lands on the same-kind partner"
    );
}

#[test]
fn scalar_keyed_identity_merge_is_not_a_dead_delta() {
    let schema = "\
model route:
    path string+
    timeout string

model api:
    routes []route #identity
";
    let src = "\
api base:
    routes:
        - \"/api\"

api t uses base:
    routes:
        - \"/api\":
            timeout = \"60\"
";
    let (resolved, diags) = compose(schema, src, "api", "t");
    assert!(
        !codes_of(&diags).contains(&codes::DEAD_DELTA),
        "the materialized identity token is pairing machinery, not a \
             restatement: {diags:?}"
    );
    let body = resolved.unwrap().body;
    let routes = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "routes" => Some(&nb.body),
            _ => None,
        })
        .expect("routes present");
    let timeout = routes.entries.iter().find_map(|e| match &e.kind {
        BodyEntryKind::ListItem(ListItem {
            kind: ListItemKind::Shorthand { body: Some(b), .. },
            ..
        }) => scalar(b, "timeout"),
        _ => None,
    });
    assert_eq!(timeout, Some(&Value::String("60".into())), "merge landed");
}

#[test]
fn cross_kind_identity_collision_is_2063() {
    let src = "\
flow base:
    steps:
        - search:
            action = \"x\"

flow t uses base:
    steps:
        - \"search\"
";
    let (_, diags) = compose(STEP_FLOW_SCHEMA, src, "flow", "t");
    assert!(
        codes_of(&diags).contains(&codes::IDENTITY_REDEFINITION),
        "shorthand vs named at an equal token is cross-kind NML2063: {diags:?}"
    );
}

#[test]
fn reference_items_identical_restatement_is_a_noop() {
    let schema = "\
model flow2:
    steps []string #identity
";
    let src = "\
flow2 base:
    steps = [@ops]

flow2 t uses base:
    steps = [@ops]
";
    let (resolved, diags) = compose(schema, src, "flow2", "t");
    assert!(
        diags.is_empty(),
        "identical restatement is a no-op: {diags:?}"
    );
    assert_eq!(
        list_names(&resolved.unwrap().body, "steps").len(),
        1,
        "one role item, not a duplicate"
    );
}

#[test]
fn item_group_seal_meets_cross_layer_discriminator() {
    // Identity-matched items compose as ONE value: a discriminator in
    // the base's item and a seal in the mid's item must meet in the
    // backstop, or the pair launders through a switch.
    let src = "\
svc base:
    kind = \"x\"
    steps:
        - w:
            ikind = \"sp\"

svc mid uses base:
    steps:
        - w:
            secret = \"s3\"

svc t uses mid:
    kind = \"y\"
    note = \"n\"
";
    let (_, diags) = compose(ITEM_ONEOF_SCHEMA, src, "svc", "t");
    let seal = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("cross-layer item seal blocks the switch");
    assert!(
        seal.message.contains("steps[w].secret"),
        "full item path: {}",
        seal.message
    );
}

#[test]
fn oneof_element_items_enforce_seals_directly() {
    // A oneof ELEMENT routes item bodies through the arm accumulator:
    // restating a sealed arm field across layers is NML2060, not a
    // silent model-less overwrite.
    let schema = "\
model spArm:
    ikind string
    secret string #sealed

model ptArm:
    ikind string
    port string

oneof istep by ikind = \"sp\":
    \"sp\" -> spArm
    \"pt\" -> ptArm

model flow:
    steps []istep #identity
";
    let src = "\
flow base:
    steps:
        - w:
            ikind = \"sp\"
            secret = \"a\"

flow t uses base:
    steps:
        - w:
            secret = \"b\"
";
    let (resolved, diags) = compose(schema, src, "flow", "t");
    assert!(
        codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "arm-aware item merge enforces the seal: {diags:?}"
    );
    let body = resolved.unwrap().body;
    let secret = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "steps" => Some(&nb.body),
            _ => None,
        })
        .and_then(|steps| {
            steps.entries.iter().find_map(|e| match &e.kind {
                BodyEntryKind::ListItem(ListItem {
                    kind: ListItemKind::Named { body, .. },
                    ..
                }) => scalar(body, "secret"),
                _ => None,
            })
        });
    assert_eq!(
        secret,
        Some(&Value::String("a".into())),
        "the sealed base value survives"
    );
}

#[test]
fn shared_property_seals_block_switches() {
    // `.shared`-distributed sealed writes are authored semantics: the
    // fold judges displaced-arm-NORMALIZED bodies, so a shared value
    // that materializes into (even bodiless) items still counts.
    let schema = "\
model xItem:
    name string+
    val string #sealed

model armX:
    xs []xItem #identity

model armY:
    note string

oneof svc by kind = \"x\":
    \"x\" -> armX
    \"y\" -> armY
";
    let src = "\
svc base:
    kind = \"x\"
    xs:
        .val = \"v\"
        - \"one\"
        - \"two\"

svc t uses base:
    kind = \"y\"
    note = \"n\"
";
    let (_, diags) = compose(schema, src, "svc", "t");
    let seal = diags
        .iter()
        .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .expect("shared-distributed seals are assigned seals");
    // ONE authored `.val` write distributed into two items is ONE
    // assignment on TWO fields (RFC 0019 counts fields — E17): the
    // identity keeps the items apart even though both injected
    // copies carry the authored span and render `xs[string].val`
    // alike — and ONE note points at the one authored site.
    assert!(
        seal.message.contains("(and 1 more field)"),
        "two distributed fields: {}",
        seal.message
    );
    assert_eq!(
        seal.related.len(),
        1,
        "one note per distinct assignment: {seal:?}"
    );
}

#[test]
fn items_after_a_rejected_switch_join_the_original_list_establishment() {
    let src = "\
holder13 base:
    slot:
        - w:
            secret = \"locked\"

holder13 mid uses base:
    slot as ua:
        x = \"1\"

holder13 t uses mid:
    slot:
        - v:
            note = \"n\"
";
    let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, src, "holder13", "t");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    let body = resolved.unwrap().body;
    assert_eq!(slot_annotation(&body), None, "still the list establishment");
    let names: Vec<String> = sub_block(&body, "slot")
        .map(|b| {
            b.entries
                .iter()
                .filter_map(|e| match &e.kind {
                    BodyEntryKind::ListItem(ListItem {
                        kind: ListItemKind::Named { name, .. },
                        ..
                    }) => Some(name.name.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(names, vec!["v"], "the top list wins by the bare-list rule");
}

#[test]
fn memo_is_invalidated_when_a_join_changes_the_list() {
    // Two rejected switches reuse one judgment; a layer that
    // supplies a NEW list (the bare-list winner changes) is judged
    // fresh — the count suffix follows the new list.
    let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"a\"

holder7 l1 uses base:
    slot as ua:
        x = \"1\"

holder7 l2 uses l1:
    slot as ua:
        x = \"2\"

holder7 l3 uses l2:
    slot:
        - v:
            kind = \"k\"
            secret = \"b\"
        - u:
            kind = \"k\"
            secret = \"c\"

holder7 l4 uses l3:
    slot as ua:
        x = \"4\"
";
    let (_, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "l4");
    let seals: Vec<&Diagnostic> = diags
        .iter()
        .filter(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
        .collect();
    assert_eq!(seals.len(), 3, "{diags:?}");
    assert!(!seals[0].message.contains("(and "), "{}", seals[0].message);
    assert!(!seals[1].message.contains("(and "), "{}", seals[1].message);
    assert!(
        seals[2].message.contains("(and 1 more field)"),
        "judged fresh over l3's list: {}",
        seals[2].message
    );
}

#[test]
fn zero_item_entry_between_list_layers_keeps_the_judged_group() {
    let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"locked\"

holder7 mid uses base:
    slot = []

holder7 top uses mid:
    slot as ua:
        x = \"1\"
";
    let (resolved, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "top");
    assert_eq!(
        codes_of(&diags),
        [codes::ZERO_ITEM_LAYER_ENTRY, codes::SEALED_FIELD_VIOLATION]
    );
    assert_eq!(slot_annotation(&resolved.unwrap().body), None);
}

#[test]
fn mixed_spelling_lists_replace_and_the_replaced_list_is_not_judged() {
    let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"locked\"

holder7 mid uses base:
    |slot:
        - v:
            kind = \"k\"

holder7 top uses mid:
    slot as ua:
        x = \"1\"
";
    let (resolved, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "top");
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ua")
    );
}

#[test]
fn the_judgment_memo_reuses_an_unchanged_group() {
    // A DoS defence has no behavioral signature: count the
    // judgments actually computed. Two rejected switches over one
    // unchanged list = ONE judgment; a new winner list = one more.
    let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"a\"

holder7 l1 uses base:
    slot as ua:
        x = \"1\"

holder7 l2 uses l1:
    slot as ua:
        x = \"2\"

holder7 l3 uses l2:
    slot:
        - v:
            kind = \"k\"
            secret = \"b\"

holder7 l4 uses l3:
    slot as ua:
        x = \"4\"
";
    JUDGMENT_MISSES.with(|c| c.set(0));
    let (_, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "l4");
    assert_eq!(
        diags
            .iter()
            .filter(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .count(),
        3
    );
    // The plan folds once (2 misses: base list, l3's list) and the
    // merge replays it — no refold, no extra judgment.
    assert_eq!(
        JUDGMENT_MISSES.with(|c| c.get()),
        2,
        "one judgment per list, reused across rejections"
    );
}

#[test]
fn items_established_here_follows_the_effective_list() {
    // Under a list establishment the effective list is the highest
    // supplier: a later discard's note points there.
    let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"

holder7 mid uses base:
    slot:
        - v:
            kind = \"k\"

holder7 top uses mid:
    slot:
        x = \"1\"
";
    let (_, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "top");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    let mid_at = src.find("holder7 mid").unwrap();
    let top_at = src.find("holder7 top").unwrap();
    let note = diags[0].related[0].span.start;
    assert!(
        note > mid_at && note < top_at,
        "the winner list, not the first (nor the discard itself)"
    );
}

// ── union compose: round-17 battery (nothing under a seal is planned,
//    plan/merge parity across the discriminator strip, the loud
//    misalignment path, declarations everywhere, one sink) ─────────

#[test]
fn distinct_items_sharing_a_non_disclosing_path_count_separately() {
    // Two scalar-keyed items render the same `slot[string]` path but
    // are two discarded seals — the sink dedups by (path, span).
    let src = "\
holder13 base:
    slot:
        - \"a\"
        - \"b\"

holder13 t uses base:
    slot as ua:
        x = \"1\"
";
    let (_, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, src, "holder13", "t");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    assert!(
        diags[0].message.contains("(and 1 more field)"),
        "{}",
        diags[0].message
    );

    let arms = "\
router base:
    route:
        \"a\" -> One:
            token = \"t1\"
        \"b\" -> One:
            token = \"t2\"

router t uses base:
    route:
        \"c\" -> Two:
            note = \"n\"
";
    let (_, diags) = compose(ARM_SET_SCHEMA, arms, "router", "t");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    assert!(
        diags[0].message.contains("(and 1 more field)"),
        "two arms' `token` seals are two FIELDS (Seg::Arm) that \
             render alike: {}",
        diags[0].message
    );
}

#[test]
fn item_scope_notes_point_at_the_base_item() {
    // A merged identity item keeps the HEAD item's span — the base,
    // when nothing switches (as here) — so a three-layer chain's
    // discard note lands on the base entry, not the middle join.
    const S: &str = "\
model ua:
    x string

model ub:
    kind string

model holder36:
    items [](ua | []ub) #identity
";
    let src = "\
holder36 base:
    items:
        - w:
            x = \"1\"

holder36 mid uses base:
    items:
        - w:
            x = \"2\"

holder36 top uses mid:
    items:
        - w:
            - \"z\"
";
    let (_, diags) = compose(S, src, "holder36", "top");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    let mid_at = src.find("holder36 mid").unwrap();
    assert!(
        diags[0].related[0].span.start < mid_at,
        "the note points at the base item, not the middle join"
    );
}

#[test]
fn item_scope_notes_follow_a_switched_head() {
    // The switching twin of `item_scope_notes_point_at_the_base_item`
    // (RFC 0019 E15): after MID's accepted `as` switch the
    // accumulated item carries MID's span, so TOP's discard note
    // lands on the establishment actually in force — the item that
    // produced the body — not on the displaced base. The base
    // authors `as ua` so it ESTABLISHES a named variant: over an
    // un-annotated (ambiguous) base, mid's `as` would be a PIN, and
    // a pin names without displacing — the head stays the base.
    const S: &str = "\
model ua:
    x string

model ub:
    kind string

model holder43:
    items [](ua | ub) #identity
";
    let src = "\
holder43 base:
    items:
        - w as ua:
            x = \"1\"

holder43 mid uses base:
    items:
        - w as ub:
            kind = \"k\"

holder43 top uses mid:
    items:
        - w:
            - \"z\"
";
    let (_, diags) = compose(S, src, "holder43", "top");
    assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    let mid_item = src.find("- w as ub:").unwrap();
    assert_eq!(
        diags[0].related[0].span.start, mid_item,
        "the note points at the switching item: {diags:?}"
    );
}

#[test]
fn item_scopes_fold_under_their_own_bracketed_paths() {
    // The FOLD_LOG shape (RFC 0025 §6, replacing the plan's lookup
    // log): an identity group folds ONCE, under its own bracketed
    // scope — never the container's dotted path, and never once per
    // layer; a bare list's items never fold at all (nothing decides
    // in a wholesale-replaced list); scalar keys appear only as
    // non-disclosing type segments.
    const S: &str = "\
model r:
    ra string

model s:
    sa string

model ua:
    x string

model ub:
    kind string
    x (r | s)

model h55:
    slot (ua | []ub)

model h56:
    steps []ub #identity

model h57:
    slot []ub #identity
";
    let run = |root: &str, src: &str| -> Vec<String> {
        FOLD_LOG.with(|l| l.borrow_mut().clear());
        let (_, _) = compose(S, src, root, "top");
        FOLD_LOG.with(|l| l.borrow().clone())
    };
    // An ITEMS-established union list: the position folds, its items
    // never do (the bare-list winner replaces wholesale).
    let folded = run(
        "h55",
        "h55 base:\n    slot:\n        - w:\n            kind = \"k\"\n            x as r:\n                ra = \"1\"\n\n\
             h55 top uses base:\n    slot:\n        - v:\n            kind = \"k\"\n            x as s:\n                sa = \"2\"\n",
    );
    assert!(folded.iter().any(|p| p == "slot"), "{folded:?}");
    assert!(
        !folded.iter().any(|p| p.contains('[') || p == "slot.x"),
        "a bare list's items never fold: {folded:?}"
    );
    // An identity group folds ONCE (n-ary), under its own bracketed
    // scope; the container's dotted `steps.x` never folds.
    let folded = run(
        "h56",
        "h56 base:\n    steps:\n        - a:\n            kind = \"k\"\n            x as r:\n                ra = \"1\"\n\n\
             h56 top uses base:\n    steps:\n        - a:\n            x as r:\n                ra = \"2\"\n",
    );
    assert_eq!(
        folded,
        ["steps[a].x"],
        "one fold per group, under the item's own scope"
    );
    // Scalar keys: non-disclosing type segments in the folded path.
    let folded = run(
        "h57",
        "h57 base:\n    slot:\n        - \"k1\":\n            x as r:\n                ra = \"1\"\n        - 7:\n            x as s:\n                sa = \"2\"\n\n\
             h57 top uses base:\n    slot:\n        - \"k1\":\n            x as r:\n                ra = \"3\"\n        - 7:\n            x as s:\n                sa = \"4\"\n",
    );
    assert!(
        folded.iter().any(|p| p == "slot[string].x")
            && folded.iter().any(|p| p == "slot[number].x"),
        "{folded:?}"
    );
    assert!(
        !folded.iter().any(|p| p.contains("k1") || p.contains("k2")),
        "a scalar key is never disclosed: {folded:?}"
    );
}

#[test]
fn schemaless_item_bearing_groups_replace_wholesale() {
    // The bare-list rule binds structural mode: deep-merging an
    // item-bearing nested group concatenates layers' items and
    // duplicates restated identities (base a,b + overlay a → a,b,a).
    let src = "\
box base:
    steps:
        - \"a\"
        - \"b\"

box t uses base:
    steps:
        - \"a\"
";
    let (resolved, _) = compose("", src, "box", "t");
    let body = resolved.unwrap().body;
    assert_eq!(
        list_names(&body, "steps").len(),
        1,
        "the supplying overlay replaces wholesale — no concatenation, \
             no duplicated identity: {body:?}"
    );
    // Named restatement: the overlay's item wins alone, never both.
    let src2 = "\
box base:
    steps:
        - s1:
            v = \"1\"

box t uses base:
    steps:
        - s1:
            v = \"2\"
";
    let (resolved, _) = compose("", src2, "box", "t");
    let body = resolved.unwrap().body;
    assert_eq!(list_names(&body, "steps"), vec!["s1"]);
}

#[test]
fn token_prehash_covers_every_scalar_kind() {
    use crate::duration::Duration;
    let h = |v: Value| token_prehash(&ItemKey::Scalar(v));
    // Durations: semantic equals collide, distinct scatter.
    let d = |s: &str| Value::Duration(Duration::parse_text(s).unwrap());
    assert_eq!(h(d("90m")), h(d("1h30m")), "equal durations share a bucket");
    assert_ne!(h(d("90m")), h(d("91m")), "distinct durations scatter");
    // Bools.
    assert_ne!(h(Value::Bool(true)), h(Value::Bool(false)));
    // Money: (amount, currency) is the identity.
    let m = |amount, cur: &str| {
        Value::Money(crate::money::Money {
            amount,
            currency: cur.into(),
            exponent: 2,
        })
    };
    assert_eq!(h(m(150, "USD")), h(m(150, "USD")));
    assert_ne!(h(m(150, "USD")), h(m(151, "USD")));
    assert_ne!(h(m(150, "USD")), h(m(150, "EUR")));
    // Numbers: the normalized pair — `1` and `1.0` are one identity.
    let n = |s: &str| Value::Number(s.parse().unwrap());
    assert_eq!(h(n("1")), h(n("1.0")), "semantic equals share a bucket");
    assert_ne!(h(n("1")), h(n("2")), "distinct numbers scatter");
    // Strings.
    assert_eq!(h(Value::String("a".into())), h(Value::String("a".into())));
    assert_ne!(h(Value::String("a".into())), h(Value::String("b".into())));
}

#[test]
fn item_key_hash_is_coherent_with_same() {
    // `ItemKey: Eq + Hash` (Part D identity): eq is `same`, and
    // equal keys MUST hash equal — the `token_prehash` rule, tagged
    // by kind (`same` requires `same_kind`, so cross-kind
    // token-equals need not collide).
    use crate::duration::Duration;
    use std::hash::{Hash, Hasher};
    let hash_of = |k: &ItemKey| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        k.hash(&mut h);
        h.finish()
    };
    let d = |s: &str| ItemKey::Scalar(Value::Duration(Duration::parse_text(s).unwrap()));
    assert!(d("90m").same(&d("1h30m")));
    assert_eq!(hash_of(&d("90m")), hash_of(&d("1h30m")), "eq ⇒ hash-eq");
    assert_eq!(d("90m"), d("1h30m"), "PartialEq IS `same`");
    assert_ne!(
        ItemKey::Named("a".into()),
        ItemKey::Reference("a".into()),
        "cross-kind token-equals are not the same identity"
    );
}

#[test]
fn a_scalar_keyed_identitys_debug_never_contains_the_token() {
    // RFC 0019 requirement 4, enforced at the token holder: the
    // redacting `Debug` on `ItemKey` keeps every derived `Debug`
    // above it (Seg, FieldIdentity) non-disclosing.
    let key = ItemKey::Scalar(Value::String("hunter2-secret".into()));
    let id = FieldIdentity::default()
        .child(Seg::Item(key))
        .child(Seg::Field("secret".into()));
    let debug = format!("{id:?}");
    assert!(
        !debug.contains("hunter2"),
        "the token leaked through Debug: {debug}"
    );
    assert!(debug.contains("string"), "the TYPE renders: {debug}");
    assert_eq!(id.at("slot"), "slot[string].secret", "the ONE join");
}

#[test]
fn a_shared_line_distributed_into_named_items_is_two_fields_one_assignment() {
    // Part D (E17): one list-level `.shared` line distributed into
    // two NAMED items is TWO fields (distinct identities) written by
    // ONE assignment — `(and 1 more field)`, one note at the one
    // authored site.
    const S: &str = "\
model ua:
    x string

model ub:
    name string+
    secret string #sealed

model holder47:
    slot (ua | []ub)
";
    let src = "\
holder47 base:
    slot:
        .secret = \"s\"
        - w:
        - v:

holder47 t uses base:
    slot as ua:
        x = \"1\"
";
    let (_, diags) = compose(S, src, "holder47", "t");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    assert!(
        diags[0]
            .message
            .contains("'slot[w].secret' (and 1 more field)"),
        "{}",
        diags[0].message
    );
    assert_eq!(
        diags[0].related.len(),
        1,
        "one note per distinct assignment: {:?}",
        diags[0]
    );
}

#[test]
fn strip_resolves_the_stated_arm_for_oneof_items() {
    // The + token strip must follow the item's STATED (non-default)
    // arm — resolving only the default arm would miss the token and
    // draw a spurious dead-delta.
    let schema = "\
model spArm:
    ikind string
    note string

model ptArm:
    name string+
    ikind string
    note string

oneof istep by ikind = \"sp\":
    \"sp\" -> spArm
    \"pt\" -> ptArm

model flow:
    steps []istep #identity
";
    let src = "\
flow base:
    steps:
        - \"s1\":
            ikind = \"pt\"
            note = \"a\"

flow t uses base:
    steps:
        - \"s1\":
            ikind = \"pt\"
            note = \"b\"
";
    let (_, diags) = compose(schema, src, "flow", "t");
    assert!(
        !codes_of(&diags).contains(&codes::DEAD_DELTA),
        "the stated arm's + token is pairing machinery: {diags:?}"
    );
}

#[test]
fn mixed_spelling_sibling_pools_group_item_seals() {
    // Base block-spelled, overlay modifier-spelled — one field, one
    // identity pool: the backstop must still see the sealed item
    // write across the spellings.
    let src = "\
box base:
    cfg:
        steps:
            - s1:
                act = \"x\"

box t uses base:
    cfg:
        skind = \"b\"
        other = \"y\"
";
    let (_, diags) = compose(MODIFIER_ITEM_SEAL_SCHEMA, src, "box", "t");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)
                && d.message.contains("cannot launder")),
        "block-spelled item seal blocks the switch: {diags:?}"
    );
}

#[test]
fn numeric_keyed_items_scatter_across_buckets() {
    // Regression (DoS): a type-name prehash collapsed every numeric
    // token into one bucket, resurrecting the O(n²) identity scan.
    // Distinct numeric values must scatter; semantic-equals must not.
    use std::collections::HashSet;
    let distinct: HashSet<u64> = (0..500)
        .map(|i| token_prehash(&ItemKey::Scalar(Value::number(i))))
        .collect();
    assert!(
        distinct.len() > 400,
        "distinct numbers scatter across buckets, got {} for 500",
        distinct.len()
    );
    // `1` and `1.0` are the same value — same bucket.
    let one: crate::decimal::Number = "1".parse().unwrap();
    let one_point_oh: crate::decimal::Number = "1.0".parse().unwrap();
    assert_eq!(
        token_prehash(&ItemKey::Scalar(Value::number(one))),
        token_prehash(&ItemKey::Scalar(Value::number(one_point_oh))),
        "semantically-equal decimals share a bucket"
    );
}

#[test]
fn cross_kind_items_do_not_widen_arm_scopes() {
    // The merge refuses cross-kind pairs (NML2063) and never composes
    // them — so a scalar item's SEALED arm must not attach to a
    // same-token NAMED item's scope and fabricate a refusal of a
    // legal switch.
    let schema = "\
model epm:
    ek string
    v string

model eqm:
    ek string
    v string #sealed

oneof el by ek = \"ep\":
    \"ep\" -> epm
    \"eq\" -> eqm

model armX:
    kind string
    items []el #identity

model armY:
    kind string
    z string

oneof svc by kind = \"x\":
    \"x\" -> armX
    \"y\" -> armY
";
    let src = "\
svc base:
    kind = \"x\"
    items:
        - \"n1\":
            ek = \"eq\"

svc mid uses base:
    items:
        - n1:
            v = \"w\"

svc t uses mid:
    kind = \"y\"
    z = \"z\"
";
    let (resolved, diags) = compose(schema, src, "svc", "t");
    assert!(
        !codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
        "no seal was assigned in either item's OWN scope — the switch \
             is legal: {diags:?}"
    );
    assert_eq!(
        scalar(&resolved.unwrap().body, "kind"),
        Some(&Value::String("y".into()))
    );
}

#[test]
fn oneof_element_scalar_items_materialize_without_dead_delta() {
    // Oneof elements materialize their `+` token through the item's
    // effective arm, and the strip knows that arm too — otherwise
    // every scalar-keyed oneof item drew a spurious NML2084.
    let schema = "\
model spArm:
    name string+
    ikind string
    note string

oneof istep by ikind = \"sp\":
    \"sp\" -> spArm

model flow:
    steps []istep #identity
";
    let src = "\
flow base:
    steps:
        - \"s1\":
            note = \"a\"

flow t uses base:
    steps:
        - \"s1\":
            note = \"b\"
";
    let (resolved, diags) = compose(schema, src, "flow", "t");
    assert!(
        !codes_of(&diags).contains(&codes::DEAD_DELTA),
        "the materialized token is pairing machinery for oneof \
             elements too: {diags:?}"
    );
    let body = resolved.unwrap().body;
    let has_name = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "steps" => Some(&nb.body),
            _ => None,
        })
        .is_some_and(|steps| {
            steps.entries.iter().any(|e| match &e.kind {
                BodyEntryKind::ListItem(ListItem {
                    kind: ListItemKind::Shorthand { body: Some(b), .. },
                    ..
                }) => scalar(b, "name").is_some(),
                _ => false,
            })
        });
    assert!(has_name, "the + token materializes through the arm");
}

/// The gather-drop token mask (r42 fold, r43-probed): a dropped
/// SHORTHAND item's `.shared`-shifted reading is masked at its token
/// field — the list-wide `.ikind = "b"` never reaches it, so the drop
/// diagnoses under its own default arm, where the undeclared
/// `extras = []` is silent. The NAMED twin has no token: the shared
/// discriminator reaches it, flips its reading to the arm where
/// `extras` IS a list, and the dropped interior's NML2079 surfaces.
/// One rule (RFC 0005 §10), both directions pinned.
#[test]
fn a_dropped_items_shared_reading_yields_to_its_token() {
    const S: &str = "\
model aArm:
    ikind string+
    va string

model bArm:
    ikind string+
    extras []string

oneof istep by ikind = \"a\":
    \"a\" -> aArm
    \"b\" -> bArm

model app:
    xs []istep #identity
";
    let shorthand_drop = "\
app base:
    xs:
        - \"a\":
            va = \"1\"

app top uses base:
    xs:
        .ikind = \"b\"
        - \"zzz\":
            extras = []
";
    let (_, diags) = compose(S, shorthand_drop, "app", "top");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::UNMATCHED_OVERLAY_ITEM)),
        "{diags:?}"
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code == Some(codes::ZERO_ITEM_LAYER_ENTRY)),
        "the token mask keeps the shared discriminator OFF the drop: {diags:?}"
    );

    let named_drop = shorthand_drop.replace("- \"zzz\":", "- zzz:");
    let (_, diags) = compose(S, &named_drop, "app", "top");
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::UNMATCHED_OVERLAY_ITEM)),
        "{diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::ZERO_ITEM_LAYER_ENTRY) && d.message.contains("extras")),
        "a Named drop has no token — the shared write reaches and the \
         interior verdict surfaces under the shifted reading: {diags:?}"
    );
}
