use serde::Serialize;

/// What a span site IS, declared by the emitter as DATA — so a consumer
/// judges a span by its STRUCTURE and never by its name. `kind` is a label
/// for the person reading a failure: a rule read off one held the carrier
/// whose field is spelled `selector_content` to the rule for whole spans,
/// silently, because the name does not end in `.content`; and renaming a
/// carrier would have flipped another. Every consumer matches this
/// exhaustively, so a new shape is a compile error there rather than a
/// silent mis-judgement.
///
/// A whole node's span and a whole token's span are ONE shape: both are
/// token-aligned, and no checker can tell them apart — a third variant
/// would be a classification nothing could ever catch being wrong, which
/// is the defect this enum exists to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanShape {
    /// A whole node's or a whole token's span: TOKEN-ALIGNED — it begins
    /// at a significant token's first byte and ends at one's last byte.
    Aligned,
    /// The bytes a machine-applicable replacement of a literal's CONTENT
    /// substitutes: the window inside the delimiters of `token`, which it
    /// is STRICTLY inside. The byte rule alone (in bounds, on character
    /// boundaries) also admits the whole token, and splicing that rewrites
    /// `"GE" -> h` into `GET -> h` — so the window carries the token it
    /// must stay inside of, and a consumer checks both.
    ContentWindow { token: Span },
    /// A template expression's `{{…}}` bytes inside the string `token`.
    TemplateExpression { token: Span },
}

/// One span a node carries, named by the field that holds it (`"FieldDef"`,
/// `"Directive.arg"`, …) and SHAPED by what it is ([`SpanShape`]) — the
/// unit the span walkers enumerate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanSite {
    pub kind: &'static str,
    pub span: Span,
    pub shape: SpanShape,
}

impl SpanSite {
    /// A whole node's or a whole token's span.
    pub fn aligned(kind: &'static str, span: Span) -> Self {
        Self {
            kind,
            span,
            shape: SpanShape::Aligned,
        }
    }

    /// A replacement window inside `token`'s delimiters. The structural
    /// half of the shape's rule is asserted at the MINT: the window
    /// begins after the token's first byte (its opening delimiter) and
    /// ends no later than the token — a window equal to its token
    /// satisfies the byte rule and splices a literal's quotes away, and
    /// the audit used to see such a site only if a reader asked. The
    /// character-boundary and token-alignment halves need the source and
    /// its boundaries, which is `TokenBoundaries::check`'s job (the one
    /// judge every reader shares). No product path mints a window — the
    /// span walkers are an audit surface — so this fires only in the
    /// audit that exists to find it.
    pub fn window(kind: &'static str, span: Span, token: Span) -> Self {
        assert!(
            token.start < span.start && span.start <= span.end && span.end <= token.end,
            "{kind}: content window {span:?} is not strictly inside its token {token:?}"
        );
        Self {
            kind,
            span,
            shape: SpanShape::ContentWindow { token },
        }
    }

    /// A template expression inside the string `token`.
    pub fn expression(kind: &'static str, span: Span, token: Span) -> Self {
        Self {
            kind,
            span,
            shape: SpanShape::TemplateExpression { token },
        }
    }
}

/// Where a value sits in its source. The CONTENT window is the bytes a
/// machine-applicable replacement substitutes: a string literal's inside,
/// minted where the token is read
/// ([`crate::cst`]), so an UNTERMINATED literal — whose token carries no
/// closing delimiter to strip — is exact too; the value's own span for
/// everything that is not a quoted literal.
///
/// ONE named parameter rather than two spans side by side: two spans can
/// be passed in either order, and the wrong order is a suggestion that
/// rewrites a literal's quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ValueSpan {
    /// What a diagnostic points at: the value as authored, a literal's
    /// delimiters included.
    pub whole: Span,
    /// What a machine-applicable replacement of the value's CONTENT
    /// substitutes.
    pub content: Span,
}

impl ValueSpan {
    /// A value that is not a quoted literal: its content IS its span.
    pub fn whole(span: Span) -> Self {
        Self {
            whole: span,
            content: span,
        }
    }

    /// A quoted literal: the span covers its delimiters, the window the
    /// bytes between them.
    pub fn literal(whole: Span, content: Span) -> Self {
        Self { whole, content }
    }
}

