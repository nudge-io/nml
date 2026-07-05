//! RFC 0004 — Lossless CST: the **production parser** for NML.
//!
//! A `rowan` red/green tree over the full grammar. It is the single parse entry
//! point; semantic consumers read the `ast` it lowers to (see `lower`), while
//! tooling reads the tree directly for spans/trivia/comments. Four guarantees,
//! upheld across the whole grammar and exercised by the fuzz harness:
//!
//! 1. **Losslessness** — the tree's text is byte-identical to the source on *any*
//!    input (every byte lands in a token, including trivia and `ErrorToken`).
//! 2. **Resilience / all-errors** — a syntax error never aborts the parse; the
//!    tree is always produced and every error is collected in one pass.
//! 3. **Offside correctness** — indentation drives structure via zero-width
//!    `Indent`/`Dedent` tokens, and layout is **suppressed inside `"""…"""`**.
//! 4. **Termination / bounded output** — recovery always makes forward progress
//!    and the error list is capped, so adversarial input is safe (RFC 0004 §9).
//!
//! Public surface: `parse` (→ lossless `Parse`), `parse_to_ast` /
//! `parse_to_ast_all` (→ semantic `ast`), `parse_with_comments`,
//! `extract_schema`, and the `ast` / `extract` / `lower` layers.

pub mod ast;
pub mod extract;
mod lexer;
pub mod lower;
mod parser;
mod syntax;
mod value;

pub use syntax::{NmlLanguage, SyntaxKind, SyntaxNode, SyntaxToken};
pub use value::decode_value;

use crate::error::NmlError;
use rowan::GreenNode;

/// The result of parsing: always a (best-effort, lossless) tree, plus every
/// error (RFC 0004 §4.3).
pub struct Parse {
    green: GreenNode,
    errors: Vec<NmlError>,
}

impl Parse {
    /// The root of the typed syntax tree.
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    /// Every diagnostic collected during lexing and parsing.
    pub fn errors(&self) -> &[NmlError] {
        &self.errors
    }

    /// Strict callers: the full tree, or **every** error (RFC 0004 §4.3 — the
    /// all-errors contract reaches strict loads, not just the LSP).
    pub fn ok(self) -> Result<SyntaxNode, Vec<NmlError>> {
        if self.errors.is_empty() {
            Ok(SyntaxNode::new_root(self.green))
        } else {
            Err(self.errors)
        }
    }
}

/// Single cap on reported diagnostics, applied *at emission* in both the lexer
/// (`super::MAX_ERRORS`) and the parser, then again on the merged list below —
/// so memory stays bounded during and after parsing on pathological input
/// (RFC 0004 §9, "bounded output").
const MAX_ERRORS: usize = 128;

/// Parse NML source into a lossless CST. Never fails, never panics.
pub fn parse(source: &str) -> Parse {
    let lexed = lexer::lex(source);
    let mut p = parser::Parser::new(&lexed.tokens);
    p.parse_root();
    let (events, parse_errors) = p.finish_parse();
    let green = parser::build_tree(&lexed.tokens, &events);

    let mut errors = lexed.errors;
    errors.extend(parse_errors);
    errors.truncate(MAX_ERRORS);

    Parse { green, errors }
}

/// Parse to the **semantic AST**, collecting **every** diagnostic — syntactic
/// *and* semantic — in source-position order. The AST is always returned
/// (best-effort, with placeholders for un-decodable values); an **empty error
/// list means the input is fully valid**.
///
/// This is the all-errors form: value validation (escapes, money precision,
/// `$ENV` namespaces, number range) is deferred to decode, so a single
/// [`lower::to_ast_with_errors`] pass collects those, merged with the syntactic
/// errors. Use this to report every problem at once; [`parse_to_ast`] is the
/// single-error drop-in derived from it.
pub fn parse_to_ast_all(source: &str) -> (crate::ast::File, Vec<NmlError>) {
    let (_parsed, file, mut errors) = parse_lowered(source);
    errors.truncate(MAX_ERRORS); // bounded output (RFC 0004 §9)
    (file, errors)
}

/// Shared core: parse to the CST, lower to the semantic AST, and merge the
/// syntactic + semantic errors into one position-sorted list. Returns the
/// [`Parse`] too (callers needing the tree, e.g. for comments). The single home
/// for the parse → AST + diagnostics pipeline.
fn parse_lowered(source: &str) -> (Parse, crate::ast::File, Vec<NmlError>) {
    use ast::AstNode as _;
    let parsed = parse(source);
    let root = ast::Root::cast(parsed.syntax()).expect("parse always yields a Root node");
    let (file, mut errors) = lower::to_ast_with_errors(&root);
    errors.extend(parsed.errors().iter().cloned());
    errors.sort_by_key(|e| e.span().start);
    (parsed, file, errors)
}

/// Parse to the owned AST, returning the **first** error by source position — a
/// drop-in for the legacy `crate::parse`. Derived from [`parse_to_ast_all`];
/// callers wanting every diagnostic use that directly.
pub fn parse_to_ast(source: &str) -> crate::error::NmlResult<crate::ast::File> {
    let (file, errors) = parse_to_ast_all(source);
    match errors.into_iter().next() {
        Some(e) => Err(e),
        None => Ok(file),
    }
}

/// Parse to a **best-effort** owned AST, discarding diagnostics. Resilient
/// recovery means the AST is always populated with whatever parsed, so
/// structure-driven tooling (LSP completion/hover/goto/references) keeps working
/// mid-edit instead of going dark on the first syntax error. Diagnostics are the
/// diagnostics path's job ([`parse_to_ast_all`]); feature handlers want only the
/// structure, and this names that intent at the call site.
pub fn parse_best_effort(source: &str) -> crate::ast::File {
    parse_to_ast_all(source).0
}

