use super::super::*;
use super::*;

#[test]
fn spelling_invariance_and_zero_item_warning() {
    let src = "\
policy base:
    denyHosts = [\"a\", \"b\"]
    label = \"x\"

policy mid uses base:
    denyHosts = [\"c\"]

policy top uses mid:
    denyHosts = []
";
    let (resolved, diags) = compose(DENY_SCHEMA, src, "policy", "top");
    assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
    let body = resolved.unwrap().body;
    let items = list_names(&body, "denyHosts");
    assert_eq!(items.len(), 3, "a, b appended with c: {items:?}");
}

#[test]
fn authored_empty_list_survives_compose() {
    // Regression: `xs = []` on a bare-overlay list vanished from the
    // composed body, cascading a spurious missing-required error.
    let schema = "model m:\n    xs []string\n    label string\n";
    let src = "\
m base:
    label = \"b\"
    xs = []

m top uses base:
    label = \"t\"
";
    let (resolved, diags) = compose(schema, src, "m", "top");
    assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
    let body = resolved.unwrap().body;
    assert!(
        body.entries.iter().any(|e| matches!(&e.kind,
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "xs")),
        "authored-empty field survives as present-but-empty"
    );
}

#[test]
fn empty_overlay_modifier_cannot_empty_base() {
    // Regression: `|deny = []` under bare overlay silently EMPTIED the
    // base's deny list — a security-shaped allow-by-emptying.
    let schema = "model policy:\n    label string\n    |deny []string\n";
    let src = "\
policy base:
    label = \"x\"
    |deny = [\"a\"]

policy t uses base:
    |deny = []
";
    let (resolved, diags) = compose(schema, src, "policy", "t");
    assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
    let body = resolved.unwrap().body;
    let kept = body.entries.iter().any(|e| {
        matches!(&e.kind,
            BodyEntryKind::Modifier(Modifier { value: ModifierValue::Block(items), .. })
                if items.len() == 1)
    });
    assert!(kept, "base deny entry survives: {body:?}");
}

#[test]
fn zero_item_base_does_not_establish_identity_list() {
    // Regression: `steps = []` in the base made the next tier's first
    // real items draw spurious NML2067 and vanish — a zero-item entry
    // neither supplies nor establishes (NML2079's contract).
    let schema = "\
model step:
    name string+
    locator string

model flow:
    steps []step #identity
";
    let src = "\
flow base:
    steps = []

flow t uses base:
    steps:
        - search:
            locator = \"#q\"
";
    let (resolved, diags) = compose(schema, src, "flow", "t");
    assert!(
        !codes_of(&diags).contains(&codes::UNMATCHED_OVERLAY_ITEM),
        "first real items are authored, not unmatched: {diags:?}"
    );
    assert_eq!(
        list_names(&resolved.unwrap().body, "steps"),
        ["search"],
        "the first item-supplying tier establishes the list"
    );
}

#[test]
fn zero_item_overlay_cannot_empty_across_spellings() {
    let schema = "\
model m:
    xs []string
";
    let src = "\
m base:
    xs:
        - \"a\"

m t uses base:
    |xs = []
";
    let (resolved, diags) = compose(schema, src, "m", "t");
    assert!(
        codes_of(&diags).contains(&codes::ZERO_ITEM_LAYER_ENTRY),
        "warned no-op: {diags:?}"
    );
    assert_eq!(
        list_names(&resolved.unwrap().body, "xs").len(),
        1,
        "the base list survives every zero-item spelling"
    );
}

#[test]
fn modifier_zero_item_warning_is_list_scoped() {
    let schema = "\
model m:
    |note string
    xs []string
";
    let src = "\
m base:
    xs = [\"a\"]

m t uses base:
    |note = []
";
    let (_, diags) = compose(schema, src, "m", "t");
    assert!(
        !codes_of(&diags).contains(&codes::ZERO_ITEM_LAYER_ENTRY),
        "NML2079 is list-scoped — a non-list modifier is the type \
             checker's business: {diags:?}"
    );
}

// ── round-6 review pins ──────────────────────────────────────────────

#[test]
fn all_zero_item_modifier_survivor_keeps_its_spelling() {
    let src = "\
holder13 base:
    slot = []

holder13 t uses base:
    |slot:
";
    let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, src, "holder13", "t");
    assert_eq!(
        codes_of(&diags),
        [codes::ZERO_ITEM_LAYER_ENTRY, codes::ZERO_ITEM_LAYER_ENTRY]
    );
    let body = resolved.unwrap().body;
    assert!(
        body.entries.iter().any(|e| matches!(&e.kind,
                BodyEntryKind::Modifier(m) if m.name.name == "slot")),
        "a modifier survivor keeps its spelling: {body:?}"
    );
}

#[test]
fn sealed_positions_normalize_under_their_own_variant() {
    // A `#sealed` position is owned by write-once alone: the
    // surviving lowest body normalizes under ITS OWN variant, never
    // a rejected upper layer's (RFC 0025 §4, E13's merge-time
    // restatement). Union and oneof twins.
    const S: &str = "\
model ua:
    x string

model ub:
    items []string

model holder31:
    slot (ua | ub) #sealed
";
    let src = "\
holder31 base:
    slot as ub:
        items = []

holder31 top uses base:
    slot as ua:
        x = \"1\"
";
    let (resolved, diags) = compose(S, src, "holder31", "top");
    assert_eq!(
        codes_of(&diags),
        [codes::ZERO_ITEM_LAYER_ENTRY, codes::SEALED_FIELD_VIOLATION],
        "the base normalizes under ITS variant (its NML2079 survives): {diags:?}"
    );
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ub")
    );
    const O: &str = "\
model arma:
    kind string
    items []string

model armb:
    kind string
    z string

oneof oo by kind:
    \"a\" -> arma
    \"b\" -> armb

model holder32:
    cfg oo #sealed
";
    let src = "\
holder32 base:
    cfg:
        kind = \"a\"
        items = []

holder32 top uses base:
    cfg:
        kind = \"b\"
        z = \"1\"
";
    let (_, diags) = compose(O, src, "holder32", "top");
    assert_eq!(
        codes_of(&diags),
        [codes::ZERO_ITEM_LAYER_ENTRY, codes::SEALED_FIELD_VIOLATION]
    );
}

#[test]
fn annotated_empty_blocks_are_writes_not_zero_item() {
    const S: &str = "\
model ua:
    x string

model uc:
    z string

model ub:
    kind string

model holder34:
    slot (ua | uc | []ub) #sealed
";
    let sealed = "\
holder34 base:
    slot as ua:
        x = \"1\"

holder34 top uses base:
    slot as ua:
";
    let (resolved, diags) = compose(S, sealed, "holder34", "top");
    assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    assert_eq!(
        nested_scalar(&resolved.unwrap().body, "slot", "x"),
        Some(&Value::String("1".into()))
    );

    const OPEN: &str = "\
model ua:
    x string

model uc:
    z string

model ub:
    kind string

model holder35:
    slot (ua | uc | []ub)
";
    let restated = "\
holder35 base:
    slot as ua:
        x = \"1\"

holder35 top uses base:
    slot as ua:
";
    let (resolved, diags) = compose(OPEN, restated, "holder35", "top");
    assert!(
        diags.is_empty(),
        "an annotated restatement joins: {diags:?}"
    );
    assert_eq!(
        nested_scalar(&resolved.unwrap().body, "slot", "x"),
        Some(&Value::String("1".into()))
    );
    let switched = "\
holder35 base:
    slot as ua:
        x = \"1\"

holder35 top uses base:
    slot as uc:
";
    let (resolved, diags) = compose(OPEN, switched, "holder35", "top");
    assert!(
        diags.is_empty(),
        "an annotated empty block switches: {diags:?}"
    );
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("uc")
    );
}

#[test]
fn empty_spellings_on_a_declared_scalar_modifier_are_values() {
    // `|label:` / `|label = []` on a declared SCALAR modifier are
    // values (a type error the validator owns), never zero-item
    // no-ops that vanish from the composed view.
    const S: &str = "\
model m2:
    |label string
";
    for src in [
        "m2 base:\n    |label = \"a\"\n\nm2 t uses base:\n    |label:\n",
        "m2 base:\n    |label = \"a\"\n\nm2 t uses base:\n    |label = []\n",
    ] {
        let (resolved, diags) = compose(S, src, "m2", "t");
        assert!(diags.is_empty(), "{src}: {diags:?}");
        let body = resolved.unwrap().body;
        let label = body
            .entries
            .iter()
            .find(|e| matches!(&e.kind, BodyEntryKind::Modifier(m) if m.name.name == "label"))
            .expect("the composed modifier");
        let t_at = src.find("m2 t uses").unwrap();
        assert!(
            label.span.start > t_at,
            "the upper (empty) value wins: {src}"
        );
    }
}

#[test]
fn union_list_items_normalize_like_a_plain_lists_items() {
    // Block-shaped items at a union position had NO normalization
    // vocabulary (an Items supply names no variant): a nested
    // `xs = []` went unwarned and an array-spelled list field kept
    // its Property spelling, unlike the same items under a plain
    // `[]ub` field. They normalize under the first `List` variant's
    // element model — the resolver's own selection.
    const S: &str = "\
model ua:
    x string

model ub:
    kind string
    tags []string
    xs []string

model holder36:
    slot (ua | []ub)

model holder37:
    slot []ub
";
    for root in ["holder36", "holder37"] {
        let src = format!(
            "{root} base:\n    slot:\n        - w:\n            kind = \"k\"\n            tags = [\"a\"]\n\n\
                 {root} top uses base:\n    slot:\n        - w:\n            kind = \"k\"\n            tags = [\"b\"]\n            xs = []\n"
        );
        let (resolved, diags) = compose(S, &src, root, "top");
        assert_eq!(
            codes_of(&diags),
            [codes::ZERO_ITEM_LAYER_ENTRY],
            "{root}: {diags:?}"
        );
        let body = resolved.unwrap().body;
        let slot = sub_block(&body, "slot").expect("the list");
        let item = slot
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::ListItem(ListItem {
                    kind: ListItemKind::Named { body, .. },
                    ..
                }) => Some(body),
                _ => None,
            })
            .expect("the item");
        let tags = sub_block(item, "tags").expect("`tags` re-spelled as a block of items");
        assert_eq!(
            tags.entries
                .iter()
                .filter(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
                .count(),
            1,
            "{root}: {tags:?}"
        );
    }
}

