use super::super::*;
use super::*;

#[test]
fn sealed_composes_with_nothing_2068() {
    let diags = validate_merge_policies(&index_from("model m:\n    xs []string #sealed #append\n"));
    assert_eq!(codes_of(&diags), [codes::INVALID_MERGE_POLICY]);
}

#[test]
fn identity_on_scalar_list_and_set_2068() {
    let diags = validate_merge_policies(&index_from(
        "model m:\n    xs []string #identity\n    ys set<string> #identity\n",
    ));
    assert_eq!(
        codes_of(&diags),
        [codes::INVALID_MERGE_POLICY, codes::INVALID_MERGE_POLICY]
    );
}

#[test]
fn list_policy_on_scalar_field_2068() {
    let diags = validate_merge_policies(&index_from("model m:\n    x string #append\n"));
    assert_eq!(codes_of(&diags), [codes::INVALID_MERGE_POLICY]);
}

#[test]
fn sealed_with_default_lints_2076() {
    let diags = validate_merge_policies(&index_from("model m:\n    order number = 100 #sealed\n"));
    assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL]);
}

#[test]
fn bare_overlay_list_with_sealed_items_lints_2076() {
    let diags = validate_merge_policies(&index_from(
        "model step:\n    name string+\n    action string #sealed\n\nmodel flow:\n    steps []step\n",
    ));
    assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL]);
}

#[test]
fn oneof_with_sealed_arm_and_unsealed_discriminator_lints_2076() {
    let diags = validate_merge_policies(&index_from(ONEOF_SCHEMA));
    assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL]);
}

#[test]
fn identity_list_of_models_is_clean() {
    let diags = validate_merge_policies(&index_from(PAIR_SCHEMA));
    // PAIR_SCHEMA's `action #sealed` under an identity-granted list is
    // exactly the reachable-seal shape — no lint.
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn lint_2076_reaches_list_item_seals() {
    let schema = "\
model step:
    name string+
    action string #sealed

model az:
    kind string
    steps []step #identity

oneof notify by kind:
    \"az\" -> az
";
    let diags = validate_merge_policies(&index_from(schema));
    assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::UNREACHABLE_SEAL)
                    && d.message.contains("discriminator")),
            "oneof lint must see seals on item models: {diags:?}"
        );
}

#[test]
fn oneof_field_reference_lints_2076_exactly_once() {
    let schema = "\
model azR:
    kind string
    key string #sealed

oneof relay by kind = \"az\":
    \"az\" -> azR

model svc:
    out relay
";
    let index = index_from(schema);
    let diags = validate_merge_policies(&index);
    let n = diags
        .iter()
        .filter(|d| d.code == Some(codes::UNREACHABLE_SEAL))
        .count();
    assert_eq!(n, 1, "one schema defect, one warning: {diags:?}");
}

#[test]
fn bare_overlay_oneof_element_list_lints_2076() {
    let schema = "\
model spArm:
    ikind string
    secret string #sealed

oneof istep by ikind = \"sp\":
    \"sp\" -> spArm

model flow:
    steps []istep
";
    let index = index_from(schema);
    let diags = validate_merge_policies(&index);
    assert!(
        diags
            .iter()
            .any(|d| d.code == Some(codes::UNREACHABLE_SEAL) && d.message.contains("bare-overlay")),
        "sealed arms of a oneof ELEMENT are seals too: {diags:?}"
    );
}

// ── round-7 review pins ──────────────────────────────────────────────

#[test]
fn bare_overlay_union_element_and_list_variant_lint_2076() {
    // The unreachable-seal lint sees union ELEMENTS (`[](a|b)`) and
    // a union's LIST VARIANT (`(a | []b)`) — with honest advice for
    // each (`#identity` is not grantable at either position).
    let diags = validate_merge_policies(&index_from(
        "model ua:\n    s string #sealed\n\nmodel ub:\n    q string\n\n\
             model m:\n    xs [](ua | ub)\n",
    ));
    assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL]);
    assert!(diags[0].message.contains("'ua'"), "{}", diags[0].message);

    let diags = validate_merge_policies(&index_from(
        "model ua:\n    x string\n\nmodel ub:\n    s string #sealed\n\n\
             model m:\n    slot (ua | []ub)\n",
    ));
    assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL]);
    assert!(
        diags[0]
            .message
            .contains("not grantable at a union list position")
            && diags[0].message.contains("list variant `[]ub`"),
        "honest advice with the list-variant lead, not a dead end: {}",
        diags[0].message
    );
    // A set variant AHEAD of the list variant is skipped: the lead
    // names the variant the backstop judges under.
    let diags = validate_merge_policies(&index_from(
        "model ua:\n    x string\n\nmodel ub:\n    s string #sealed\n\n\
             model m:\n    slot (ua | set<string> | []ub)\n",
    ));
    assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL]);
    assert!(
        diags[0].message.contains("list variant `[]ub`"),
        "{}",
        diags[0].message
    );
}

#[test]
fn union_identity_element_gets_the_union_wording_of_2068() {
    let diags = validate_merge_policies(&index_from(
        "model ua:\n    x string\n\nmodel ub:\n    y string\n\n\
             model m:\n    xs [](ua | ub) #identity\n",
    ));
    assert_eq!(codes_of(&diags), [codes::INVALID_MERGE_POLICY]);
    assert!(
        diags[0].message.contains("identity across variants"),
        "union-specific wording: {}",
        diags[0].message
    );
}

#[test]
fn the_first_list_variant_lints_wherever_it_sits() {
    for schema in [
        "model ua:\n    x string\n\nmodel ub:\n    s string #sealed\n\nmodel m:\n    slot (ua | string | []ub)\n",
        "model ua:\n    x string\n\nmodel ub:\n    s string #sealed\n\nmodel m:\n    slot (ua | set<ua> | []ub)\n",
    ] {
        let diags = validate_merge_policies(&index_from(schema));
        assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL], "{schema}");
        assert!(
            diags[0].message.contains("list variant `[]ub`"),
            "{}",
            diags[0].message
        );
    }
}
