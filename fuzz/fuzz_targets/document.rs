//! Fuzz the full document parse: never panics, and the lossless CST
//! invariant holds — the tree's text is byte-identical to the source
//! (RFC 0004), for every input including hostile numeric literals — every
//! span a node carries holds the invariant its declared SHAPE names —
//! and lowering is TOTAL: on a document with no error, every entry node of
//! the CST lowers to one AST entry (a silent drop was content loss: the
//! formatter rewrites from the lowered tree).

#![no_main]

use libfuzzer_sys::fuzz_target;
use nml_core::ast::{
    ArmTarget, Body, BodyEntryKind, DeclarationKind, File, ListItem, ListItemKind, Modifier,
    ModifierValue, SharedPropertyKind,
};
use nml_core::cst::SyntaxKind;
use nml_core::diagnostic::Severity;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let parse = nml_core::cst::parse(s);
    assert_eq!(
        parse.syntax().text().to_string(),
        s,
        "lossless CST invariant violated"
    );
    // The semantic pipeline is input-reachable too (nudge decodes tenant
    // config): the source-character policy, lowering, and the TOTAL value
    // decoders (U+FFFD escape recovery, multiline shape rules, line
    // continuation) must never panic, and the findings list stays bounded
    // (RFC 0009 exact-count honesty caps every collection site).
    let (file, diags) = nml_core::cst::parse_to_ast_all(s);
    assert!(
        diags.len() <= 512,
        "diagnostics not bounded: {}",
        diags.len()
    );
    // The span-site invariant in its exact form, for EVERY input (erroneous
    // documents included), judged by the SHAPE each emitter declares
    // (`nml_core::span::SpanShape`) and never by the site's name — a name
    // is a label, and reading a rule off one held `Arm.selector_content`, a
    // content window whose field is not spelled `.content`, to the rule for
    // whole spans. A whole span is TOKEN-ALIGNED — it begins at a
    // significant token's first byte and ends at one's last byte — so a
    // diagnostic anchored on a node never renders at its indentation or
    // past its last line. A content window is on character boundaries (an
    // applier splices it; RFC 0026 decision 2) AND strictly inside its
    // token, which is what keeps a replacement off a literal's delimiters.
    // A template expression is inside its string token; its exact `{{…}}`
    // bytes are pinned on terminated documents by `content_spans`.
    let boundaries = parse.token_boundaries();
    let mut judged = |site: nml_core::span::SpanSite| {
        if let Err(why) = boundaries.check(site, s) {
            panic!("{} span {:?} {why}", site.kind, site.span);
        }
    };
    nml_core::ast::for_each_span(&file, &mut judged);
    let (_, schema, _, _) = nml_core::cst::parse_and_extract_split(s);
    nml_core::schema::for_each_span(&schema, &mut judged);
    // Lowering totality: a document with no error lowers every CST entry
    // node — a property, a nested block, a modifier, a shared property, a
    // list item, a field definition, a routing arm — to one AST entry.
    // Recovery legitimately leaves entries out of an erroneous document,
    // so the invariant is scoped to clean ones, as the formatter's
    // round-trip promise is scoped to well-formed input.
    if diags.iter().all(|d| d.severity != Severity::Error) {
        let in_tree = parse
            .syntax()
            .descendants()
            .filter(|n| {
                matches!(
                    n.kind(),
                    SyntaxKind::Property
                        | SyntaxKind::NestedBlock
                        | SyntaxKind::Modifier
                        | SyntaxKind::SharedProperty
                        | SyntaxKind::ListItem
                        | SyntaxKind::FieldDef
                        | SyntaxKind::Arm
                )
            })
            .count();
        let lowered = ast_entries(&file);
        assert_eq!(
            in_tree,
            lowered,
            "lowering dropped {} entry node(s) of a clean document",
            in_tree.saturating_sub(lowered)
        );
    }
});

/// Every entry of the lowered tree, recursively — the AST twin of the
/// CST count above.
fn ast_entries(file: &File) -> usize {
    file.declarations
        .iter()
        .map(|d| match &d.kind {
            DeclarationKind::Block(b) => body(&b.body),
            DeclarationKind::Array(a) => {
                a.body.modifiers.iter().map(modifier).sum::<usize>()
                    + a.body
                        .shared_properties
                        .iter()
                        .map(|s| shared(&s.kind))
                        .sum::<usize>()
                    + a.body.properties.len()
                    + a.body.items.iter().map(item).sum::<usize>()
            }
            DeclarationKind::Const(_) | DeclarationKind::Template(_) | DeclarationKind::OneOf(_) => {
                0
            }
        })
        .sum()
}

fn body(b: &Body) -> usize {
    b.entries
        .iter()
        .map(|e| match &e.kind {
            BodyEntryKind::Property(_) | BodyEntryKind::FieldDefinition(_) => 1,
            BodyEntryKind::NestedBlock(n) => 1 + body(&n.body),
            BodyEntryKind::Modifier(m) => modifier(m),
            BodyEntryKind::SharedProperty(s) => shared(&s.kind),
            BodyEntryKind::ListItem(l) => item(l),
            BodyEntryKind::Arm(a) => {
                1 + match &a.target {
                    ArmTarget::Inline { body: b, .. } => body(b),
                    _ => 0,
                }
            }
        })
        .sum()
}

fn modifier(m: &Modifier) -> usize {
    1 + match &m.value {
        ModifierValue::Block(items) => items.iter().map(item).sum(),
        _ => 0,
    }
}

fn shared(s: &SharedPropertyKind) -> usize {
    1 + match s {
        SharedPropertyKind::Block(b) => body(b),
        SharedPropertyKind::Scalar(_) => 0,
    }
}

fn item(l: &ListItem) -> usize {
    1 + match &l.kind {
        ListItemKind::Named { body: b, .. } => body(b),
        ListItemKind::Shorthand { body: Some(b), .. } => body(b),
        _ => 0,
    }
}
