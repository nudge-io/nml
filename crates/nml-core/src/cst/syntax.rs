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
    Eq,    // =
    Arrow, // -> (the arm arrow: `oneof` arms and every future arm form — RFC 0006)
    /// `=>` — lexed ONLY so the parser can reject it with targeted guidance
    /// ("'=>' was replaced by '->'"); accepted by no production (RFC 0006).
    FatArrow,
    Colon,    // :
    Dash,     // -
    Pipe,     // |
    Dot,      // .
    LBracket, // [
    RBracket, // ]
    LParen,   // (
    RParen,   // )
    Comma,    // ,
    Question, // ?
    Plus,     // +  (positional-field marker — RFC 0005 §16)
    /// `&` — the selector-conjunction operator (RFC 0014): valid only
    /// between `Role` tokens in value position (`@role/admin & @role/editor`).
    /// The language carries it opaquely; consumers assign the AND semantics.
    Amp, // &
    /// `<` — opens a type-constructor argument list (`set<cidr>`, RFC 0032).
    /// `set` is a contextual keyword: an `Ident` is a constructor only when
    /// immediately followed by `Lt` in type position; bare `set` stays a name.
    Lt, // <
    Gt,       // >  (closes a type-constructor argument list)
    /// `#` — opens a field directive (`#live`, `#key("host")` — RFC 0032).
    /// Directives are schema metadata the language parses but never
    /// interprets; consumers (e.g. nudge) assign meaning.
    Hash, // #

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
    /// RFC 0019: the `uses <LayerRef> (, <LayerRef>)*` header clause on an
    /// instance declaration — layer composition refs, sibling of `Extends`.
    Uses,
    // bodies & entries
    Body,
    Property,
    NestedBlock,
    Modifier,
    SharedProperty,
    ListItem,
    FieldDef,

    /// A field directive `#name` / `#name(value)` trailing a FieldDef (RFC
    /// 0032). Opaque to nml-core beyond syntax (name + optional value).
    Directive,
    /// A routing arm inside a plain block: `(@role/selector | else) -> Target`
    /// (the house arm idiom, RFC 0006 arrow). Generic in the grammar; the schema
    /// restricts where arms are valid (e.g. RFC 0018 `denial:`).
    Arm,
    TypeExpr,
    /// RFC 0018: the parenthesized facet list after a type name
    /// (`number(min = 1, max = 65535)`).
    FacetList,
    /// One `key = number` facet inside a [`SyntaxKind::FacetList`].
    Facet,
    // values
    Value,
    ArrayValue,
    Fallback,
    /// RFC 0017: a duration literal (`5s`, `1h30m`, `5 foo`) — the
    /// alternating `Number`/`Ident` chain wrapped for position queries.
    DurationLiteral,
    /// An error *node* wrapping recovered tokens (panic-mode recovery).
    Error,
}

impl SyntaxKind {
    /// Highest valid discriminant (the last variant). Used to bounds-check the
    /// `rowan` raw→typed cast.
    const LAST: u16 = SyntaxKind::Error as u16;

    /// The ONE human-name mapping for diagnostics (RFC 0009): every
    /// "expected …, found …" renders through this, so wording cannot drift
    /// per site. Total: a new kind cannot compile without naming itself.
    pub(crate) fn describe(self) -> &'static str {
        use SyntaxKind::*;
        match self {
            Whitespace => "whitespace",
            Newline => "a line break",
            Comment => "a comment",
            Indent => "an indent",
            Dedent => "a dedent",
            Ident => "a name",
            Number => "a number",
            String => "a string",
            Role => "a role reference",
            Secret => "a variable reference",
            Eq => "`=`",
            Arrow => "`->`",
            FatArrow => "`=>`",
            Colon => "`:`",
            Dash => "`-`",
            Pipe => "`|`",
            Dot => "`.`",
            LBracket => "`[`",
            RBracket => "`]`",
            LParen => "`(`",
            RParen => "`)`",
            Comma => "`,`",
            Question => "`?`",
            Plus => "`+`",
            Amp => "`&`",
            Lt => "`<`",
            Gt => "`>`",
            Hash => "`#`",
            ErrorToken => "an unrecognized character",
            Eof => "end of file",
            Root => "a document",
            BlockDecl => "a block declaration",
            ArrayDecl => "an array declaration",
            ConstDecl => "a const declaration",
            TemplateDecl => "a template declaration",
            OneOfDecl => "a oneof declaration",
            OneOfArm => "a oneof arm",
            Name => "a name",
            Extends => "an `is` clause",
            Uses => "a `uses` clause",
            Body => "a block body",
            Property => "a property",
            NestedBlock => "a nested block",
            Modifier => "a modifier",
            SharedProperty => "a shared property",
            ListItem => "a list item",
            FieldDef => "a field definition",
            Directive => "a directive",
            Arm => "a routing arm",
            TypeExpr => "a type",
            FacetList => "a facet list",
            Facet => "a facet",
            Value => "a value",
            ArrayValue => "an array value",
            Fallback => "a fallback chain",
            DurationLiteral => "a duration literal",
            Error => "unparsed input",
        }
    }

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

/// The duration decoder's borrowed component views, from paired
/// magnitude/unit tokens — the ONE assembly the value decoder, the facet
/// lowering, and the facet extraction all share, so their tokens→decoder
/// handoff cannot drift. `first_magnitude` substitutes for the first
/// magnitude's text: an authored sign lives in a separate `Dash` token,
/// so the signed spelling (`"-5"`) exists in no single token.
pub(super) fn duration_component_tokens<'a>(
    pairs: &'a [(SyntaxToken, SyntaxToken)],
    first_magnitude: &'a str,
) -> Vec<crate::duration::ComponentTokens<'a>> {
    pairs
        .iter()
        .enumerate()
        .map(|(i, (magnitude, unit))| crate::duration::ComponentTokens {
            magnitude: if i == 0 {
                first_magnitude
            } else {
                magnitude.text()
            },
            unit: unit.text(),
            magnitude_span: token_span(magnitude),
            unit_span: token_span(unit),
        })
        .collect()
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
