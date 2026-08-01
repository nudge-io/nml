//! CST-driven duration literal tooling (RFC 0017 §10).
//!
//! Every surface here derives from the [`nml_core::cst::duration_literal_at`]
//! query seam and works purely over token spans — no line-string surgery, so
//! positions stay correct across tabs, spaced compounds (`1h 30m`), and
//! non-ASCII text earlier on the line.

use nml_core::cst::ast::AstNode;
use nml_core::cst::{self, Parse, SyntaxKind, SyntaxToken};
use nml_core::duration::{Duration, DurationUnit};
use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Range};

use crate::position::LineIndex;

/// Typed digits and edit ranges for unit-suffix completion.
///
/// `digits` is the magnitude verbatim — separators preserved, so accepting
/// a completion on `1_000` keeps the author's grouping. `insert` covers
/// exactly the digits before the cursor; `replace` covers the whole
/// magnitude+suffix pair, so re-triggering inside an existing `30s` swaps
/// the suffix instead of stacking a second one. Both ranges contain the
/// cursor (LSP completion-item contract). `units` is never empty.
pub struct DurationUnitContext {
    pub digits: String,
    /// Attached text of the complete segments before the one being
    /// completed (`"1h"` when completing the `30` of `1h30`); empty for a
    /// bare number. Lets the preview show the composed literal.
    pub prefix: String,
    pub insert: Range,
    pub replace: Range,
    pub units: Vec<DurationUnit>,
}

/// Ranged hover for a duration literal at `pos`, if any.
///
/// Renders the author's spelling, the component breakdown with its human
/// respelling (`1h + 30m = 90m`), and the machine-comparable total
/// (`5_400_000ms total`) — human reading first, precise data second.
pub fn duration_hover(
    parse: &Parse,
    source: &str,
    pos: Position,
    line_index: &LineIndex,
) -> Option<Hover> {
    let byte = line_index.offset(pos);
    let at = cst::duration_literal_at(parse, byte)?;
    if at.sign.is_some() {
        // A signed literal is domain-invalid (durations are unsigned): a
        // hover presenting the positive total under a `-5s` spelling would
        // mislead. The diagnostic explanation hover teaches instead.
        return None;
    }
    let span = at.display_span();
    // Parse from the tokens (trivia-immune); display the author's spelling.
    let d = Duration::parse_text(&at.literal.attached_text()).ok()?;
    let literal_text = &source[span.start..span.end];
    let mut text = format!("**duration** `{literal_text}`");
    if d.segments().len() > 1 {
        let parts: Vec<String> = d
            .segments()
            .iter()
            .map(|s| format!("{}{}", s.magnitude, s.unit.suffix()))
            .collect();
        text.push_str(&format!("\n\n{}", parts.join(" + ")));
        if let Some(coarse) = d.coarsest_exact() {
            text.push_str(&format!(" = {coarse}"));
        }
    }
    if let Some(total) = d.normalized_total() {
        text.push_str(&format!("\n\n{total} total"));
    }
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: text,
        }),
        range: Some(line_index.range(span)),
    })
}

/// Document highlight for the whole literal when the cursor is inside one.
pub fn duration_highlight_range(
    parse: &Parse,
    pos: Position,
    line_index: &LineIndex,
) -> Option<Range> {
    let byte = line_index.offset(pos);
    let at = cst::duration_literal_at(parse, byte)?;
    Some(line_index.range(at.display_span()))
}