#[test]
fn a_discarded_lists_items_normalize_under_their_own_variant() {
    // A displaced `[]ub` list under a winning `as ua`: the item's
    // `x` (a different union, `(r | s)`) diagnoses under ITS OWN
    // variant (RFC 0025 §4 — the subtraction's item interior), and a
    // dead body decides nothing: only the winner's `x` folds.
    const S: &str = "\
model p:
    pa string
    xs string

model q:
    qa string

model r:
    ra string
    xs []string

model s:
    sa string

model ua:
    x (p | q)

model ub:
    kind string
    x (r | s)

model holder38:
    slot (ua | []ub)
";
    let src = "\
holder38 base:
    slot:
        - w:
            kind = \"k\"
            x as r:
                ra = \"1\"
                xs = []

holder38 top uses base:
    slot as ua:
        x as p:
            pa = \"2\"
            xs = \"a\"
";
    FOLD_LOG.with(|l| l.borrow_mut().clear());
    let (resolved, diags) = compose(S, src, "holder38", "top");
    // The base item's `xs = []` is zero-item under `r` (a list); under
    // `p` (a string) it would be a value and the warning would vanish.
    assert_eq!(
        codes_of(&diags),
        [codes::ZERO_ITEM_LAYER_ENTRY],
        "{diags:?}"
    );
    assert_eq!(
        diags[0].span.map(|s| s.start),
        src.find("xs = []"),
        "the base item's entry"
    );
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ua")
    );
    let folded: Vec<String> = FOLD_LOG.with(|l| l.borrow().clone());
    assert!(
        folded.iter().any(|p| p == "slot.x"),
        "the winner's `x` folds: {folded:?}"
    );
    assert!(
        !folded.iter().any(|p| p.contains("slot[")),
        "a dead body decides nothing — the displaced list's items \
             are diagnosed, never folded: {folded:?}"
    );
}