/// Extract schema definitions (models / enums / oneofs) from source over the CST,
/// reading the tree directly (extraction needs no owned AST). Returns the
/// [`ExtractedSchema`](crate::schema::ExtractedSchema) plus the **full**
/// error set — syntactic *and* semantic (e.g. an out-of-precision money default),
/// position-sorted and bounded, identical to [`parse_to_ast_all`]. Resilient:
/// definitions from the well-formed parts are always returned, so a mid-edit
/// schema file still contributes what it can. This is the single schema-loading
/// primitive shared by the validator's loader, the LSP registry, and embedders.
pub fn extract_schema(source: &str) -> (crate::schema::ExtractedSchema, Vec<NmlError>) {
    use ast::AstNode as _;
    // One parse; the canonical lower pass yields every diagnostic, and `extract`
    // reads the same tree for the schema itself.
    let (parsed, _ast, mut errors) = parse_lowered(source);
    errors.truncate(MAX_ERRORS);
    let root = ast::Root::cast(parsed.syntax()).expect("parse always yields a Root node");
    (extract::extract(&root), errors)
}

/// The leading documentation comment of the top-level declaration named `name`
/// (RFC 0004 §4.3 comment attachment), or `None`. A convenience for tooling (LSP
/// hover) that surfaces comments as docs by name without holding the `!Send` tree.
pub fn doc_comment_for(source: &str, name: &str) -> Option<String> {
    use ast::AstNode as _;
    let root = ast::Root::cast(parse(source).syntax())?;
    let decl = root
        .decls()
        .find(|d| d.name().and_then(|n| n.text()).as_deref() == Some(name))?;
    decl.doc_comment()
}

/// A source comment extracted from the CST (RFC 0004 §4.3). Comments are not part
/// of the semantic [`ast`](crate::ast); they are surfaced here as a side channel
/// for tools (e.g. the formatter) that must preserve them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// Comment text after the leading `//`, verbatim except for trailing
    /// whitespace (so `////` dividers and deliberate spacing survive).
    pub text: String,
    /// Span covering the comment from `//` to end of line (exclusive of the newline).
    pub span: crate::span::Span,
    /// True when only whitespace precedes the comment on its line; false when it
    /// trails code on the same line.
    pub own_line: bool,
}

/// Parse to the semantic AST **with comments**. Comments are read from the
/// **lossless tree** itself, in source order, with own-line/trailing placement
/// derived from their position — no separate side-channel pass.
pub fn parse_with_comments(
    source: &str,
) -> crate::error::NmlResult<(crate::ast::File, Vec<Comment>)> {
    let (parsed, file, errors) = parse_lowered(source);
    match errors.into_iter().next() {
        Some(e) => Err(e),
        None => Ok((file, comments_of(&parsed.syntax()))),
    }
}

/// Extract source-ordered [`Comment`]s from the CST's comment tokens.
fn comments_of(root: &SyntaxNode) -> Vec<Comment> {
    root.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == SyntaxKind::Comment)
        .map(|t| {
            let raw = t.text();
            Comment {
                text: raw.strip_prefix("//").unwrap_or(raw).trim_end().to_string(),
                span: syntax::token_span(&t),
                own_line: is_own_line(&t),
            }
        })
        .collect()
}