/// CST-driven unit completion at `pos`. `value_allows_duration` gates bare
/// `Number` tokens in config value positions; facet literals and
/// mid-compound segments need no schema walk.
pub fn find_duration_unit_completions_at(
    parse: &Parse,
    pos: Position,
    line_index: &LineIndex,
    value_allows_duration: impl FnOnce() -> bool,
) -> Option<DurationUnitContext> {
    let byte = line_index.offset(pos);
    if let Some(at) = cst::duration_literal_at(parse, byte) {
        return completion_in_literal(&at, byte, line_index);
    }
    let root = parse.syntax();
    let tok = number_token_near_offset(&root, byte)?;
    if tok
        .parent()
        .is_some_and(|p| p.kind() == SyntaxKind::DurationLiteral)
    {
        return None;
    }
    if has_leading_dash(&tok) {
        // Durations are unsigned: `-5` can never grow into a valid literal,
        // and a completion must never insert an instant diagnostic.
        return None;
    }
    let parent = tok.parent()?;
    if facet_allows_duration(&parent) || value_allows_duration() {
        context_from_tokens(
            &tok,
            None,
            byte,
            line_index,
            DurationUnit::ALL.to_vec(),
            String::new(),
        )
    } else {
        None
    }
}

fn completion_in_literal(
    at: &cst::DurationLiteralAt,
    byte: usize,
    line_index: &LineIndex,
) -> Option<DurationUnitContext> {
    let components = at.literal.components();
    let active = at.active;
    let (mag, unit) = components.get(active)?;
    let pair_end = unit
        .as_ref()
        .map(|u| usize::from(u.text_range().end()))
        .unwrap_or_else(|| usize::from(mag.text_range().end()));
    if unit.is_some() && byte >= pair_end {
        // Segment already complete and the cursor is past it — nothing to
        // swap. (A dangling magnitude keeps offering at its end: that IS
        // the mid-compound typing position.)
        return None;
    }
    let units = if unit.is_some() {
        DurationUnit::ALL.to_vec()
    } else {
        units_after_dangling(&components, active)?
    };
    // Preview/filter only make sense when completing the trailing segment;
    // an interior swap leaves following segments the preview cannot show.
    let prefix = if active + 1 == components.len() {
        attached_prefix(&components[..active])
    } else {
        String::new()
    };
    context_from_tokens(mag, unit.as_ref(), byte, line_index, units, prefix)
}

/// Build the completion context from the magnitude (and optional stale
/// unit) tokens. Returns `None` when the cursor lies outside the pair —
/// an LSP text edit must contain the requested position.
fn context_from_tokens(
    mag: &SyntaxToken,
    unit: Option<&SyntaxToken>,
    byte: usize,
    line_index: &LineIndex,
    units: Vec<DurationUnit>,
    prefix: String,
) -> Option<DurationUnitContext> {
    // Verbatim: `1_000` completes to `1_000s`, preserving the author's
    // separators (the decoder accepts them; stripping would rewrite them).
    let digits = mag.text().to_string();
    if !digits.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    let mag_start = usize::from(mag.text_range().start());
    let replace_end = unit
        .map(|u| usize::from(u.text_range().end()))
        .unwrap_or_else(|| usize::from(mag.text_range().end()));
    if byte < mag_start || byte > replace_end {
        return None;
    }
    let start = line_index.position(mag_start);
    // Both ends derive from the clamped byte offset — a client position
    // past the end of the line must not produce an edit range past the
    // replace range.
    let insert = Range::new(start, line_index.position(byte));
    let replace = Range::new(start, line_index.position(replace_end));
    Some(DurationUnitContext {
        digits,
        prefix,
        insert,
        replace,
        units,
    })
}

/// Attached text of the complete leading segments (`[1h, 30m]` → `"1h30m"`).
fn attached_prefix(components: &[(SyntaxToken, Option<SyntaxToken>)]) -> String {
    let mut out = String::new();
    for (mag, unit) in components {
        out.push_str(mag.text());
        if let Some(u) = unit {
            out.push_str(u.text());
        }
    }
    out
}