/// A byte offset range in the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// The content-span invariant every AST and schema span keeps: a
    /// non-empty span lies inside `source` and both its first and its last
    /// byte are content — never a space, a tab or a line break. An empty
    /// span is a position and is content wherever it lies. The kernel
    /// computes every node span as a content span (`cst::syntax::
    /// content_span`); the corpus pin adds it to the shape-aware verdict
    /// ([`crate::cst::TokenBoundaries::check`]) over every span
    /// [`crate::ast::for_each_span`] and [`crate::schema::for_each_span`]
    /// enumerate, where a terminated corpus makes the proxy exact.
    pub fn is_content_in(&self, source: &str) -> bool {
        let bytes = source.as_bytes();
        if self.start > self.end || self.end > bytes.len() {
            return false;
        }
        if self.start == self.end {
            return true;
        }
        let blank = |c: u8| matches!(c, b' ' | b'\t' | b'\r' | b'\n');
        !blank(bytes[self.start]) && !blank(bytes[self.end - 1])
    }

    pub fn empty(offset: usize) -> Self {
        Self {
            start: offset,
            end: offset,
        }
    }
}

/// A line and column location in the source text (1-indexed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Location {
    pub line: usize,
    pub column: usize,
}

impl Location {
    pub fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

/// Maps byte offsets to line/column locations.
pub struct SourceMap {
    line_starts: Vec<usize>,
}

impl SourceMap {
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0];
        for (i, ch) in source.char_indices() {
            if ch == '\n' {
                line_starts.push(i + 1);
            }
        }
        Self { line_starts }
    }

    pub fn location(&self, offset: usize) -> Location {
        let line = self
            .line_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let column = offset - self.line_starts[line];
        Location::new(line + 1, column + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_map() {
        let source = "hello\nworld\nfoo";
        let map = SourceMap::new(source);
        assert_eq!(map.location(0), Location::new(1, 1));
        assert_eq!(map.location(5), Location::new(1, 6));
        assert_eq!(map.location(6), Location::new(2, 1));
        assert_eq!(map.location(12), Location::new(3, 1));
    }

    #[test]
    fn test_span_merge() {
        let a = Span::new(5, 10);
        let b = Span::new(8, 15);
        let merged = a.merge(b);
        assert_eq!(merged, Span::new(5, 15));
    }

    /// A content window is unforgeable at the mint: equal to its token,
    /// starting on the opening delimiter, or reaching past the token, it
    /// cannot be built — the audit's rule, asserted where the site is
    /// made rather than only where a reader happens to check it.
    #[test]
    #[should_panic(expected = "is not strictly inside its token")]
    fn a_window_equal_to_its_token_cannot_be_minted() {
        let token = Span::new(4, 9);
        let _ = SpanSite::window("c", token, token);
    }

    #[test]
    #[should_panic(expected = "is not strictly inside its token")]
    fn a_window_past_its_token_cannot_be_minted() {
        let _ = SpanSite::window("c", Span::new(5, 10), Span::new(4, 9));
    }

    #[test]
    fn a_window_inside_its_token_mints() {
        let token = Span::new(4, 9);
        let site = SpanSite::window("c", Span::new(5, 8), token);
        assert_eq!(site.shape, SpanShape::ContentWindow { token });
        // An unterminated literal's window ends ON the token's end.
        let _ = SpanSite::window("c", Span::new(5, 9), token);
        // An empty window right after the delimiter (`""`).
        let _ = SpanSite::window("c", Span::new(5, 5), token);
    }

    /// The content-span predicate's contract: a non-empty span's first and
    /// last byte are neither a space, a tab, a carriage return nor a line
    /// break; an out-of-range or inverted span is never content; an empty
    /// span is a position and content wherever it lies.
    #[test]
    fn is_content_in_refuses_every_blank_byte_and_the_out_of_range() {
        let src = "ab \t\r\ncd";
        assert!(Span::new(0, 2).is_content_in(src));
        assert!(Span::new(6, 8).is_content_in(src));
        assert!(
            Span::new(0, 8).is_content_in(src),
            "blank bytes INSIDE are fine"
        );
        for end in [3, 4, 5, 6] {
            assert!(
                !Span::new(0, end).is_content_in(src),
                "ends on a blank byte: {end}"
            );
        }
        for start in [2, 3, 4, 5] {
            assert!(
                !Span::new(start, 8).is_content_in(src),
                "starts on a blank byte: {start}"
            );
        }
        assert!(!Span::new(0, 9).is_content_in(src), "past the end");
        assert!(!Span::new(3, 2).is_content_in(src), "inverted");
        assert!(Span::empty(3).is_content_in(src), "a position");
        assert!(Span::empty(8).is_content_in(src), "a position at the end");
    }
}
