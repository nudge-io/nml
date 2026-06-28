//! `SyntaxKind` — the single token-and-node taxonomy for the CST — and the
//! `rowan` [`Language`](rowan::Language) binding.
//!
//! RFC 0004 §4.1/§4.2: one enum spans the lexer, the parser, and the tree;
//! there is no parallel token/node taxonomy. Discriminants are contiguous and
//! `repr(u16)` so the `rowan` round-trip is a checked cast (no per-variant
//! match to keep in sync).

/// Every token and node kind in the NML CST.
///
/// Variants are grouped: **trivia** (lossless, invisible to the parser),
/// **structural tokens** (parser-consumed, including the offside-rule layout
/// markers `Indent`/`Dedent`), then **nodes**. `Error` (the error *node*) is
/// kept last so the internal `LAST` bound covers the valid discriminant range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum SyntaxKind {
    // ── trivia: kept in the tree for losslessness, hidden from the parser ──
    Whitespace,
    Newline,
    Comment,

    // ── offside layout markers (zero-width, parser-consumed; RFC 0004 §4.2.1) ──
    Indent,
    Dedent,

    // ── atoms (raw source text; semantic decoding happens at the value layer) ──
    Ident,
    Number,
    String,
    /// A role reference, e.g. `@role/admin`, `@public`.
    Role,
    /// A variable reference, e.g. `$ENV.MY_VAR`.
    Secret,

    // ── punctuation ──
    Eq,        // =
    FatArrow,  // =>
    Colon,     // :
    Dash,      // -
    Pipe,      // |
    Dot,       // .
    LBracket,  // [
    RBracket,  // ]
    LParen,    // (
    RParen,    // )
    Comma,     // ,
    Question,  // ?
    Bang,      // !

    /// Unrecognized input, one character wide. Never dropped — every source
    /// byte lands in some token, so the tree is byte-faithful on any input.
    ErrorToken,
    /// Zero-width end-of-input sentinel (synthesized by the parser cursor; not
    /// emitted as a physical token).
    Eof,

    // ── nodes ──
    Root,
    // declarations
    BlockDecl,
    ArrayDecl,
    ConstDecl,
    TemplateDecl,
    OneOfDecl,
    OneOfArm,
    Name,
    Extends,
    // bodies & entries
    Body,
    Property,
    NestedBlock,
    Modifier,
    SharedProperty,
    ListItem,
    FieldDef,
    TypeExpr,
    // values
    Value,
    ArrayValue,
    Fallback,
    /// An error *node* wrapping recovered tokens (panic-mode recovery).
    Error,
}

impl SyntaxKind {
    /// Highest valid discriminant (the last variant). Used to bounds-check the
    /// `rowan` raw→typed cast.
    const LAST: u16 = SyntaxKind::Error as u16;

    /// Trivia is preserved in the tree but never seen by the parser (RFC 0004
    /// §4.2.1: trivia stays invisible; structure is explicit via layout tokens).
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            SyntaxKind::Whitespace | SyntaxKind::Newline | SyntaxKind::Comment
        )
    }
}

/// The `rowan` language marker for NML (uninhabited — it is a type-level tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NmlLanguage {}

impl rowan::Language for NmlLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> SyntaxKind {
        assert!(
            raw.0 <= SyntaxKind::LAST,
            "rowan SyntaxKind {} out of range for NML",
            raw.0
        );
        // SAFETY: `SyntaxKind` is `repr(u16)` with contiguous discriminants
        // `0..=LAST`, and the bound is asserted above.
        unsafe { std::mem::transmute::<u16, SyntaxKind>(raw.0) }
    }

    fn kind_to_raw(kind: SyntaxKind) -> rowan::SyntaxKind {
        rowan::SyntaxKind(kind as u16)
    }
}

/// Convenience: typed `rowan` aliases for the NML tree.
pub type SyntaxNode = rowan::SyntaxNode<NmlLanguage>;
pub type SyntaxToken = rowan::SyntaxToken<NmlLanguage>;

/// Raw-kind helper for the tree builder.
pub(super) fn raw(kind: SyntaxKind) -> rowan::SyntaxKind {
    rowan::SyntaxKind(kind as u16)
}

/// A `rowan` byte offset as a `usize`.
pub(super) fn text_offset(offset: rowan::TextSize) -> usize {
    u32::from(offset) as usize
}

/// The full byte span of a node (the single home for `TextRange → Span`).
pub(super) fn node_span(node: &SyntaxNode) -> crate::span::Span {
    let r = node.text_range();
    crate::span::Span::new(text_offset(r.start()), text_offset(r.end()))
}

/// The byte span of a token.
pub(super) fn token_span(tok: &SyntaxToken) -> crate::span::Span {
    let r = tok.text_range();
    crate::span::Span::new(text_offset(r.start()), text_offset(r.end()))
}

/// The span of a node's **significant** content — first to last non-trivia token
/// in its subtree. Unlike [`node_span`], this excludes leading/trailing attached
/// trivia (comments, whitespace) — important wherever spans drive behaviour
/// (template offsets, comment placement) rather than just diagnostics.
pub(super) fn content_span(node: &SyntaxNode) -> crate::span::Span {
    let mut first = None;
    let mut last = None;
    for tok in node
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
    {
        let r = tok.text_range();
        first.get_or_insert(text_offset(r.start()));
        last = Some(text_offset(r.end()));
    }
    match (first, last) {
        (Some(s), Some(e)) => crate::span::Span::new(s, e),
        _ => node_span(node),
    }
}