/// Units to offer for a dangling magnitude: strictly finer than the
/// previous segment's unit, minus any unit already spelled in the literal
/// (a repeat would be NML3007).
fn units_after_dangling(
    components: &[(SyntaxToken, Option<SyntaxToken>)],
    active: usize,
) -> Option<Vec<DurationUnit>> {
    let Some(prev) = active.checked_sub(1) else {
        return Some(DurationUnit::ALL.to_vec());
    };
    let (_, Some(prev_unit_tok)) = &components[prev] else {
        return None;
    };
    let prev_unit = DurationUnit::from_suffix(prev_unit_tok.text())?;
    let used: Vec<DurationUnit> = components
        .iter()
        .filter_map(|(_, u)| u.as_ref().and_then(|t| DurationUnit::from_suffix(t.text())))
        .collect();
    let finer: Vec<DurationUnit> = prev_unit
        .finer()
        .iter()
        .copied()
        .filter(|u| !used.contains(u))
        .collect();
    (!finer.is_empty()).then_some(finer)
}

fn facet_allows_duration(node: &nml_core::cst::SyntaxNode) -> bool {
    use nml_core::cst::ast::{self};
    let mut current = Some(node.clone());
    while let Some(n) = current {
        if let Some(facet) = ast::Facet::cast(n.clone()) {
            return facet_in_duration_type(&facet);
        }
        current = n.parent();
    }
    false
}

fn facet_in_duration_type(facet: &nml_core::cst::ast::Facet) -> bool {
    use nml_core::cst::ast::{self};
    let mut node = facet.syntax().parent();
    while let Some(n) = node {
        if let Some(te) = ast::TypeExpr::cast(n.clone()) {
            return te.name().is_some_and(|name| name.text() == "duration");
        }
        node = n.parent();
    }
    false
}

/// Whether a `Dash` token (trivia-skipping) directly precedes `tok` — the
/// facet/value sign spelling, which rules out a duration.
fn has_leading_dash(tok: &SyntaxToken) -> bool {
    let mut prev = tok.prev_sibling_or_token();
    while let Some(el) = prev {
        if el.kind().is_trivia() {
            prev = el.prev_sibling_or_token();
            continue;
        }
        return el.kind() == SyntaxKind::Dash;
    }
    false
}

fn number_token_near_offset(
    root: &nml_core::cst::SyntaxNode,
    byte: usize,
) -> Option<nml_core::cst::SyntaxToken> {
    for probe in [byte, byte.saturating_sub(1)] {
        let Some(tok) = root.token_at_offset((probe as u32).into()).left_biased() else {
            continue;
        };
        if tok.kind() == SyntaxKind::Number {
            return Some(tok);
        }
    }
    None
}

/// Whether the composed literal (`prefix + digits + suffix`) is a valid,
/// in-domain duration — items failing this would insert an instant
/// diagnostic, so the handler withholds them.
pub fn completion_is_valid(ctx: &DurationUnitContext, suffix: &str) -> bool {
    Duration::parse_text(&format!("{}{}{suffix}", ctx.prefix, ctx.digits)).is_ok()
}

