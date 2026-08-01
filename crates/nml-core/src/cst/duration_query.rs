//! Position queries for [`SyntaxKind::DurationLiteral`] nodes — the single
//! seam every duration-aware LSP surface derives from.

use crate::cst::Parse;
use crate::cst::ast::{self, AstNode};
use crate::cst::syntax::{SyntaxKind, SyntaxNode, SyntaxToken, node_span, token_span};
use crate::span::Span;
use rowan::TextSize;

/// A duration literal at a source offset, with per-component spans.
#[derive(Debug, Clone)]
pub struct DurationLiteralAt {
    pub literal: ast::DurationLiteral,
    pub span: Span,
    pub components: Vec<(Span, Option<Span>)>,
    pub active: usize,
    /// A leading `-` directly before the literal (facet sign — the `Dash`
    /// token lives on the owner node, outside the literal).
    pub sign: Option<Span>,
}

impl DurationLiteralAt {
    /// Span covering only magnitude/unit tokens — excludes surrounding trivia.
    pub fn tight_span(&self) -> Span {
        let Some((first, _)) = self.components.first() else {
            return self.span;
        };
        let (last_mag, last_unit) = self.components.last().expect("non-empty");
        let end = last_unit.map(|u| u.end).unwrap_or(last_mag.end);
        Span::new(first.start, end)
    }

    /// [`Self::tight_span`] widened to include a leading sign — what a
    /// reader perceives as "the literal" (`-5s`, not `5s`).
    pub fn display_span(&self) -> Span {
        let tight = self.tight_span();
        match self.sign {
            Some(sign) => Span::new(sign.start, tight.end),
            None => tight,
        }
    }
}

/// The duration literal containing `offset`, if any.
pub fn duration_literal_at(parse: &Parse, offset: usize) -> Option<DurationLiteralAt> {
    let root = parse.syntax();
    let source_len = usize::from(root.text().len());
    let offset = TextSize::from(offset.min(source_len) as u32);
    // At a token boundary both neighbors are candidates (e.g. the `-|5s`
    // edge, where the left token is the sign OUTSIDE the literal): take
    // whichever side actually sits inside a literal.
    let literal = root
        .token_at_offset(offset)
        .filter(|t| !t.kind().is_trivia())
        .find_map(|tok| duration_literal_ancestor(tok.parent()?))?;
    Some(build_literal_at(literal, usize::from(offset)))
}

/// Every duration literal intersecting `range`.
pub fn duration_literals_in(parse: &Parse, range: Span) -> Vec<DurationLiteralAt> {
    let root = parse.syntax();
    let mut out = Vec::new();
    collect_duration_literals(&root, range, &mut out);
    out
}

fn collect_duration_literals(node: &SyntaxNode, range: Span, out: &mut Vec<DurationLiteralAt>) {
    let span = node_span(node);
    // Prune subtrees outside the range: keeps range requests O(range), not
    // O(document).
    if !spans_intersect(span, range) {
        return;
    }
    if node.kind() == SyntaxKind::DurationLiteral {
        let literal = ast::DurationLiteral::cast(node.clone()).expect("kind checked");
        out.push(build_literal_at(literal, span.start));
        return;
    }
    for child in node.children() {
        collect_duration_literals(&child, range, out);
    }
}

fn build_literal_at(literal: ast::DurationLiteral, offset: usize) -> DurationLiteralAt {
    let span = node_span(literal.syntax());
    let raw = literal.components();
    let components: Vec<(Span, Option<Span>)> = raw
        .iter()
        .map(|(mag, unit)| (token_span(mag), unit.as_ref().map(token_span)))
        .collect();
    let active = active_component_index(literal.syntax(), &raw, offset);
    let sign = leading_sign_span(literal.syntax());
    DurationLiteralAt {
        literal,
        span,
        components,
        active,
        sign,
    }
}

/// The `Dash` token directly before the literal (trivia-skipping), if any.
fn leading_sign_span(literal: &SyntaxNode) -> Option<Span> {
    let mut prev = literal.prev_sibling_or_token();
    while let Some(el) = prev {
        if el.kind().is_trivia() {
            prev = el.prev_sibling_or_token();
            continue;
        }
        return (el.kind() == SyntaxKind::Dash).then(|| {
            let range = el.text_range();
            Span::new(usize::from(range.start()), usize::from(range.end()))
        });
    }
    None
}