#[test]
fn a_model_and_a_oneof_sharing_a_name_resolve_alike_on_every_pass() {
    // A colliding name (NML1000/NML2016 at schema load — composition
    // still runs over the loaded schema): the plan and normalization
    // resolved it model-first, the merge oneof-first, so a nested
    // union planned under `dup` the model merged under `dup` the
    // oneof's arm — a kinds mismatch, NML2086, a debug crash. One
    // resolution order (`resolve_ref`) on every pass.
    const S: &str = "\
model va:
    p string

model vb:
    q string

model arma:
    kind string
    slot (va | []vb)

model dup:
    slot (va | vb)

oneof dup by kind = \"a\":
    \"a\" -> arma

model ub:
    y string

model h40:
    pos (dup | ub)
    cfg dup
";
    for src in [
        "h40 base:\n    pos as dup:\n        slot as va:\n            p = \"1\"\n\n\
             h40 top uses base:\n    pos:\n        slot:\n",
        "h40 base:\n    cfg:\n        slot as va:\n            p = \"1\"\n\n\
             h40 top uses base:\n    cfg:\n        slot:\n",
    ] {
        let (resolved, diags) = compose(S, src, "h40", "top");
        assert!(
            !codes_of(&diags).contains(&codes::INTERNAL_COMPOSE_INVARIANT),
            "{src}: {diags:?}"
        );
        assert!(resolved.is_some(), "{src}");
    }
    // The ROOT too: the plan and the merge read a colliding root
    // oneof-first while normalization, the positionalizer and the
    // validator read the model — the identity merge under the model
    // never ran (a false "missing required field" on the composed
    // view). Model-first everywhere: the base item's `kind` survives
    // into the merged item.
    const ROOT: &str = "\
model vb:
    kind string
    q string

model arma:
    kind string
    note string

model dup2:
    slot []vb #identity

oneof dup2 by kind = \"a\":
    \"a\" -> arma
";
    let src = "\
dup2 base:
    slot:
        - w:
            kind = \"k\"
            q = \"1\"

dup2 top uses base:
    slot:
        - w:
            q = \"2\"
";
    let (resolved, diags) = compose(ROOT, src, "dup2", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    let item = first_named_item_body(sub_block(&body, "slot").expect("the list"));
    assert_eq!(
        scalar(item, "kind"),
        Some(&Value::String("k".into())),
        "{body:?}"
    );
    assert_eq!(scalar(item, "q"), Some(&Value::String("2".into())));
}

#[test]
fn set_variant_admits_the_empty_array_as_zero_item() {
    // `= []` at `(ua | set<string>)` is a warned no-op that keeps the
    // established body; an array literal under a set-first union
    // followed by a named switch composes clean (no list to judge).
    const S: &str = "\
model ua:
    x string

model holder44:
    slot (ua | set<string>)

model holder45:
    slot (set<string> | ua)
";
    let src = "holder44 base:\n    slot as ua:\n        x = \"1\"\n\nholder44 top uses base:\n    slot = []\n";
    let (resolved, diags) = compose(S, src, "holder44", "top");
    assert_eq!(
        codes_of(&diags),
        [codes::ZERO_ITEM_LAYER_ENTRY],
        "{diags:?}"
    );
    assert_eq!(
        nested_scalar(&resolved.unwrap().body, "slot", "x"),
        Some(&Value::String("1".into()))
    );
    let src = "holder45 base:\n    slot = [\"a\"]\n\nholder45 top uses base:\n    slot as ua:\n        x = \"1\"\n";
    let (resolved, diags) = compose(S, src, "holder45", "top");
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(
        slot_annotation(&resolved.unwrap().body).as_deref(),
        Some("ua")
    );
}

#[test]
fn zero_item_spellings_at_a_declared_list_modifier_are_one_predicate() {
    // Every spelling of "nothing" above a declared list modifier is
    // the same warned no-op: the base's item survives.
    const S: &str = "\
model m3:
    |xs []string
";
    for upper in ["    xs = []\n", "    xs:\n", "    |xs:\n", "    |xs = []\n"] {
        let src = format!("m3 base:\n    |xs = [\"a\"]\n\nm3 t uses base:\n{upper}");
        let (resolved, diags) = compose(S, &src, "m3", "t");
        assert_eq!(
            codes_of(&diags),
            [codes::ZERO_ITEM_LAYER_ENTRY],
            "{upper:?}: {diags:?}"
        );
        let body = resolved.unwrap().body;
        let count = body.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::Modifier(m) if m.name.name == "xs" => match &m.value {
                ModifierValue::Block(items) => Some(items.len()),
                ModifierValue::Inline(sv) => match &sv.value {
                    Value::Array(vs) => Some(vs.len()),
                    _ => None,
                },
                ModifierValue::TypeAnnotation { .. } => None,
            },
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "xs" => Some(
                nb.body
                    .entries
                    .iter()
                    .filter(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
                    .count(),
            ),
            _ => None,
        });
        assert_eq!(
            count,
            Some(1),
            "{upper:?}: the base item survives: {body:?}"
        );
    }
}