/// Preview for completion `label_details`: the composed literal's canonical
/// spelling (`= 1h30m` when accepting `m` after `1h30`). `None` when it
/// would merely echo the inserted text.
pub fn completion_preview(ctx: &DurationUnitContext, suffix: &str) -> Option<String> {
    let literal = format!("{}{}{suffix}", ctx.prefix, ctx.digits);
    let canonical = Duration::parse_text(&literal).ok()?.to_string();
    // Echo check ignores separators: `1_000` + `ms` canonicalizes to
    // `1000ms` — same literal, not worth a preview.
    let echo: String = format!("{}{suffix}", ctx.digits)
        .chars()
        .filter(|c| *c != '_')
        .collect();
    (canonical != echo).then_some(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nml_core::SchemaIndex;

    fn field_index(src: &str) -> SchemaIndex {
        let (s, diags) = nml_core::cst::extract_schema(src);
        assert!(diags.is_empty(), "{diags:?}");
        SchemaIndex::build(s.models, s.enums, s.oneofs)
    }

    fn allows_duration(
        file: &nml_core::ast::File,
        source: &str,
        pos: Position,
        index: &SchemaIndex,
        li: &LineIndex,
    ) -> bool {
        use crate::server::{governs_duration, value_governors_at, value_position_prop_name};
        value_position_prop_name(source, pos)
            .map(|prop| {
                value_governors_at(file, pos, index, li, prop)
                    .fields
                    .iter()
                    .any(|f| governs_duration(&f.field_type))
            })
            .unwrap_or(false)
    }

    #[test]
    fn mid_compound_completion_in_value() {
        let idx = field_index("model service:\n    timeout duration?\n    port number?\n");
        let src = "service Api:\n    timeout = 1h30\n";
        let (file, parse) = nml_core::cst::parse_best_effort_with_tree(src);
        let li = LineIndex::new(src);
        let pos = Position::new(1, 18);
        let ctx = find_duration_unit_completions_at(&parse, pos, &li, || {
            allows_duration(&file, src, pos, &idx, &li)
        })
        .expect("mid-compound");
        assert_eq!(ctx.digits, "30");
        assert_eq!(ctx.prefix, "1h");
        assert!(ctx.units.contains(&DurationUnit::Minutes));
        assert!(!ctx.units.contains(&DurationUnit::Hours));
        assert_eq!(
            completion_preview(&ctx, "m").as_deref(),
            Some("1h30m"),
            "preview composes the whole literal"
        );
    }

    #[test]
    fn facet_bare_number_completion() {
        let idx = field_index("model M:\n    timeout duration(min = 1s)\n");
        let src = "model X:\n    timeout duration(min = 5)\n";
        let (file, parse) = nml_core::cst::parse_best_effort_with_tree(src);
        let li = LineIndex::new(src);
        let pos = Position::new(1, 28);
        let ctx = find_duration_unit_completions_at(&parse, pos, &li, || {
            allows_duration(&file, src, pos, &idx, &li)
        })
        .expect("facet bare number");
        assert_eq!(ctx.digits, "5");
        assert!(
            completion_preview(&ctx, "s").is_none(),
            "single-segment preview would echo the label"
        );
    }

    #[test]
    fn mid_digit_trigger_replaces_the_whole_magnitude() {
        let idx = field_index("model service:\n    timeout duration?\n");
        let src = "service Api:\n    timeout = 30\n";
        let (file, parse) = nml_core::cst::parse_best_effort_with_tree(src);
        let li = LineIndex::new(src);
        // Cursor between `3` and `0`.
        let pos = Position::new(1, 15);
        let ctx = find_duration_unit_completions_at(&parse, pos, &li, || {
            allows_duration(&file, src, pos, &idx, &li)
        })
        .expect("mid-digit trigger");
        assert_eq!(ctx.digits, "30");
        assert_eq!(ctx.replace.end.character, 16, "replace covers all digits");
        assert_eq!(ctx.insert.end.character, 15, "insert ends at the cursor");
    }

    #[test]
    fn retrigger_inside_existing_suffix_swaps_it() {
        let src = "service Api:\n    timeout = 30s\n";
        let (_file, parse) = nml_core::cst::parse_best_effort_with_tree(src);
        let li = LineIndex::new(src);
        // Cursor between `30` and `s`.
        let pos = Position::new(1, 16);
        let ctx =
            find_duration_unit_completions_at(&parse, pos, &li, || true).expect("suffix retrigger");
        assert_eq!(ctx.digits, "30");
        assert_eq!(
            ctx.replace.end.character, 17,
            "replace covers the stale suffix"
        );
        assert_eq!(ctx.units, DurationUnit::ALL.to_vec());
    }

    #[test]
    fn cursor_past_complete_segment_offers_nothing() {
        let src = "service Api:\n    timeout = 30s\n";
        let (_file, parse) = nml_core::cst::parse_best_effort_with_tree(src);
        let li = LineIndex::new(src);
        // Cursor after the `s` — the segment is done.
        let pos = Position::new(1, 17);
        assert!(find_duration_unit_completions_at(&parse, pos, &li, || true).is_none());
    }

    #[test]
    fn out_of_domain_literal_is_not_valid() {
        let src = "service Api:\n    timeout = 99999999999999999999\n";
        let (_file, parse) = nml_core::cst::parse_best_effort_with_tree(src);
        let li = LineIndex::new(src);
        let pos = Position::new(1, 37);
        let ctx = find_duration_unit_completions_at(&parse, pos, &li, || true)
            .expect("bare number context");
        assert!(
            !completion_is_valid(&ctx, "h"),
            "an out-of-domain literal must be withheld"
        );
    }

    #[test]
    fn separator_digits_complete_verbatim() {
        // `1_000` must complete to `1_000ms` — never rewrite the author's
        // digit grouping.
        let src = "service Api:\n    timeout = 1_000\n";
        let (_file, parse) = nml_core::cst::parse_best_effort_with_tree(src);
        let li = LineIndex::new(src);
        let ctx = find_duration_unit_completions_at(&parse, Position::new(1, 19), &li, || true)
            .expect("separator digits");
        assert_eq!(ctx.digits, "1_000");
        assert!(completion_is_valid(&ctx, "ms"));
        assert!(
            completion_preview(&ctx, "ms").is_none(),
            "a separator-only difference is an echo, not a preview"
        );
    }

    #[test]
    fn past_end_of_line_position_clamps_the_insert_range() {
        // A client column past the end of the line must not produce an
        // edit range wider than the replace range.
        let src = "service Api:\n    timeout = 30\n";
        let (_file, parse) = nml_core::cst::parse_best_effort_with_tree(src);
        let li = LineIndex::new(src);
        let ctx = find_duration_unit_completions_at(&parse, Position::new(1, 99), &li, || true)
            .expect("clamped position");
        assert_eq!(ctx.insert.end.character, 16);
        assert_eq!(ctx.replace.end.character, 16);
    }

    #[test]
    fn number_facet_offers_no_units() {
        // The facet gate is the carrier type, not the facet name: a
        // `number(min = …)` bound must not grow duration suffixes.
        let src = "model M:\n    port number(min = 5)\n";
        let (_file, parse) = nml_core::cst::parse_best_effort_with_tree(src);
        let li = LineIndex::new(src);
        assert!(
            find_duration_unit_completions_at(&parse, Position::new(1, 24), &li, || false)
                .is_none()
        );
    }

    #[test]
    fn signed_bare_number_offers_no_units() {
        // Durations are unsigned — completing `-5` into `-5s` would insert
        // an instant diagnostic.
        let src = "model M:\n    timeout duration(min = -5)\n";
        let (_file, parse) = nml_core::cst::parse_best_effort_with_tree(src);
        let li = LineIndex::new(src);
        assert!(
            find_duration_unit_completions_at(&parse, Position::new(1, 29), &li, || true).is_none()
        );
    }

    #[test]
    fn signed_literal_gets_no_hover_but_a_full_highlight() {
        // `-5s` is domain-invalid: a hover would present the positive total
        // under a negative spelling. The highlight still covers the sign —
        // that is what the reader perceives as the literal.
        let src = "model M:\n    timeout duration(min = -5s)\n";
        let (_file, parse) = nml_core::cst::parse_best_effort_with_tree(src);
        let li = LineIndex::new(src);
        let pos = Position::new(1, 29);
        assert!(duration_hover(&parse, src, pos, &li).is_none());
        let range = duration_highlight_range(&parse, pos, &li).expect("signed highlight");
        assert_eq!((range.start.character, range.end.character), (27, 30));
    }

    #[test]
    fn hover_survives_tab_separated_compound() {
        // Tabs are legal inter-token trivia; the hover parses from the
        // tokens, never from whitespace-normalized source text.
        let src = "service Api:\n    timeout = 1h\t30m\n";
        let (_file, parse) = nml_core::cst::parse_best_effort_with_tree(src);
        let li = LineIndex::new(src);
        let hover =
            duration_hover(&parse, src, Position::new(1, 15), &li).expect("tab compound hovers");
        let HoverContents::Markup(m) = hover.contents else {
            panic!("markdown hover");
        };
        assert!(m.value.contains("1h + 30m"), "{}", m.value);
    }
}