fn active_component_index(
    literal: &SyntaxNode,
    components: &[(SyntaxToken, Option<SyntaxToken>)],
    offset: usize,
) -> usize {
    let offset = TextSize::from(offset as u32);
    let tok = literal
        .token_at_offset(offset)
        .right_biased()
        .or_else(|| literal.token_at_offset(offset).left_biased());
    let Some(tok) = tok else {
        return components.len().saturating_sub(1);
    };
    let tok_start = tok.text_range().start();
    for (i, (mag, unit)) in components.iter().enumerate() {
        if mag.text_range().start() == tok_start
            || unit
                .as_ref()
                .is_some_and(|u| u.text_range().start() == tok_start)
        {
            return i;
        }
        let end = unit
            .as_ref()
            .map(|u| u.text_range().end())
            .unwrap_or_else(|| mag.text_range().end());
        if mag.text_range().start() <= tok_start && tok_start < end {
            return i;
        }
    }
    components.len().saturating_sub(1)
}

fn duration_literal_ancestor(mut node: SyntaxNode) -> Option<ast::DurationLiteral> {
    loop {
        if let Some(dl) = ast::DurationLiteral::cast(node.clone()) {
            return Some(dl);
        }
        node = node.parent()?;
    }
}

fn spans_intersect(a: Span, b: Span) -> bool {
    a.start < b.end && b.start < a.end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cst::parse;

    #[test]
    fn parse_wraps_value_duration_literal() {
        use crate::cst::syntax::SyntaxKind;
        let p = parse("service App:\n    k = 30s\n");
        let count = p
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::DurationLiteral)
            .count();
        assert_eq!(count, 1, "{}", p.syntax());
    }

    #[test]
    fn duration_literal_at_finds_compound_value() {
        let src = "service App:\n    timeout = 1h30m\n";
        let p = parse(src);
        let at = duration_literal_at(&p, src.find("30m").unwrap()).expect("literal");
        assert_eq!(at.components.len(), 2);
        assert_eq!(at.active, 1);
    }

    #[test]
    fn duration_literal_at_finds_facet() {
        let src = "model M:\n    timeout duration(min = 1h30m)\n";
        let p = parse(src);
        let at = duration_literal_at(&p, src.find("1h").unwrap()).expect("facet literal");
        assert_eq!(at.components.len(), 2);
        assert_eq!(at.active, 0);
    }

    #[test]
    fn tight_span_excludes_surrounding_trivia() {
        let src = "service App:\n    k = 1h30m\n";
        let p = parse(src);
        let at = duration_literal_at(&p, src.find('1').unwrap()).expect("literal");
        let tight = at.tight_span();
        assert_eq!(&src[tight.start..tight.end], "1h30m");
    }

    #[test]
    fn attached_text_is_trivia_immune() {
        // Tabs are legal inter-token trivia; token reconstruction must not
        // depend on the source whitespace between components.
        let src = "service App:\n    k = 1h\t30m\n";
        let p = parse(src);
        let at = duration_literal_at(&p, src.find('1').unwrap()).expect("literal");
        assert_eq!(at.literal.attached_text(), "1h30m");
    }

    #[test]
    fn display_span_includes_a_leading_sign() {
        // A negative duration is domain-invalid, but the editor still
        // highlights what the reader perceives as one literal.
        let src = "model M:\n    t duration(min = -5s)\n";
        let p = parse(src);
        let at = duration_literal_at(&p, src.find("5s").unwrap()).expect("literal");
        let display = at.display_span();
        assert_eq!(&src[display.start..display.end], "-5s");
        // Unsigned literals are unaffected.
        let src = "service App:\n    k = 5s\n";
        let p = parse(src);
        let at = duration_literal_at(&p, src.find("5s").unwrap()).expect("literal");
        assert!(at.sign.is_none());
        assert_eq!(at.display_span(), at.tight_span());
    }

    #[test]
    fn duration_literals_in_sweeps_range() {
        let src = "service App:\n    a = 1h30m\n    b = 5s\n";
        let p = parse(src);
        let start = src.find("1h30m").unwrap();
        let end = src.find("5s").unwrap() + 2;
        let found = duration_literals_in(&p, Span::new(start, end));
        assert_eq!(found.len(), 2);
    }
}