/// A comment is "own-line" when only whitespace precedes it on its line.
fn is_own_line(tok: &SyntaxToken) -> bool {
    let mut prev = tok.prev_token();
    while let Some(t) = prev {
        match t.kind() {
            SyntaxKind::Newline => return true,        // reached line start
            SyntaxKind::Whitespace => prev = t.prev_token(),
            _ => return false,                         // code precedes it → trailing
        }
    }
    true // start of file
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal typed wrapper used by the structural tests. The full typed-wrapper
    /// layer (all node kinds) lands in P4 (`cst::ast`) with its consumers; until
    /// then the public API stays `Parse`/`parse`/`decode_value` only.
    struct BlockDecl(SyntaxNode);

    impl BlockDecl {
        fn cast(node: SyntaxNode) -> Option<Self> {
            (node.kind() == SyntaxKind::BlockDecl).then_some(Self(node))
        }
        fn keyword(&self) -> Option<SyntaxToken> {
            self.0
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .find(|t| t.kind() == SyntaxKind::Ident)
        }
        fn name(&self) -> Option<String> {
            self.0
                .children()
                .find(|n| n.kind() == SyntaxKind::Name)
                .and_then(|n| first_ident(&n))
                .map(|t| t.text().to_string())
        }
        fn body(&self) -> Option<SyntaxNode> {
            self.0.children().find(|n| n.kind() == SyntaxKind::Body)
        }
    }

    fn block_decls(root: &SyntaxNode) -> impl Iterator<Item = BlockDecl> {
        root.children().filter_map(BlockDecl::cast)
    }

    /// First `Ident` token directly under `node`, skipping trivia.
    fn first_ident(node: &SyntaxNode) -> Option<SyntaxToken> {
        node.children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind() == SyntaxKind::Ident)
    }

    #[test]
    fn parse_to_ast_drop_in_smoke() {
        use crate::ast::DeclarationKind;
        // Valid input → semantic AST.
        let file = parse_to_ast("service App:\n    port = 8080\n").unwrap();
        assert_eq!(file.declarations.len(), 1);
        assert!(matches!(file.declarations[0].kind, DeclarationKind::Block(_)));
        // Invalid input → Err (the first error).
        assert!(parse_to_ast("service App:\n    @@@\n").is_err());
    }

    #[test]
    fn parse_to_ast_surfaces_semantic_errors() {
        // The CST defers value validation to decode; the drop-in re-surfaces it,
        // so these all error.
        for src in [
            "service App:\n    p = 9.999 USD\n",  // money precision (USD has 2 dp)
            "service App:\n    s = \"bad \\q\"\n", // unknown escape
            "service App:\n    k = $NOPE.X\n",    // unknown secret namespace
            "service App:\n    items = [9.999 USD]\n", // nested (inside an array)
        ] {
            assert!(parse_to_ast(src).is_err(), "should error: {src:?}");
        }
    }

    #[test]
    fn parse_to_ast_all_no_redundant_structural_errors() {
        // Incomplete values are *syntactic* failures the parser reports; decode
        // (semantic) must not double-count them. `x =` (empty) and `y = -` (dash,
        // no number) are each ONE problem → ONE error.
        for src in ["service App:\n    x =\n", "service App:\n    y = -\n"] {
            let (_ast, errors) = parse_to_ast_all(src);
            assert_eq!(errors.len(), 1, "one problem → one error for {src:?}: {errors:?}");
            // The single error is the parser's syntactic one, not a duplicate
            // "empty value" decode error (which would be earlier by position).
            assert!(!errors[0].message().contains("empty value"));
        }
    }

    /// First Comment token whose text matches.
    fn comment_with(root: &SyntaxNode, needle: &str) -> SyntaxToken {
        root.descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind() == SyntaxKind::Comment && t.text().contains(needle))
            .unwrap_or_else(|| panic!("comment containing {needle:?} not found"))
    }

    #[test]
    fn comment_attachment_policy() {
        // RFC 0004 §4.3: own-line comment → leading of the FOLLOWING node;
        // same-line trailing comment → the PRECEDING node.
        let root = parse(
            "// header\nservice App: // hdr-trail\n    port = 8080 // trail\n    // own\n    host = 9090\n",
        )
        .syntax();

        // own-line file header → inside the following declaration, not Root.
        assert_eq!(
            comment_with(&root, "header").parent().unwrap().kind(),
            SyntaxKind::BlockDecl,
            "own-line header → following node"
        );
        // trailing comment on the header line → the BlockDecl (preceding node).
        assert_eq!(
            comment_with(&root, "hdr-trail").parent().unwrap().kind(),
            SyntaxKind::BlockDecl,
            "same-line trailing on header → preceding node"
        );
        // trailing comment after a value → the Value it trails.
        assert_eq!(
            comment_with(&root, " trail").parent().unwrap().kind(),
            SyntaxKind::Value,
            "same-line trailing → preceding (innermost) node"
        );
        // own-line comment before a property → leading of that property.
        assert_eq!(
            comment_with(&root, "own").parent().unwrap().kind(),
            SyntaxKind::Property,
            "own-line comment → following node"
        );
    }

    /// RFC 0004 §4.3 — an own-line comment separated from the following node by a
    /// body-closing **dedent** attaches to the following node, not the preceding
    /// body. `build_tree` defers such a comment past the (zero-width) dedent into
    /// the outer scope its column belongs to (column-aware deferred-trivia buffer).
    #[test]
    fn own_line_comment_before_dedent_attaches_to_following_node() {
        // `// between` (col 0) sits between two declarations; per §4.3 it should be
        // leading of the FOLLOWING declaration, not trapped in the preceding body.
        let src = "service A:\n    p = 1\n// between\nservice B:\n    q = 2\n";
        let p = parse(src);
        assert_eq!(p.syntax().text().to_string(), src, "deferral stays lossless");
        let parent = comment_with(&p.syntax(), "between").parent().unwrap().kind();
        assert_ne!(parent, SyntaxKind::Body, "must not attach to the closing body");
        assert_eq!(parent, SyntaxKind::BlockDecl, "own-line → following declaration");
    }

    #[test]
    fn trailing_and_deferred_comment_coexist_across_dedent() {
        // A same-line trailing comment on the body's last line AND an own-line
        // col-0 comment before the dedent must each attach correctly and stay
        // lossless: the trailer to its preceding value, the own-line to the
        // following declaration.
        let src = "service A:\n    p = 1 // trail\n// between\nservice B:\n    q = 2\n";
        let p = parse(src);
        assert_eq!(p.syntax().text().to_string(), src, "stays lossless");
        assert_eq!(
            comment_with(&p.syntax(), "trail").parent().unwrap().kind(),
            SyntaxKind::Value,
            "same-line trailing → preceding value"
        );
        assert_eq!(
            comment_with(&p.syntax(), "between").parent().unwrap().kind(),
            SyntaxKind::BlockDecl,
            "own-line before dedent → following declaration"
        );
    }

    #[test]
    fn body_indented_comment_before_dedent_stays_in_body() {
        // The dual of the above: a comment indented at the body's level is the last
        // line *of that body*, so it must NOT be deferred out — column-awareness
        // distinguishes it from the outer-scope (col-0) case.
        let src = "service A:\n    p = 1\n    // tail\nservice B:\n    q = 2\n";
        let p = parse(src);
        assert_eq!(p.syntax().text().to_string(), src, "stays lossless");
        assert_eq!(
            comment_with(&p.syntax(), "tail").parent().unwrap().kind(),
            SyntaxKind::Body,
            "body-indented comment belongs to the closing body"
        );
    }

    #[test]
    fn comment_deferred_to_intermediate_scope_not_outermost() {
        // Three indent levels: a comment at the MIDDLE column must escape the inner
        // body yet stop at the middle scope (here, leading of the following
        // middle-level property) — not fall through to the outermost following decl.
        let src = "service A:\n    group:\n        x = 1\n    // mid\n    y = 2\n";
        let p = parse(src);
        assert_eq!(p.syntax().text().to_string(), src, "multi-level deferral is lossless");
        let parent = comment_with(&p.syntax(), "mid").parent().unwrap();
        // It leads the following middle-scope property `y` — having escaped the
        // inner (col-8) body but stopped short of the outermost following decl.
        assert_eq!(parent.kind(), SyntaxKind::Property, "own-line → following property");
        assert!(
            parent.text().to_string().contains("y = 2"),
            "must lead the middle-scope property y, got: {:?}",
            parent.text().to_string()
        );
    }

    #[test]
    fn deferred_comment_at_eof_stays_lossless() {
        // Hardest losslessness case for the deferral: own-line comments that close
        // out nested bodies at EOF, with and without a trailing newline. If a
        // deferred comment were stranded (its scope never reopening), tree text
        // would diverge from source — assert it never does.
        for src in [
            "a:\n    b:\n        x = 1\n// c",       // col-0, deep nesting, no trailing nl
            "a:\n    b:\n        x = 1\n// c\n",      // …with trailing nl
            "a:\n    b:\n        x = 1\n    // mid",  // mid-column comment at EOF
            "a:\n    p = 1\n// c1\n// c2",            // multi-line block at EOF
            "a:\n    b:\n        x = 1\n  // c\n",    // comment at an unaligned column
        ] {
            let p = parse(src);
            assert_eq!(
                p.syntax().text().to_string(),
                src,
                "deferral must stay byte-lossless for {src:?}"
            );
        }
    }

    #[test]
    fn extract_schema_is_cst_native_and_resilient() {
        // Well-formed schema: definitions extracted, no errors.
        let (schema, errors) = extract_schema("model svc:\n    port number\n");
        assert!(errors.is_empty(), "clean schema has no errors: {errors:?}");
        assert_eq!(schema.models.len(), 1);
        assert_eq!(schema.models[0].name, "svc");

        // Resilient: a malformed leading construct still yields the well-formed
        // model that follows, and the parse error is surfaced (bounded).
        let (schema, errors) = extract_schema("@@@\nmodel ok:\n    name string\n");
        assert!(
            schema.models.iter().any(|m| m.name == "ok"),
            "extraction continues past the error"
        );
        assert!(!errors.is_empty(), "the parse error is surfaced");
        assert!(errors.len() <= MAX_ERRORS, "error output stays bounded");
    }

    #[test]
    fn extract_schema_surfaces_semantic_errors_in_defaults() {
        // A schema default can carry a *semantic* (decode-layer) error — here a
        // money value with too much precision. `extract_schema` must report the
        // full diagnostic set, matching `parse_to_ast_all`, not just syntactic
        // errors (otherwise a malformed default would slip through schema loading).
        let src = "model m:\n    x number = 9.999 USD\n";
        let errors = extract_schema(src).1;
        assert!(
            errors.iter().any(|e| e.message().contains("decimal places")),
            "semantic default error must surface: {:?}",
            errors.iter().map(|e| e.message().to_string()).collect::<Vec<_>>()
        );
        // Parity with the canonical all-errors entry point.
        assert_eq!(errors.len(), parse_to_ast_all(src).1.len());
    }

    #[test]
    fn doc_comment_for_reads_leading_comment_block() {
        // A multi-line comment block above a declaration becomes its doc, with
        // `//` markers stripped and lines joined. The dedent fix is what lets the
        // block attach to `B` rather than the preceding body.
        let src = "service A:\n    p = 1\n// line one\n// line two\nservice B:\n    q = 2\n";
        assert_eq!(
            doc_comment_for(src, "B").as_deref(),
            Some("line one\nline two"),
            "leading comment block is the declaration's documentation"
        );
        // A declaration without a leading comment has no doc.
        assert_eq!(doc_comment_for(src, "A"), None);
        // Unknown names resolve to nothing rather than panicking.
        assert_eq!(doc_comment_for(src, "Nope"), None);
    }

    #[test]
    fn parse_with_comments_extracts_from_tree() {
        // Same fixture as the legacy lexer's comment test — own-line vs trailing
        // placement must match.
        let src =
            "// header\nservice App: // trailing\n    // indented\n    port = 8080 // why\n";
        let (file, comments) = parse_with_comments(src).unwrap();
        assert_eq!(file.declarations.len(), 1);
        assert_eq!(comments.len(), 4);
        assert_eq!(comments[0].text, " header");
        assert!(comments[0].own_line);
        assert_eq!(comments[1].text, " trailing");
        assert!(!comments[1].own_line);
        assert_eq!(comments[2].text, " indented");
        assert!(comments[2].own_line);
        assert_eq!(comments[3].text, " why");
        assert!(!comments[3].own_line);
    }

    #[test]
    fn parse_to_ast_all_bounds_error_output() {
        // Hundreds of semantic errors must not produce an unbounded list — the
        // output is capped at MAX_ERRORS like the parser's (RFC 0004 §9).
        let mut src = String::from("service App:\n");
        for i in 0..400 {
            src.push_str(&format!("    k{i} = 9.999 USD\n"));
        }
        let (_ast, errors) = parse_to_ast_all(&src);
        assert!(errors.len() <= MAX_ERRORS, "unbounded error output: {}", errors.len());
    }

    #[test]
    fn parse_to_ast_all_reports_every_error_position_sorted() {
        // Two semantic (money, secret namespace) + one syntactic (`@@@`) — all
        // surfaced at once (exceeding legacy's first-error-only), position-sorted.
        let src = "service App:\n    p = 9.999 USD\n    q = $NOPE.X\n    @@@\n";
        let (_ast, errors) = parse_to_ast_all(src);
        assert!(errors.len() >= 3, "expected ≥3 errors, got {}: {errors:?}", errors.len());
        assert!(
            errors.windows(2).all(|w| w[0].span().start <= w[1].span().start),
            "errors must be position-sorted"
        );
        // And the single-error drop-in returns exactly the first of them.
        assert_eq!(parse_to_ast(src).unwrap_err().span().start, errors[0].span().start);
    }

    #[test]
    fn parse_to_ast_reports_first_error_by_position() {
        // A semantic error (money precision, line 2) *before* a syntactic one
        // (`@@@`, line 3). The merge sorts syntactic + semantic errors by source
        // position, so the drop-in reports the EARLIER (money) error.
        let src = "service App:\n    p = 9.999 USD\n    @@@\n";
        let err = parse_to_ast(src).unwrap_err();
        assert!(
            err.message().contains("decimal places"),
            "should report the earlier money error, got: {}",
            err.message()
        );
    }

    /// RFC 0004 §11.1: the retention rule. `Parse` wraps the `Send + Sync` green
    /// tree, so consumers (nudge's async runtime, the LSP) may hold it across
    /// threads/awaits; the `!Send` red `SyntaxNode` is materialized locally and
    /// never retained. This compiles only if the invariant holds for real.
    #[test]
    fn parse_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Parse>();
    }

    fn tree_text(p: &Parse) -> String {
        p.syntax().text().to_string()
    }

    fn count_kind(root: &SyntaxNode, kind: SyntaxKind) -> usize {
        root.descendants_with_tokens()
            .filter(|e| e.kind() == kind)
            .count()
    }

    /// Losslessness is the foundational invariant: assert it on every example.
    fn assert_lossless(src: &str) -> Parse {
        let p = parse(src);
        assert_eq!(tree_text(&p), src, "tree text must equal source");
        p
    }

    /// Valid input: lossless, error-free, *and structurally sound* — no `Error`
    /// nodes, and no non-trivia token orphaned directly under `Root` (which would
    /// signal a grammar gap that losslessness alone cannot catch).
    fn parse_ok(src: &str) -> Parse {
        let p = assert_lossless(src);
        assert!(p.errors().is_empty(), "unexpected errors for {src:?}: {:?}", p.errors());
        let root = p.syntax();
        assert_eq!(count_kind(&root, SyntaxKind::Error), 0, "no Error nodes for {src:?}");
        let orphan = root
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| !t.kind().is_trivia());
        assert!(!orphan, "non-trivia token orphaned under Root for {src:?}");
        p
    }

    fn has(root: &SyntaxNode, kind: SyntaxKind) -> bool {
        count_kind(root, kind) > 0
    }

    #[test]
    fn parses_clean_block_losslessly() {
        let src = "service App:\n    port = 8080\n    host = \"localhost\"\n";
        let p = assert_lossless(src);
        assert!(p.errors().is_empty(), "errors: {:?}", p.errors());

        let root = p.syntax();
        let block = block_decls(&root).next().expect("one block");
        assert_eq!(block.keyword().unwrap().text(), "service");
        assert_eq!(block.name().as_deref(), Some("App"));
        let body = block.body().expect("body");
        assert_eq!(count_kind(&body, SyntaxKind::Property), 2);
    }

    #[test]
    fn parses_nested_block() {
        let src = "service App:\n    db:\n        port = 5432\n";
        let p = assert_lossless(src);
        assert!(p.errors().is_empty(), "errors: {:?}", p.errors());
        let root = p.syntax();
        assert_eq!(count_kind(&root, SyntaxKind::NestedBlock), 1);
        assert_eq!(count_kind(&root, SyntaxKind::Property), 1);
    }

    #[test]
    fn multiline_string_suppresses_layout() {
        // The triple-quoted value contains deeper indentation and a `key:` line;
        // none of it must become tree structure — it is one String token.
        let src = "service App:\n    note = \"\"\"\n        not: indentation\n        still string\n\"\"\"\n    port = 1\n";
        let p = assert_lossless(src);
        assert!(p.errors().is_empty(), "errors: {:?}", p.errors());

        let root = p.syntax();
        // Exactly one String token, covering the whole multi-line literal.
        assert_eq!(count_kind(&root, SyntaxKind::String), 1);
        // Two properties (note, port) — the string's inner lines added none.
        assert_eq!(count_kind(&root, SyntaxKind::Property), 2);
        // The deeper indentation inside the string produced no extra Indent.
        // (Body opens once for the block + the string contributes no layout.)
        assert_eq!(count_kind(&root, SyntaxKind::Indent), 1);
    }

    #[test]
    fn recovers_and_collects_all_errors() {
        // Two broken lines, then a valid property: resilience must keep going and
        // still parse `port`, collecting every error (RFC 0004 §2 all-errors).
        let src = "service App:\n    = bad\n    @@@\n    port = 8080\n";
        let p = assert_lossless(src);
        assert!(
            p.errors().len() >= 2,
            "expected multiple errors, got {:?}",
            p.errors()
        );
        let root = p.syntax();
        // The valid property after the errors is still recovered.
        let has_port = root
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::Property)
            .any(|prop| first_ident(&prop).is_some_and(|t| t.text() == "port"));
        assert!(has_port, "valid `port` property should survive recovery");
    }

    #[test]
    fn inconsistent_dedent_recovers() {
        // 8-space body, then a 4-space line matching no open level.
        let src = "service App:\n        a = 1\n    b = 2\n";
        let p = assert_lossless(src);
        assert!(
            p.errors().iter().any(|e| e.message().contains("dedent")),
            "expected an inconsistent-dedent diagnostic, got {:?}",
            p.errors()
        );
    }

    #[test]
    fn empty_and_trivia_only_inputs_are_lossless() {
        for src in ["", "\n", "   \n\n", "// just a comment\n", "   // x"] {
            let p = assert_lossless(src);
            // No declarations, but always a tree.
            assert_eq!(block_decls(&p.syntax()).count(), 0);
        }
    }

    #[test]
    fn ok_surfaces_all_errors_for_strict_callers() {
        let clean = parse("service App:\n    port = 1\n");
        assert!(clean.ok().is_ok());

        let broken = parse("service App:\n    @@@\n    %%%\n");
        match broken.ok() {
            Ok(_) => panic!("strict caller must reject invalid input"),
            Err(errors) => assert!(errors.len() >= 2, "all errors, not just the first"),
        }
    }

    /// RFC 0006: `=>` is rejected with the one-character fix named, the
    /// parse recovers (every stale arrow surfaces in a single pass), and
    /// the arms still lower — so `nml fmt` on strict=false pipelines can
    /// even auto-heal a legacy file into `->`.
    #[test]
    fn legacy_fat_arrow_gets_guidance_and_recovers() {
        let src = "oneof email by provider:\n    \"log\" => emailLog\n    \"postmark\" => emailPostmark\n";
        let parsed = parse(src);
        // Recovery is lossless and kept the structure: both arms present.
        assert_eq!(tree_text(&parsed), src, "rejected-arrow file round-trips");
        assert_eq!(count_kind(&parsed.syntax(), SyntaxKind::OneOfArm), 2);
        // The rejected token lives under an Error node, like every recovery.
        assert!(has(&parsed.syntax(), SyntaxKind::Error));
        match parsed.ok() {
            Ok(_) => panic!("'=>' must be rejected"),
            Err(errors) => {
                assert_eq!(errors.len(), 2, "one guidance error per stale arrow");
                for e in &errors {
                    assert!(
                        e.to_string().contains("'=>' was replaced by '->'"),
                        "guidance must name the fix: {e}"
                    );
                }
            }
        }

        // Truncated arm: a stale arrow as the final token (no model name)
        // still recovers without panic — guidance plus the cascading
        // missing-name error, and the tree stays lossless.
        let truncated = "oneof email by provider:\n    \"log\" =>";
        let parsed = parse(truncated);
        assert_eq!(tree_text(&parsed), truncated, "truncated arm round-trips");
        let errors = parsed.ok().expect_err("truncated stale arm must error");
        assert!(
            errors.iter().any(|e| e.to_string().contains("'=>' was replaced by '->'")),
            "guidance survives truncation"
        );
    }

    #[test]
    fn declarations_const_template_oneof_array() {
        let const_d = parse_ok("const MaxRetries = 3\n");
        assert!(has(&const_d.syntax(), SyntaxKind::ConstDecl));

        let tmpl = parse_ok("template Greeting:\n    \"Hello, world\"\n");
        assert!(has(&tmpl.syntax(), SyntaxKind::TemplateDecl));

        let oneof = parse_ok(
            "oneof email by provider as providerKind = \"log\":\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n",
        );
        let root = oneof.syntax();
        assert!(has(&root, SyntaxKind::OneOfDecl));
        assert_eq!(count_kind(&root, SyntaxKind::OneOfArm), 2);

        let array = parse_ok("[]route myRoutes:\n    - Home\n    - About\n");
        assert!(has(&array.syntax(), SyntaxKind::ArrayDecl));
    }

    #[test]
    fn field_definitions_all_forms() {
        let p = parse_ok(
            "model Plan:\n    name string\n    tier string?\n    region string = \"us\"\n    tags []string\n    mode (active | inactive)\n",
        );
        let root = p.syntax();
        assert_eq!(count_kind(&root, SyntaxKind::FieldDef), 5);
        // `[]string` and `(active | inactive)` are nested type expressions.
        assert!(count_kind(&root, SyntaxKind::TypeExpr) >= 7);
    }

    #[test]
    fn value_forms_money_negative_fallback_array_secret_role() {
        let p = parse_ok(
            "service App:\n    port = 8080\n    price = 100 USD\n    temp = -5\n    host = primary | \"localhost\"\n    key = $ENV.SECRET\n    owner = @role/admin\n    enabled = true\n    tags = [\"x\", \"y\"]\n",
        );
        let root = p.syntax();
        assert!(has(&root, SyntaxKind::Fallback), "fallback chain");
        assert!(has(&root, SyntaxKind::ArrayValue), "array literal");
        // Money is Number + currency Ident inside one Value (syntactic only).
        assert!(has(&root, SyntaxKind::Value));
    }

    #[test]
    fn entries_modifiers_shared_nested_list() {
        let p = parse_ok(
            "service App is Base, Mixin:\n    db:\n        timeout = 30\n    |visibility = \"public\"\n    |allow:\n        - @role/admin\n        - Guest\n    .defaults:\n        retries = 3\n    .region = \"us\"\n",
        );
        let root = p.syntax();
        assert!(has(&root, SyntaxKind::Extends));
        assert!(has(&root, SyntaxKind::NestedBlock));
        assert_eq!(count_kind(&root, SyntaxKind::Modifier), 2);
        assert_eq!(count_kind(&root, SyntaxKind::SharedProperty), 2);
        assert_eq!(count_kind(&root, SyntaxKind::ListItem), 2);
    }

    #[test]
    fn list_item_forms_named_shorthand_reference() {
        let p = parse_ok("[]route r:\n    - Home:\n        path = \"/\"\n    - \"shorthand\"\n    - SomeRef\n");
        assert_eq!(count_kind(&p.syntax(), SyntaxKind::ListItem), 3);
    }

    #[test]
    fn core_model_fixture_parses_and_extracts_clean() {
        // The repurposed RFC 0005 fixture parses + extracts with no errors, and its
        // `!` / `?!` markers land. Guards the fixture against rot.
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/models/core.model.nml"
        ))
        .unwrap();
        parse_ok(&src); // no parse errors, lossless
        let (schema, errors) = crate::cst::extract_schema(&src);
        assert!(errors.is_empty(), "fixture should extract clean: {errors:?}");

        let field = |model: &str, field: &str| {
            schema
                .models
                .iter()
                .find(|m| m.name == model)
                .and_then(|m| m.fields.iter().find(|f| f.name == field))
                .unwrap_or_else(|| panic!("{model}.{field} missing"))
        };
        assert!(field("resource", "path").shorthand);
        assert!(field("role", "name").shorthand);
        let run = field("command", "run");
        assert!(run.shorthand && run.optional, "`run string?!` is shorthand + optional");
    }

    #[test]
    fn scalar_list_item_with_body_parses_clean_and_lossless() {
        // `- "/admin":` + indented body (scalar-key-with-body, RFC 0005 §9). parse_ok
        // asserts no errors + losslessness; the item carries a scalar value and a body.
        let p = parse_ok("[]resource resources:\n    - \"/admin\":\n        method = \"POST\"\n");
        assert_eq!(count_kind(&p.syntax(), SyntaxKind::ListItem), 1);
    }

    #[test]
    fn kitchen_sink_parses_clean_and_lossless() {
        // Every construct in one document: the strongest completeness check.
        let src = "\
const MaxRetries = 3

template Greeting:
    \"Hello\"

oneof email by provider:
    \"log\" -> emailLog

model Plan:
    name string
    tier string?
    tags []string
    mode (active | inactive)

service App is Base:
    port = 8080
    price = 100 USD
    host = $ENV.HOST | \"localhost\"
    tags = [\"a\", \"b\"]
    db:
        timeout = 30
    |allow:
        - @role/admin
    .region = \"us\"
";
        let p = parse_ok(src);
        let root = p.syntax();
        for kind in [
            SyntaxKind::ConstDecl,
            SyntaxKind::TemplateDecl,
            SyntaxKind::OneOfDecl,
            SyntaxKind::BlockDecl,
            SyntaxKind::FieldDef,
            SyntaxKind::Property,
            SyntaxKind::Modifier,
            SyntaxKind::SharedProperty,
            SyntaxKind::Fallback,
            SyntaxKind::ArrayValue,
        ] {
            assert!(has(&root, kind), "kitchen sink should contain {kind:?}");
        }
    }

    /// Grammar completeness: a corpus of representative *valid* inputs across every
    /// construct must parse clean and lossless. The strongest "is the grammar
    /// complete" check.
    /// The value node under the first `Property` in a parsed document.
    fn first_value_node(root: &SyntaxNode) -> SyntaxNode {
        root.descendants()
            .find(|n| n.kind() == SyntaxKind::Property)
            .expect("a property")
            .children()
            .find(|c| {
                matches!(
                    c.kind(),
                    SyntaxKind::Value | SyntaxKind::ArrayValue | SyntaxKind::Fallback
                )
            })
            .expect("a value node")
    }

    /// Wrap a value expression in a block property (properties live in blocks).
    fn wrap(value_expr: &str) -> String {
        format!("service App:\n    k = {value_expr}\n")
    }

    fn decode_first(value_expr: &str) -> crate::types::Value {
        let src = wrap(value_expr);
        decode_value(&first_value_node(&parse(&src).syntax()))
            .expect("decode")
            .value
    }

    #[test]
    fn value_decode_scalars_correct() {
        use crate::types::{Number, Value};
        assert_eq!(decode_first("\"a\\nb\\t!\""), Value::String("a\nb\t!".into()));
        assert_eq!(decode_first("42"), Value::Number(Number::Int(42)));
        assert_eq!(decode_first("-5"), Value::Number(Number::Int(-5)));
        assert_eq!(decode_first("2.5"), Value::Number(Number::Float(2.5)));
        assert_eq!(decode_first("true"), Value::Bool(true));
        assert_eq!(decode_first("false"), Value::Bool(false));
        assert_eq!(decode_first("GroqFast"), Value::Reference("GroqFast".into()));
        assert_eq!(decode_first("@role/admin"), Value::Role("@role/admin".into()));
        assert_eq!(decode_first("$ENV.X"), Value::Secret("$ENV.X".into()));
        assert!(matches!(decode_first("100 USD"), Value::Money(_)));
        assert!(matches!(decode_first("\"hi {{n}}\""), Value::TemplateString(_)));
    }

    #[test]
    fn value_decode_multiline_dedent() {
        use crate::types::Value;
        // Common leading indent stripped; blank first/last lines trimmed.
        let v = decode_first("\"\"\"\n    Hello\n    World\n    \"\"\"");
        assert_eq!(v, Value::String("Hello\nWorld".into()));
    }

    #[test]
    fn value_decode_array_and_fallback_structure() {
        use crate::types::Value;
        match decode_first("[\"a\", \"b\", \"c\"]") {
            Value::Array(items) => assert_eq!(items.len(), 3),
            other => panic!("expected array, got {other:?}"),
        }
        // Nested arrays decode element-wise (each element is itself an array).
        match decode_first("[[1, 2], [3, 4]]") {
            Value::Array(rows) => {
                assert_eq!(rows.len(), 2);
                assert!(rows.iter().all(|r| matches!(r.value, Value::Array(_))));
            }
            other => panic!("expected nested array, got {other:?}"),
        }
        // `a | b | c` is right-associative: Fallback(a, Fallback(b, c)).
        match decode_first("a | b | c") {
            Value::Fallback(first, rest) => {
                assert_eq!(first.value, Value::Reference("a".into()));
                assert!(matches!(rest.value, Value::Fallback(_, _)));
            }
            other => panic!("expected fallback, got {other:?}"),
        }
    }

    #[test]
    fn escape_error_span_is_char_precise() {
        // value: "ok \q bad" — `\q` is an invalid escape.
        let src = wrap("\"ok \\q bad\"");
        let node = first_value_node(&parse(&src).syntax());
        let cst_span = decode_value(&node).unwrap_err().span();
        // The diagnostic underlines exactly the offending `\q`, not the whole value.
        assert_eq!(&src[cst_span.start..cst_span.end], "\\q");
    }

    #[test]
    fn value_decode_rejects_unknown_secret_namespace() {
        let src = wrap("$NOPE.X");
        let node = first_value_node(&parse(&src).syntax());
        let err = decode_value(&node).unwrap_err();
        assert!(err.message().contains("unknown variable source"), "{}", err.message());
    }

    #[test]
    fn valid_corpus_parses_clean() {
        let corpus = [
            // enums (list items), traits/models (field defs)
            "enum Status:\n    - active\n    - inactive\n    - pending\n",
            "trait Auditable:\n    createdAt string\n    updatedAt string\n",
            "model User:\n    name string\n    age number\n",
            // value forms
            "service App:\n    greeting = \"Hello {{args.name}}\"\n",
            "service App:\n    dir = \"./static\"\n",
            "service App:\n    provider = GroqFast\n",
            "service App:\n    role = @role/admin\n",
            "service S:\n    price = 100.00 USD\n",
            "service S:\n    a = -5\n    b = 3.14\n",
            "service S:\n    key = $ENV.KEY | \"default\"\n",
            // multiline string with internal indentation
            "service App:\n    bio = \"\"\"\n    Hello\n    World\n    \"\"\"\n",
            // arrays, modifiers, shared properties, named list items
            "[]mount mounts:\n    |allow = [@authenticated]\n    - Main:\n        path = \"/\"\n",
            "workflow W:\n    .defaults:\n        retries = 3\n    - step1:\n        x = 1\n",
            "workflow W:\n    .interval = 7200\n    - step1:\n        x = 1\n",
            // role list items with rich role syntax
            "role admin:\n    members:\n        - @role/editor\n        - @user/test@example.com\n",
            // discriminated unions
            "oneof email by provider:\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n",
            // const + template
            "const MaxRetries = 3\n",
            "template Greeting:\n    \"Hello\"\n",
        ];
        for src in corpus {
            parse_ok(src);
        }
    }

    #[test]
    fn currency_does_not_cross_newline() {
        // A 3-uppercase identifier starting the next line is its own entry, not
        // the previous number's currency code.
        let p = parse_ok("service App:\n    count = 5\n    USD = 1\n");
        let root = p.syntax();
        assert_eq!(count_kind(&root, SyntaxKind::Property), 2);
        // `count` decodes to a plain number, not money.
        assert!(matches!(decode_first("5"), crate::types::Value::Number(_)));
        // Same-line currency still parses as money.
        assert!(matches!(decode_first("5 USD"), crate::types::Value::Money(_)));
    }

    #[test]
    fn body_entry_sequence_is_structurally_correct() {
        // Order *and* kind of every entry in the outer block body must be right
        // — losslessness can't catch a mis-typed or mis-nested entry.
        let src = "service App is Base, Mixin:\n    db:\n        timeout = 30\n    |visibility = \"public\"\n    |allow:\n        - @role/admin\n    .defaults:\n        retries = 3\n    .region = \"us\"\n    port = 8080\n";
        let root = parse_ok(src).syntax();
        let body = root
            .descendants()
            .find(|n| n.kind() == SyntaxKind::Body)
            .expect("a body");
        let kinds: Vec<SyntaxKind> = body.children().map(|n| n.kind()).collect();
        assert_eq!(
            kinds,
            vec![
                SyntaxKind::NestedBlock,    // db:
                SyntaxKind::Modifier,       // |visibility = ...
                SyntaxKind::Modifier,       // |allow:
                SyntaxKind::SharedProperty, // .defaults:
                SyntaxKind::SharedProperty, // .region = ...
                SyntaxKind::Property,       // port = 8080
            ]
        );
        // `is Base, Mixin` is an Extends on the *declaration*, not in the body.
        assert!(root
            .descendants()
            .find(|n| n.kind() == SyntaxKind::BlockDecl)
            .unwrap()
            .children()
            .any(|n| n.kind() == SyntaxKind::Extends));
    }

    #[test]
    fn valid_edge_cases_parse_clean() {
        let corpus = [
            "service App:\n",                                    // empty body
            "service App:\n    tags = []\n",                     // empty array literal
            "service App:\n    matrix = [[1, 2], [3, 4]]\n",     // nested arrays
            "service App:\n    greeting =\n        \"hi\"\n",     // indented (block) value
            "model M:\n    f string?\n    g (a | b)?\n",         // optional field + optional union
            "service App:\n    |field string?\n",                // modifier type annotation
            "service App:\n    items = [1, 2, 3,]\n",            // trailing comma in array
            "const C = a | b | c\n",                             // const with fallback chain
            "[]x ys:\n    .shared = 1\n    - One:\n        a = 1\n", // array decl: shared + named item
        ];
        for src in corpus {
            parse_ok(src);
        }
    }

    #[test]
    fn indented_value_decodes() {
        // The block-form value (`= <newline> <indent> value`) must decode like
        // an inline value.
        let src = "service App:\n    greeting =\n        \"hi\"\n";
        let v = decode_value(&first_value_node(&parse(src).syntax())).unwrap().value;
        assert_eq!(v, crate::types::Value::String("hi".into()));
    }

    #[test]
    fn field_type_does_not_cross_newline() {
        // `flag` has no same-line type, so it is incomplete (an error); `other`
        // is a *separate* property — the field-def detector must not consume the
        // next line's identifier as `flag`'s type.
        let p = assert_lossless("model M:\n    flag\n    other = 1\n");
        assert!(!p.errors().is_empty(), "bare `flag` should be an error");
        let root = p.syntax();
        // `other = 1` survives as its own property (not swallowed as a type).
        let has_other = root
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::Property)
            .any(|prop| first_ident(&prop).is_some_and(|t| t.text() == "other"));
        assert!(has_other, "`other = 1` must be a separate property");
        // And it was *not* mis-parsed as a single field definition.
        assert_eq!(count_kind(&root, SyntaxKind::FieldDef), 0);
    }

    #[test]
    fn deeply_nested_values_are_bounded() {
        // Nested arrays and union types must be depth-bounded too (not just
        // blocks) — `depth_guarded_type` covers both.
        let arr = format!("x:\n    v = {}{}\n", "[".repeat(400), "]".repeat(400));
        let p = assert_lossless(&arr);
        assert!(p.errors().iter().any(|e| e.message().contains("depth")));

        let ty = format!("model M:\n    f {}int\n", "[]".repeat(400));
        let p2 = assert_lossless(&ty);
        assert!(p2.errors().iter().any(|e| e.message().contains("depth")));
    }

    #[test]
    fn deep_nesting_is_bounded_no_stack_overflow() {
        // Adversarial: hundreds of ever-deeper nested blocks. Without a depth
        // guard this overflows the stack (RFC 0004 §9). With it, recursion caps
        // and the over-deep tail is consumed iteratively.
        let mut src = String::new();
        for level in 0..600 {
            for _ in 0..level {
                src.push(' ');
            }
            src.push_str("a:\n");
        }
        let p = parse(&src); // must not overflow or panic
        assert_eq!(tree_text(&p), src, "lossless on deeply-nested input");
        assert!(
            p.errors().iter().any(|e| e.message().contains("depth")),
            "depth guard should fire past the limit"
        );
    }

    /// Fuzz/property harness (RFC 0004 §9): on *any* input the parser must
    /// terminate, never panic, stay byte-lossless, and keep the error list
    /// bounded. A small deterministic LCG drives it (no external deps).
    #[test]
    fn fuzz_termination_losslessness_bounded() {
        // Covers every token-triggering character class introduced in P1 plus
        // adversarial bytes (null, a control char, a 4-byte emoji, a non-`\n`
        // Unicode line separator) that exercise the lexer's catch-all/`utf8_len`
        // path — so losslessness/termination/no-panic hold across the *whole* byte
        // space, not just the syntactic subset (RFC 0004 §9).
        let alphabet = [
            "service", "App", ":", "=", "=>", "->", " ", "    ", "\n", "\r\n", "\"", "\"\"\"", "\\",
            "//c", "x", "1", "12.5", "@role/x", "$ENV.K", "-", "|", ".", "[", "]", "(", ")", ",",
            "?", "\t", "é", "\0", "\u{1}", "🎉", "\u{2028}",
        ];
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..4000 {
            let len = (next() % 40) as usize;
            let mut src = String::new();
            for _ in 0..len {
                src.push_str(alphabet[(next() as usize) % alphabet.len()]);
            }
            let p = parse(&src); // must not panic
            assert_eq!(tree_text(&p), src, "lossless on adversarial input: {src:?}");
            assert!(p.errors().len() <= MAX_ERRORS, "error list stays bounded");

            // Decode is reachable from untrusted input (nudge decodes tenant
            // config values), so it must never panic on *any* tree — only return
            // Ok/Err. Decode every value node the fuzzed parse produced.
            for node in p.syntax().descendants() {
                if matches!(
                    node.kind(),
                    SyntaxKind::Value | SyntaxKind::ArrayValue | SyntaxKind::Fallback
                ) {
                    let _ = decode_value(&node);
                }
            }

            // Schema extraction and the semantic-AST lowering are also input-reachable
            // and recurse (resolve_field_type / nested bodies); neither may panic.
            use ast::AstNode as _;
            if let Some(root) = ast::Root::cast(p.syntax()) {
                let _ = extract::extract(&root);
                let _ = lower::to_ast(&root);
            }
            // The public drop-ins compose parse + lower (+ comment extraction +
            // schema extraction); prove they are panic-free on any source too.
            let _ = parse_to_ast(&src);
            let _ = parse_with_comments(&src);
            let _ = extract_schema(&src);
        }
    }
}