#[test]
fn oneof_and_union_element_items_normalize_like_model_element_items() {
    // Oneof- and union-element items had NO normalization vocabulary
    // (only a model element resolved): a nested `xs = []` went
    // unwarned and an array-spelled list kept its Property spelling
    // — under plain lists and union positions alike, and under the
    // block-modifier spelling. Every element kind is a peer now.
    const S: &str = "\
model ua:
    x string

model arma:
    kind string
    tags []string
    xs []string

model armb:
    kind string
    z string

oneof oo by kind = \"a\":
    \"a\" -> arma
    \"b\" -> armb

model ra:
    tags []string
    xs []string

model rb:
    y string

model h47:
    steps []oo

model h48:
    slot (ua | []oo)

model h49:
    steps [](ra | rb)

model h50:
    slot (ua | [](ra | rb))

model h51:
    |steps []arma
";
    let cases = [
        ("h47", "steps", "- w:\n            kind = \"a\"", false),
        ("h48", "slot", "- w:\n            kind = \"a\"", false),
        ("h49", "steps", "- w as ra:", false),
        ("h50", "slot", "- w as ra:", false),
        ("h51", "steps", "- w:\n            kind = \"a\"", true),
    ];
    for (root, field, item, modifier) in cases {
        let spell = |extra: &str| {
            let head = if modifier {
                format!("    |{field}:")
            } else {
                format!("    {field}:")
            };
            format!("{head}\n        {item}\n            tags = [\"a\"]{extra}\n")
        };
        let src = format!(
            "{root} base:\n{}\n{root} top uses base:\n{}",
            spell(""),
            spell("\n            xs = []")
        );
        let (resolved, diags) = compose(S, &src, root, "top");
        assert_eq!(
            codes_of(&diags),
            [codes::ZERO_ITEM_LAYER_ENTRY],
            "{root}: {diags:?}"
        );
        let body = resolved.unwrap().body;
        let item_body: &Body = if modifier {
            body.entries
                .iter()
                .find_map(|e| match &e.kind {
                    BodyEntryKind::Modifier(m) if m.name.name == field => match &m.value {
                        ModifierValue::Block(items) => items.iter().find_map(|i| match &i.kind {
                            ListItemKind::Named { body, .. } => Some(body),
                            _ => None,
                        }),
                        _ => None,
                    },
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{root}: the modifier's item: {body:?}"))
        } else {
            first_named_item_body(sub_block(&body, field).expect("the list"))
        };
        let tags = sub_block(item_body, "tags")
            .unwrap_or_else(|| panic!("{root}: `tags` re-spelled as a block: {item_body:?}"));
        assert_eq!(
            tags.entries
                .iter()
                .filter(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
                .count(),
            1,
            "{root}: {tags:?}"
        );
    }
}

#[test]
fn union_list_items_normalize_under_the_first_list_variant_only() {
    // Two list variants: items resolve to the FIRST `List` everywhere
    // (the validator's reading), so `tags = ["b"]` is ub's list, not
    // uc's string.
    const S: &str = "\
model ua:
    x string

model ub:
    kind string
    tags []string

model uc:
    kind string
    tags string

model h54:
    slot (ua | []ub | []uc)
";
    let src = "\
h54 base:
    slot:
        - w:
            kind = \"k\"
            tags = [\"a\"]

h54 top uses base:
    slot:
        - w:
            kind = \"k\"
            tags = [\"b\"]
";
    let (resolved, diags) = compose(S, src, "h54", "top");
    assert!(diags.is_empty(), "{diags:?}");
    let body = resolved.unwrap().body;
    let item = first_named_item_body(sub_block(&body, "slot").expect("the list"));
    assert!(
        sub_block(item, "tags").is_some(),
        "ub's list, re-spelled: {item:?}"
    );
}

#[test]
fn a_colliding_name_is_judged_model_first_at_every_vocab_site() {
    // The seal lives on the MODEL `dup`; a list element `[]dup` and a
    // union element `(dup | ub)` are both judged under it (the
    // validator's reading) — never the oneof of the same name.
    const S: &str = "\
model arma:
    kind string

oneof dup by kind = \"a\":
    \"a\" -> arma

model dup:
    kind string
    secret string #sealed

model ua:
    x string

model ub:
    y string

model h59:
    slot (ua | []dup)

model h60:
    slot (ua | [](dup | ub))
";
    for (root, item) in [("h59", "- w:"), ("h60", "- w as dup:")] {
        let src = format!(
            "{root} base:\n    slot:\n        {item}\n            kind = \"k\"\n            secret = \"s\"\n\n\
                 {root} top uses base:\n    slot as ua:\n        x = \"1\"\n"
        );
        let (resolved, diags) = compose(S, &src, root, "top");
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
        assert_eq!(
            slot_annotation(&resolved.unwrap().body),
            None,
            "{root}: the switch was rejected, the list survives"
        );
    }
}

#[test]
fn block_form_empty_modifier_draws_nml2079() {
    // The one zero-item spelling that escaped: `|deny:` with no items
    // — "always diagnosed, never silently ignored" admits no spelling
    // exception.
    let schema = "\
model m:
    |deny []string #append
";
    let src = "\
m base:
    |deny:
        - \"a\"

m t uses base:
    |deny:
";
    let (resolved, diags) = compose(schema, src, "m", "t");
    assert!(
        codes_of(&diags).contains(&codes::ZERO_ITEM_LAYER_ENTRY),
        "block-form empty modifier is a warned no-op: {diags:?}"
    );
    let body = resolved.unwrap().body;
    let items = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::Modifier(m) if m.name.name == "deny" => match &m.value {
                ModifierValue::Block(items) => Some(items.len()),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or(0);
    assert_eq!(items, 1, "and the base's items survive: {body:?}");
}
