//! `nml fmt` — a trivia-aware printer over the lossless CST (RFC 0004).
//!
//! # Why the tree, and not the AST
//!
//! The formatter used to re-emit from the semantic AST with a side list of
//! comments. That substrate cannot hold the thing being formatted: the AST
//! has no blank lines, no record of whether the author put a value on the
//! `=`'s line or the next one, and no idea where a comment sits relative to
//! either. Worse, an AST re-emitter is a silent data-loss machine by
//! construction — every construct must remember to re-render itself or it
//! is deleted from the author's file on the next run.
//!
//! This printer walks the lossless tree instead, and the two hazards go
//! away by construction:
//!
//! * **Content is copied, never reconstructed.** Every significant token is
//!   written from the tree. A grammar production this file has never heard
//!   of still prints, because spacing is decided by adjacent token KINDS,
//!   not by a match on node types that a new feature could forget to join.
//! * **Layout is computed, never copied.** Indentation, line breaks that
//!   the grammar requires, and the blank run between two lines are
//!   generated — with the author's own trivia in hand at every decision, so
//!   the decisions that belong to the author (a blank line, a line break
//!   the grammar leaves free, an aligned column) can be honoured rather
//!   than guessed at from byte offsets.
//!
//! The one exception to "content is copied" is the closed, enumerated set
//! of spellings the specification makes normative — digit separators, a
//! duration's spacing and component order, money's spacing, a multi-line
//! string's edge-space protection. Those are rendered from the decoded
//! value by the crate's one value renderer, which is the ONLY place in
//! this file that rewrites a token's text.
//!
//! The written style specification is `spec/style.md`; every policy
//! decision below cites its section.

use crate::printer::Printer;
use nml_core::cst::{self, INDENT_UNIT, SyntaxKind, SyntaxNode, SyntaxToken};
use nml_core::error::{NmlError, NmlResult};
use nml_core::source_policy::string_literal;
use nml_core::template;
use nml_core::types::Value;

/// Parse and format NML source text into canonical form — the crate's
/// whole public API.
///
/// Source text in, source text out: the caller owns the write (write
/// atomically — temp file plus rename in the target directory — so a crash
/// mid-write cannot truncate a user's config).
///
/// Guaranteed for every document this accepts (`spec/style.md` §8): the
/// output parses; formatting is idempotent; the lowered tree is unchanged
/// apart from spans; every significant token and every comment survives in
/// order with its text unchanged, bar the spellings §5 makes normative; no
/// string or template value changes; nothing is duplicated; and a CRLF file
/// stays a CRLF file.
///
/// Refuses an invalid document rather than writing a guess back over it
/// (`gofmt` and `rustfmt` refuse for the same reason): the caller leaves the
/// file untouched and reports the error. Fix the errors first — `nml check`
/// names them, `nml fix` repairs the machine-applicable ones.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use nml_fmt::formatter::format_source;
///
/// // The author's blank line, and their choice to put the value on the
/// // next line, are theirs: both survive.
/// let source = "const A = 1\n\nconst B =\n    2\n";
/// assert_eq!(format_source(source)?, source);
///
/// // A document that does not parse is refused, not guessed at.
/// assert!(format_source("service Api:\n  = 1\n").is_err());
/// # Ok(())
/// # }
/// ```
pub fn format_source(source: &str) -> NmlResult<String> {
    let tree = cst::parse_checked(source)?;
    let mut walk = Walk::new();
    walk.node(&tree, 0);
    match walk.error {
        Some(e) => Err(e),
        None => Ok(walk.out.finish(cst::line_terminator(source))),
    }
}

/// The tree walk. Depth is carried down rather than counted from the
/// `Indent`/`Dedent` markers in the token stream, because the parser flushes
/// a body's leading trivia in FRONT of its `Indent` (RFC 0004 §4.3): a
/// comment above a block's first entry would otherwise print at the
/// enclosing depth. A node that opens an indentation level is exactly a node
/// with an `Indent` child, which is a fact of the tree — no list of node
/// kinds to keep in step with the grammar.
struct Walk {
    out: Printer,
    /// The previous significant token on the line under construction.
    prev: Option<SyntaxToken>,
    /// The whitespace run just consumed, with CR (transport, not content)
    /// removed — the author's gap, for the columns §4 lets them keep.
    gap: Option<String>,
    /// The first decode failure. Unreachable after [`cst::parse_checked`]
    /// (a value that does not decode is one of the errors it refuses), so
    /// this is a total fallback rather than a live path — and the reason
    /// this file needs no `expect` on document data.
    error: Option<NmlError>,
}

/// Whether a node's indentation level begins at its FIRST child rather than
/// at its `Indent` marker. The parser flushes a body's leading trivia in
/// front of that marker (RFC 0004 §4.3), so a comment written above a
/// block's first entry sits before the `Indent` and inside the block — it
/// must print one level in, not at the enclosing depth. The test is exactly
/// "nothing but trivia precedes the marker", which distinguishes a `Body`
/// (opens at its first child) from a `ConstDecl` or `Property` whose value
/// the author put on the next line (`const X =` stays at its own depth; only
/// the value moves in).
fn indent_opens_at_first_child(node: &SyntaxNode) -> bool {
    for child in node.children_with_tokens() {
        match child.kind() {
            SyntaxKind::Indent => return true,
            k if k.is_trivia() => {}
            _ => return false,
        }
    }
    false
}

/// The nodes that render as ONE unit from their decoded value
/// (`spec/style.md` §5) rather than token by token: the three value nodes,
/// and the bare `DurationLiteral` a numeric facet takes as its bound
/// (`duration(max = 1h30m)`), which has no `Value` wrapper to decode
/// through but the same normative spelling.
fn renders_as_a_unit(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Value
            | SyntaxKind::ArrayValue
            | SyntaxKind::Fallback
            | SyntaxKind::DurationLiteral
    )
}

impl Walk {
    fn new() -> Self {
        Self {
            out: Printer::new(),
            prev: None,
            gap: None,
            error: None,
        }
    }

    fn node(&mut self, node: &SyntaxNode, depth: usize) {
        if renders_as_a_unit(node.kind()) {
            self.unit(node, depth);
            return;
        }
        // Depth is a running count of the tree's own layout markers, with
        // the first one pulled forward over a body's leading trivia.
        let mut pending = indent_opens_at_first_child(node);
        let mut depth = depth + usize::from(pending);
        for child in node.children_with_tokens() {
            if let Some(tok) = child.as_token() {
                match tok.kind() {
                    SyntaxKind::Indent => {
                        if pending {
                            pending = false;
                        } else {
                            depth += 1;
                        }
                    }
                    SyntaxKind::Dedent => depth = depth.saturating_sub(1),
                    _ => self.token(tok, depth),
                }
            } else if let Some(sub) = child.as_node() {
                self.node(sub, depth);
            }
        }
    }

    fn token(&mut self, tok: &SyntaxToken, depth: usize) {
        match tok.kind() {
            // The zero-width end sentinel carries no text. The layout
            // markers never reach here: the walk consumes them.
            SyntaxKind::Eof => {}
            SyntaxKind::Whitespace => {
                // A CR is inter-token whitespace in a CRLF file — transport,
                // never a gap the author typed.
                let text = tok.text().replace('\r', "");
                self.gap = if text.is_empty() { None } else { Some(text) };
                return;
            }
            SyntaxKind::Newline => {
                self.out.end_line();
                self.prev = None;
            }
            SyntaxKind::Comment => self.comment(tok, depth),
            _ => {
                // The line opens BEFORE the token is spelled: a multi-line
                // string's body sits under ITS OWN line, and `body_depth`
                // reads the line under construction. Spelling first left an
                // own-line `"""` selector's body at the PREVIOUS line's
                // depth — the delimiter moved, the body stayed.
                if self.out.at_line_start() {
                    self.out.open_line(depth);
                }
                let text = self.token_text(tok);
                self.write(tok.kind(), Some(tok), &text, depth);
            }
        }
        self.gap = None;
    }

    /// A comment that opens a line stays an own-line comment at the depth of
    /// the body the TREE says it is in; a comment that follows code on its
    /// line stays there, behind the gap its author wrote (`spec/style.md`
    /// §3, §4). Neither can be reordered against the code around it: a
    /// comment is a token in the same stream.
    fn comment(&mut self, tok: &SyntaxToken, depth: usize) {
        let text = tok.text().trim_end();
        if self.out.at_line_start() {
            self.out.open_line(depth);
        } else {
            let gap = self.authored_gap();
            self.out.push(&gap);
        }
        self.out.push(text);
    }

    /// Write `text` as the next thing on the current line, behind whatever
    /// separator §4 calls for.
    fn write(&mut self, kind: SyntaxKind, tok: Option<&SyntaxToken>, text: &str, depth: usize) {
        if self.out.at_line_start() {
            self.out.open_line(depth);
        } else {
            let sep = self.separator(kind, tok);
            self.out.push(&sep);
        }
        self.out.push(text);
        self.prev = tok.cloned();
    }

    /// The gap the author left, at least one space. The rule behind the two
    /// columns §4 keeps (`->` and a trailing comment): the formatter never
    /// invents alignment and never destroys it.
    fn authored_gap(&self) -> String {
        match self.gap.as_deref() {
            Some(g) if !g.is_empty() => g.to_string(),
            _ => " ".to_string(),
        }
    }

    /// The separator between the previous significant token on this line and
    /// the next one (`spec/style.md` §4). A function of the two token KINDS,
    /// with a parent-kind tie-break for the four tokens the grammar gives
    /// two jobs: `|` (modifier sigil vs. union/fallback operator), `-` (list
    /// bullet vs. sign), `(` (facet or directive list vs. parenthesised
    /// type) and `>` (type-argument close).
    fn separator(&self, next: SyntaxKind, next_tok: Option<&SyntaxToken>) -> String {
        use SyntaxKind as K;
        let Some(prev) = self.prev.as_ref() else {
            return String::new();
        };
        // The ARM arrow opens a right-hand column: the author's gap stands
        // (§4). The same token inside a type is the map constructor
        // (`(role -> string)`), where there is no column to keep and §4's
        // canonical spacing applies — the fifth token the grammar gives two
        // jobs, beside `|`, `-`, `(` and the angle brackets.
        if next == K::Arrow {
            let arm = next_tok
                .and_then(SyntaxToken::parent)
                .is_some_and(|p| matches!(p.kind(), K::OneOfArm | K::Arm));
            return if arm {
                self.authored_gap()
            } else {
                " ".to_string()
            };
        }
        if matches!(
            next,
            K::Colon | K::Comma | K::Question | K::Plus | K::Gt | K::Lt | K::RParen | K::RBracket
        ) {
            return String::new();
        }
        if matches!(
            prev.kind(),
            K::LBracket | K::LParen | K::Lt | K::Dot | K::Hash
        ) {
            return String::new();
        }
        if prev.kind() == K::RBracket {
            // An array TYPE's brackets bind to the element type
            // (`[]string`) and to a declaration's name (`[]validator vs:`).
            // An array VALUE's `]` closes a value, and what can follow it
            // on the line — a directive — is a separate thing:
            // `xs []number = [1, 2] #live`. The sixth token the grammar
            // gives two jobs.
            let a_type = prev
                .parent()
                .is_some_and(|p| matches!(p.kind(), K::TypeExpr | K::ArrayDecl));
            return if a_type {
                String::new()
            } else {
                " ".to_string()
            };
        }
        if next == K::LParen {
            // `number(min = 1)` and `#key("h")` bind to the name before
            // them; a parenthesised TYPE is the field's type, one space
            // after the field's name.
            let bound = next_tok
                .and_then(SyntaxToken::parent)
                .is_some_and(|p| matches!(p.kind(), K::FacetList | K::Directive));
            return if bound {
                String::new()
            } else {
                " ".to_string()
            };
        }
        if prev.kind() == K::Dash {
            // `- item` is a bullet. A sign never reaches here: it lives
            // inside a value, which renders as one unit.
            let bullet = prev.parent().is_some_and(|p| p.kind() == K::ListItem);
            return if bullet {
                " ".to_string()
            } else {
                String::new()
            };
        }
        if prev.kind() == K::Pipe && prev.parent().is_some_and(|p| p.kind() == K::Modifier) {
            // `|allow` — the modifier sigil binds to its name; the same
            // token between two types or two fallback arms is an operator.
            return String::new();
        }
        " ".to_string()
    }

    /// A significant token's text. Verbatim, except where the specification
    /// makes a spelling normative: digit separators and magnitude spelling
    /// in a facet's number (`spec/syntax.md` "Number Literals": separators
    /// are spelling, never value — `10_000` → `10000`, `007` → `7`), and a
    /// string token outside a value node, which renders through the one
    /// string speller so a multi-line selector re-indents with its line.
    fn token_text(&mut self, tok: &SyntaxToken) -> String {
        match tok.kind() {
            SyntaxKind::Number => match nml_core::decimal::Number::parse_literal(tok.text()) {
                Ok(n) => n.to_string(),
                // Unreachable: a malformed number is a parse error.
                Err(_) => tok.text().to_string(),
            },
            SyntaxKind::String => {
                let body_depth = self.body_depth();
                match cst::decode_string(tok) {
                    Ok(text) => {
                        let mut out = String::new();
                        let authored_triple = Some(tok.text().starts_with("\"\"\""));
                        format_string(&mut out, &text, body_depth, true, authored_triple);
                        out
                    }
                    Err(e) => {
                        self.error.get_or_insert(e);
                        tok.text().to_string()
                    }
                }
            }
            _ => tok.text().to_string(),
        }
    }

    /// The indentation level a multi-line string's body sits at: the level
    /// of the line the opening `"""` is on when the string opens that line
    /// (the `template T:` and `const X =` forms the specification teaches),
    /// one level deeper when it follows `= ` on a line of its own content.
    /// Either way the closing delimiter aligns with the body, which is
    /// NML0020's rule.
    fn body_depth(&self) -> usize {
        self.out.line_depth() + usize::from(!self.out.at_line_start())
    }

    /// A value node renders as ONE unit from its decoded value, so the
    /// normative spellings (`spec/syntax.md`: separators, duration spacing
    /// and component order, money's space, the multi-line edge-space
    /// protection) all live in [`format_value`]. Its trivia is still walked
    /// in place, so a line break the author put between `=` and the value
    /// stands (`spec/style.md` §5) and a comment inside it keeps its seat.
    fn unit(&mut self, node: &SyntaxNode, depth: usize) {
        let mut rendered = false;
        let mut last: Option<SyntaxToken> = None;
        for tok in node
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
        {
            match tok.kind() {
                SyntaxKind::Indent | SyntaxKind::Dedent | SyntaxKind::Eof => {}
                SyntaxKind::Whitespace => {
                    let text = tok.text().replace('\r', "");
                    self.gap = if text.is_empty() { None } else { Some(text) };
                    continue;
                }
                SyntaxKind::Newline => {
                    self.out.end_line();
                    self.prev = None;
                }
                SyntaxKind::Comment => self.comment(&tok, depth),
                _ => {
                    last = Some(tok.clone());
                    if !rendered {
                        rendered = true;
                        self.render_unit(node, depth, &tok);
                    }
                }
            }
            self.gap = None;
        }
        // A following directive (`x = "a" #live`) separates from the value,
        // not from the value's first token.
        if last.is_some() {
            self.prev = last;
        }
    }

    fn render_unit(&mut self, node: &SyntaxNode, depth: usize, first: &SyntaxToken) {
        if self.out.at_line_start() {
            self.out.open_line(depth);
        } else {
            let sep = self.separator(first.kind(), Some(first));
            self.out.push(&sep);
        }
        let body_depth = self.body_depth();
        let rendered = if node.kind() == SyntaxKind::DurationLiteral {
            // RFC 0017 §6 / `spec/syntax.md`: the canonical form is attached
            // with components coarse→fine, which is what `Duration`'s own
            // display spells — the same speller a duration inside a value
            // reaches through `format_value`.
            let joined: String = cst_significant(node)
                .map(|t| t.text().to_string())
                .collect();
            nml_core::duration::Duration::parse_text(&joined)
                .ok()
                .map(|d| d.to_string())
        } else {
            match cst::decode_value(node) {
                Ok(value) => {
                    let mut text = String::new();
                    // The author's DELIMITER stands with the rest of their
                    // spelling (`spec/style.md` §5): a `"""` string whose
                    // content happens to fit one line is not collapsed into
                    // an escape-heavy `"…"`. The flags are read off the
                    // node's string tokens in source order, which is the
                    // order `format_value` renders them in.
                    let mut triple: std::collections::VecDeque<bool> = cst_significant(node)
                        .filter(|t| t.kind() == SyntaxKind::String)
                        .map(|t| t.text().starts_with("\"\"\""))
                        .collect();
                    format_value(&mut text, &value.value, body_depth, &mut triple);
                    Some(text)
                }
                Err(e) => {
                    self.error.get_or_insert(e);
                    None
                }
            }
        };
        match rendered {
            Some(text) => self.out.push(&text),
            // Unreachable after `parse_checked` (a value that does not
            // decode is one of the errors it refuses); the source tokens are
            // the safe fallback while the error travels to the caller.
            None => {
                for tok in cst_significant(node) {
                    self.out.push(tok.text());
                }
            }
        }
    }
}

/// A node's significant tokens in source order.
fn cst_significant(node: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + '_ {
    node.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind().is_significant())
}
/// Render a decoded value. `body_depth` is the indentation level a
/// multi-line string's body sits at (see `Walk::body_depth`); scalars ignore
/// it. This is the ONE place a token's text is rewritten rather than copied,
/// and every rewrite here is a spelling the specification makes normative.
fn format_value(
    out: &mut String,
    value: &Value,
    body_depth: usize,
    triple: &mut std::collections::VecDeque<bool>,
) {
    match value {
        Value::String(s) => format_string(out, s, body_depth, true, triple.pop_front()),
        // A resolved env value formats as the REFERENCE that produced it
        // (`$ENV.KEY`), never its content: the formatter writes source
        // text, and printing resolved material would embed a secret in a
        // file. Formatting only ever runs on parser output in-repo, so
        // this arm is defense-in-depth — and semantically it is the
        // correct round-trip (back to the authored spelling).
        Value::Resolved(r) => out.push_str(r.var()),
        Value::TemplateString(segments) => {
            // Braces stay raw: the reparse re-detects the template from
            // them, so the value's TYPE survives the round-trip.
            let s = template::segments_to_string(segments);
            format_string(out, &s, body_depth, false, triple.pop_front());
        }
        // Number's Display is exact and scale-preserving (RFC 0016):
        // the stored decimal renders with its own written scale, so
        // `2.50` survives formatting.
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::Money(m) => {
            out.push_str(&m.format_display());
        }
        // Canonical duration form is ATTACHED (`30s`) — a unit is a
        // suffix, unlike a currency code (RFC 0017 §6). Display renders
        // the stored value: the authored unit survives (never rescaled),
        // magnitude spelling normalizes (`030s` → `30s`).
        Value::Duration(d) => {
            out.push_str(&d.to_string());
        }
        Value::Bool(b) => {
            out.push_str(if *b { "true" } else { "false" });
        }
        Value::Secret(s) => {
            out.push_str(s);
        }
        Value::Role(r) => {
            out.push_str(r);
        }
        Value::Reference(r) => {
            out.push_str(r);
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_value(out, &item.value, body_depth, triple);
            }
            out.push(']');
        }
        Value::Fallback(primary, fallback) => {
            format_value(out, &primary.value, body_depth, triple);
            out.push_str(" | ");
            format_value(out, &fallback.value, body_depth, triple);
        }
    }
}

/// `escape_braces` distinguishes literal strings (true — a `{{` renders as
/// `\u{7B}{`, so raw-text template detection cannot re-fire on reparse and
/// the value stays a String) from template strings (false — braces are the
/// syntax the reparse must re-detect).
fn format_string(
    out: &mut String,
    s: &str,
    body_depth: usize,
    escape_braces: bool,
    authored_triple: Option<bool>,
) {
    // A value with a newline is triple-quoted; a value the author
    // triple-quoted STAYS triple-quoted. Both yield to one condition: the
    // value must have a body a block can carry. Two values have none: the empty string (`""""""` renders a
    // body of nothing, which is `""`) and a run of line breaks alone. A
    // block's closing delimiter must align with the indentation of its
    // CONTENT lines (NML0020), and a body whose every line is empty has no
    // content column — min-indent reads 0 while the delimiter sits at the
    // body's depth, so the block form of such a value is a document the
    // parser rejects. The literal form is the only one that can hold it.
    let no_body = s.is_empty() || s.bytes().all(|b| b == b'\n');
    let triple = !no_body && (s.contains('\n') || authored_triple == Some(true));
    if triple {
        out.push_str("\"\"\"\n");
        for line in s.split('\n') {
            // An empty content line is written empty: indentation on a line
            // with nothing after it is trailing whitespace, which
            // `spec/style.md` §4 removes — and which an editor's
            // trim-on-save would remove for it, leaving the file failing
            // `nml fmt --check` and the two tools fighting over every save.
            // Dedent is blind to it either way (a blank line steers neither
            // min-indent nor the value).
            for _ in 0..body_depth * usize::from(!line.is_empty()) {
                out.push_str(INDENT_UNIT);
            }
            // Edge spaces are CONTENT the transport layer would eat: a raw
            // leading space is indistinguishable from indentation on
            // reparse (min-indent would strip it) and a raw trailing space
            // is one editor trim-on-save away from a changed value — so a
            // line's first and last space render as `\s` (Java's escape,
            // adopted for exactly these emissions).
            let bytes = line.as_bytes();
            let protect_first = bytes.first() == Some(&b' ');
            let protect_last = line.len() > 1 && bytes.last() == Some(&b' ');
            let mut chars = line.char_indices().peekable();
            // A raw `"""` inside the body would close the string early on
            // reparse — escape the THIRD quote of any consecutive run (and
            // only that one), so quote-heavy content (JSON) stays readable
            // while a closing-delimiter run can never form.
            let mut quote_run = 0usize;
            while let Some((i, ch)) = chars.next() {
                if (i == 0 && protect_first) || (i == line.len() - 1 && protect_last) {
                    out.push_str("\\s");
                    quote_run = 0;
                    continue;
                }
                if ch == '"' {
                    quote_run += 1;
                    if quote_run == 3 {
                        out.push_str("\\\"");
                        quote_run = 0;
                        continue;
                    }
                } else {
                    quote_run = 0;
                }
                if ch == '\t' {
                    // A raw tab at the start of a rendered line would sit in
                    // the indentation run and fail NML0005 on reparse; escape
                    // uniformly (matching the single-line branch) so the
                    // formatter can never emit a document the parser rejects.
                    out.push_str("\\t");
                } else if escape_braces && ch == '{' && chars.peek().map(|(_, c)| *c) == Some('{') {
                    out.push_str("\\u{7B}");
                } else {
                    push_value_char(out, ch);
                }
            }
            out.push('\n');
        }
        for _ in 0..body_depth {
            out.push_str(INDENT_UNIT);
        }
        out.push_str("\"\"\"");
    } else if escape_braces {
        // A literal: the core's one speller (byte-identical to the loop
        // below plus the `{{` re-escape).
        out.push_str(&string_literal(s));
    } else {
        out.push('"');
        for ch in s.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\t' => out.push_str("\\t"),
                // A line break spells as its escape. Unreachable today —
                // a template's braces make `no_body` false, so a value
                // with a break takes the block branch — and here for the
                // same reason the branch above escapes a tab: this speller
                // is TOTAL, so no future value can make it emit a raw
                // break into a one-line literal (a document the parser
                // would reject).
                '\n' => out.push_str("\\n"),
                c => push_value_char(out, c),
            }
        }
        out.push('"');
    }
}

/// Render one string-value character, escaping what the parser's
/// source-character policy bans raw (`must_escape` is the policy's own
/// predicate, so formatted output can never carry a character the parser
/// rejects — a formatter emitting a raw ESC or bidi control would be
/// writing NML0017/NML0018 into the file it produces).
fn push_value_char(out: &mut String, ch: char) {
    match ch {
        '\\' => out.push_str("\\\\"),
        '\r' => out.push_str("\\r"),
        c if nml_core::source_policy::must_escape(c) => {
            // The policy's own spelling — the same string the NML0017/
            // NML0018 messages advise and their machine repairs insert.
            out.push_str(&nml_core::source_policy::unicode_escape(c));
        }
        c => out.push(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A non-item line inside a modifier block cannot be lowered losslessly:
    /// `format_source` refuses (the CLI leaves the file untouched) rather
    /// than dropping the line.
    #[test]
    fn format_source_refuses_non_items_in_a_modifier_block() {
        let err = format_source(
            "policy p:\n    |deny:\n        - \"a\"\n        .note = \"x\"\n        - \"b\"\n",
        )
        .expect_err("refused");
        assert!(
            err.to_string()
                .contains("expected a list item in a modifier block, found a shared property"),
            "{err}"
        );
    }

    /// A `key:` block dedented to an array body's item column cannot be
    /// lowered losslessly either: `format_source` refuses, so `nml fmt`
    /// leaves the file untouched instead of writing it back without the
    /// block (content loss, found in review).
    #[test]
    fn format_source_refuses_a_misplaced_entry_in_an_array_body() {
        let err = format_source(
            "[]validator validators:\n    - a:\n        files:\n            - \"x/**\"\n    \
             stray:\n        allowRefs:\n            - \"y\"\n    - b:\n        files:\n            \
             - \"z/**\"\n",
        )
        .expect_err("refused");
        assert!(
            err.to_string().contains(
                "expected a list item, a property, a modifier or a shared property in an array \
                 body, found a nested block"
            ),
            "{err}"
        );
    }

    use nml_core::parse;

    /// The formatter, source to source — the one door the product has. The
    /// AST-only entry point is gone with the AST re-emitter it belonged to.
    fn fmt(source: &str) -> String {
        format_source(source).expect("the fixture formats")
    }

    /// Every comment in a document, in source order, trimmed. The oracle
    /// for comment preservation: a comment is a token in the tree, so this
    /// reads them from the tree rather than from a side channel.
    fn comment_texts(source: &str) -> Vec<String> {
        nml_core::cst::parse(source)
            .syntax()
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind() == nml_core::cst::SyntaxKind::Comment)
            .map(|t| t.text().trim().to_string())
            .collect()
    }

    /// A resolved env value formats as the REFERENCE that produced it —
    /// the leak-proof round-trip: formatting a resolved tree writes the
    /// authored `$ENV.KEY` spelling, never the (possibly secret)
    /// resolved content.
    #[test]
    fn resolved_value_formats_as_its_reference() {
        let mut out = String::new();
        format_value(
            &mut out,
            &nml_core::types::Value::Resolved(nml_core::types::ResolvedText::new(
                "$ENV.API_KEY",
                "hunter2",
            )),
            0,
            &mut std::collections::VecDeque::new(),
        );
        assert_eq!(out, "$ENV.API_KEY");
        assert!(!out.contains("hunter2"));
    }

    fn roundtrip(source: &str) {
        let file = parse(source).unwrap();
        let formatted = fmt(source);
        let reparsed = parse(&formatted).unwrap_or_else(|e| {
            panic!(
                "failed to reparse formatted output:\n{}\nerror: {}",
                formatted,
                e.message()
            )
        });
        assert_eq!(
            file.declarations.len(),
            reparsed.declarations.len(),
            "declaration count mismatch after round-trip"
        );
    }

    fn idempotent(source: &str) {
        let first = fmt(source);
        let second = fmt(&first);
        assert_eq!(first, second, "formatting is not idempotent");
    }

    /// RFC 0017 §6: canonical duration form is ATTACHED and faithful —
    /// a spaced unit joins (`30 s` → `30s`), magnitude spelling
    /// normalizes (`030s` → `30s`), and the authored unit is NEVER
    /// rescaled (`72h` stays `72h`, not `259200s`) — unlike money, whose
    /// canonical form is spaced (`19.99 USD`): a currency code is a noun,
    /// a duration unit is a suffix.
    #[test]
    fn duration_formatting_attached_and_faithful() {
        for (source_value, expected) in [
            ("30s", "30s"),
            ("30 s", "30s"),
            ("030s", "30s"),
            ("72h", "72h"),
            ("30000ms", "30000ms"),
            ("250us", "250us"),
            ("50ns", "50ns"),
            // Separators are spelling: fmt canonicalizes them away, the
            // `007` → `7` doctrine (value and unit survive untouched).
            ("1_000ms", "1000ms"),
            ("1_000_000 ns", "1000000ns"),
            ("0s", "0s"),
            // Compound literals (RFC 0017 §10): spaced components join,
            // segments render coarse→fine, zero components drop as
            // spelling — but units NEVER carry (`60m` stays `60m`).
            ("1h30m", "1h30m"),
            ("1h 30m", "1h30m"),
            ("30m1h", "1h30m"),
            ("5m2s", "5m2s"),
            ("1h0s", "1h"),
            ("60m", "60m"),
        ] {
            let formatted = fmt(&format!("service App:\n    x = {source_value}\n"));
            assert!(
                formatted.contains(&format!("x = {expected}\n")),
                "{source_value}: got {formatted:?}"
            );
        }
        idempotent("service App:\n    x = 72h\n    y = 500 ms\n    z = 1h 30m\n");
        roundtrip("service App:\n    x = 30s\n");
        roundtrip("service App:\n    x = 1h30m45s\n");
    }

    /// RFC 0016 §1.10: the exact-decimal fixed-point table. Every output
    /// of the pre-decimal formatter re-formats to itself (no save-churn on
    /// previously formatted files) — with exactly one exception, `-0.0`,
    /// whose sign was presentation the model no longer fakes.
    #[test]
    fn number_formatting_fixed_point_table() {
        for (source_value, expected) in [
            // Old-formatter outputs: all fixed points.
            ("2.5", "2.5"),
            ("8080.0", "8080.0"),
            ("8080", "8080"),
            ("-5", "-5"),
            ("0.75", "0.75"),
            ("9007199254740993", "9007199254740993"),
            // Written scale now survives (the old formatter collapsed it —
            // a change only where information was previously destroyed).
            ("2.50", "2.50"),
            ("0.20", "0.20"),
            ("8080.000", "8080.000"),
            // Leading zeros still canonicalize.
            ("007", "7"),
            // Digit separators are spelling and normalize away (the same
            // doctrine); written scale still survives beside them.
            ("1_000_000", "1000000"),
            ("1_234.5_0", "1234.50"),
            // The one non-fixed-point: −0 is unrepresentable.
            ("-0.0", "0.0"),
        ] {
            let formatted = fmt(&format!("service App:\n    x = {source_value}\n"));
            assert!(
                formatted.contains(&format!("x = {expected}\n")),
                "{source_value}: got {formatted:?}"
            );
            // And the output is itself a fixed point.
            let again = fmt(&formatted);
            assert_eq!(formatted, again, "{source_value} must reach a fixed point");
        }
    }

    /// RFC 0018: facet lists canonicalize (single spaces, ", " joins)
    /// and round-trip idempotently — fmt renders types through the one
    /// shared `FieldTypeExpr` renderer.
    #[test]
    fn facet_lists_format_canonically() {
        let src = "model server:\n    port number( min=1 ,  max =65535 )\n    step set<number(multipleOf   =0.01)>\n";
        let once = fmt(src);
        assert!(
            once.contains("port number(min = 1, max = 65535)"),
            "{once:?}"
        );
        assert!(
            once.contains("step set<number(multipleOf = 0.01)>"),
            "{once:?}"
        );
        let twice = fmt(&once);
        assert_eq!(once, twice, "facet rendering must be a fixed point");

        // Duration facet values (RFC 0017) canonicalize and round-trip
        // through the same renderer — compound values included.
        let src = "model job:\n    t duration( min=1s ,  multipleOf =250ms )\n    g duration(max = 1h 30m)\n";
        let once = fmt(src);
        assert!(
            once.contains("t duration(min = 1s, multipleOf = 250ms)"),
            "{once:?}"
        );
        assert!(once.contains("g duration(max = 1h30m)"), "{once:?}");
        let twice = fmt(&once);
        assert_eq!(
            once, twice,
            "duration facet rendering must be a fixed point"
        );
    }

    /// Every extreme literal the language accepts survives formatting:
    /// the boundaries fixture (34-digit integers, clamped 10^34, scale
    /// preservation) formats idempotently and value-preservingly, and
    /// synthesized subnormal-band / 6176-scale-zero literals — too long
    /// to live readably in a fixture file — are appended in-test. fmt
    /// is Display-driven, so this pins the scale-preservation contract
    /// at the domain's edges.
    #[test]
    fn number_boundaries_fixture_formats_losslessly() {
        let fixture = include_str!("../../../tests/fixtures/valid/number-boundaries.nml");
        // Extremes the fixture omits (a 6178-char line is not a readable
        // fixture): the subnormal band and the deepest zero scale.
        let src = format!(
            "{fixture}\nextremes Edge:\n    subnormal = 0.{}5\n    deepZero = 0.{}\n",
            "0".repeat(6150),
            "0".repeat(6176),
        );
        let src = src.as_str();
        let once = fmt(src);
        let twice = fmt(&once);
        assert_eq!(once, twice, "fmt must be idempotent on boundary literals");
        // A spelling PAST the 34-digit budget canonicalizes to the budget:
        // the trailing zeros drop losslessly (`spec/types.md`), so the
        // written scale the formatter preserves is the one the value has.
        // The fixture itself stays canonical; this is where the
        // over-long spelling lives.
        let over = fmt("service N:\n    exactHalf = 0.50000000000000000000000000000000000\n");
        assert_eq!(
            over, "service N:\n    exactHalf = 0.5000000000000000000000000000000000\n",
            "35 written digits render at the 34-digit budget"
        );
        // Value preservation: reparse and compare every number
        // semantically (numeric Eq — cosmetic moves allowed, value
        // drift not).
        let a = nml_core::cst::parse_to_ast(src).unwrap();
        let b = nml_core::cst::parse_to_ast(&once).unwrap();
        let nums = |f: &nml_core::ast::File| {
            let mut out = Vec::new();
            fn walk_body(b: &nml_core::ast::Body, out: &mut Vec<nml_core::types::Number>) {
                for e in &b.entries {
                    match &e.kind {
                        nml_core::ast::BodyEntryKind::Property(p) => {
                            walk_value(&p.value.value, out)
                        }
                        nml_core::ast::BodyEntryKind::NestedBlock(n) => walk_body(&n.body, out),
                        nml_core::ast::BodyEntryKind::ListItem(i) => {
                            if let nml_core::ast::ListItemKind::Named { body, .. } = &i.kind {
                                walk_body(body, out);
                            }
                        }
                        _ => {}
                    }
                }
            }
            fn walk_value(v: &nml_core::types::Value, out: &mut Vec<nml_core::types::Number>) {
                match v {
                    nml_core::types::Value::Number(n) => out.push(*n),
                    nml_core::types::Value::Array(items) => {
                        for it in items {
                            walk_value(&it.value, out);
                        }
                    }
                    _ => {}
                }
            }
            for d in &f.declarations {
                if let nml_core::ast::DeclarationKind::Block(blk) = &d.kind {
                    walk_body(&blk.body, &mut out);
                }
            }
            out
        };
        let (na, nb) = (nums(&a), nums(&b));
        assert_eq!(na.len(), nb.len(), "number count must survive fmt");
        assert!(!na.is_empty(), "fixture must actually contain numbers");
        for (x, y) in na.iter().zip(&nb) {
            assert_eq!(x, y, "value drift through fmt");
            assert_eq!(x.scale(), y.scale(), "scale drift through fmt");
        }
    }

    /// The formatter can never emit a document the parser rejects: string
    /// values holding policy-banned characters (reachable only via `\u{…}`
    /// escapes) render back AS escapes. `roundtrip` is the enforcement —
    /// a raw ESC/bidi/CR in the output would fail the reparse with
    /// NML0016–NML0018.
    #[test]
    fn banned_value_characters_render_as_escapes() {
        // The roundtrip IS the enforcement: `roundtrip` reparses with
        // the source policy live, so a formatter that laundered an
        // escape into a raw banned byte (the pre-closed-class defect for
        // NEL/CSI/LS/PS) fails here, not in a user's tree.
        let source = "service App:\n    ansi = \"\\u{1B}[0m\"\n    bidi = \"\\u{202E}\"\n    cr = \"a\\rb\"\n    nel = \"\\u{85}\"\n    csi = \"\\u{9B}\"\n    ls = \"\\u{2028}\"\n    ps = \"\\u{2029}\"\n    tag = \"\\u{E0067}\"\n";
        roundtrip(source);
        idempotent(source);
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("\\u{1B}[0m"), "{formatted}");
        assert!(formatted.contains("\\u{202E}"), "{formatted}");
        assert!(formatted.contains("a\\rb"), "{formatted}");
        assert!(formatted.contains("\\u{85}"), "{formatted}");
        assert!(formatted.contains("\\u{9B}"), "{formatted}");
        assert!(formatted.contains("\\u{2028}"), "{formatted}");
        assert!(formatted.contains("\\u{2029}"), "{formatted}");
        assert!(formatted.contains("\\u{E0067}"), "{formatted}");
        for raw in ['\u{85}', '\u{9B}', '\u{2028}', '\u{2029}', '\u{E0067}'] {
            assert!(
                !formatted.contains(raw),
                "a raw banned byte in formatter output: {formatted:?}"
            );
        }
    }

    /// The one speller's arm surfaces — oneof arm values and literal arm
    /// selectors/targets — must re-escape the banned set exactly like
    /// every other string surface: a second escape table laundered
    /// every banned character it did not know about (ESC, bidi, CR,
    /// then the closed-class additions) into raw bytes the parser
    /// rejects, so `nml fmt` corrupted a working file.
    #[test]
    fn quoted_arm_surfaces_render_banned_characters_as_escapes() {
        let source = concat!(
            "model emailLog:\n    kind string?\n\n",
            "oneof email by provider:\n    \"a\\u{85}b\" -> emailLog\n    \"c\\u{202E}d\" -> emailLog\n"
        );
        roundtrip(source);
        idempotent(source);
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("a\\u{85}b"), "{formatted}");
        assert!(formatted.contains("c\\u{202E}d"), "{formatted}");
        for raw in ['\u{85}', '\u{202E}'] {
            assert!(
                !formatted.contains(raw),
                "a raw banned byte in formatter output: {formatted:?}"
            );
        }

        let arms = concat!(
            "model landing:\n    label number\n\n",
            "model svc:\n    routing (string -> landing)?\n\n",
            "svc Api:\n    routing:\n        \"p\\u{2028}q\" -> \"up\\rsell\"\n"
        );
        roundtrip(arms);
        idempotent(arms);
        let formatted = format_source(arms).unwrap();
        assert!(formatted.contains("p\\u{2028}q"), "{formatted}");
        assert!(formatted.contains("up\\rsell"), "{formatted}");
    }

    /// Edge spaces in multiline values (writable via `\u{20}`, which
    /// survives dedent under the text-block order) round-trip: the
    /// formatter re-escapes a line's first/last space so reparse dedent
    /// and editor whitespace-trimming can't eat them. Idempotence is the
    /// enforcement — a lost space would change the second format.
    #[test]
    fn edge_spaces_in_multiline_values_are_protected() {
        let source = "service App:\n    x = \"\"\"\n        \\u{20}lead\n        tail\\u{20}\n        \"\"\"\n";
        roundtrip(source);
        idempotent(source);
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("\\slead"), "{formatted}");
        assert!(formatted.contains("tail\\s"), "{formatted}");
    }

    /// A value line beginning with tab content (writable via `\t`) must
    /// re-emit the tab AS an escape: raw, it would land inside the rendered
    /// line's indentation run and fail NML0005 on reparse — the formatter
    /// may never produce a document the parser rejects.
    #[test]
    fn leading_tab_content_renders_escaped() {
        let source = "service App:\n    x = \"\"\"\n        \\ta\n        b\n        \"\"\"\n";
        roundtrip(source);
        idempotent(source);
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("\\ta"), "{formatted}");
    }

    /// A multiline value containing `"""` (writable via `\"\"\"`) must not
    /// re-emit a raw closing run — the third quote of any consecutive run
    /// renders escaped, so the reparse sees the same value instead of a
    /// truncated string (this was a live data-destroying round-trip bug).
    /// Quote PAIRS stay raw so JSON-ish content remains readable.
    #[test]
    fn triple_quotes_in_multiline_values_roundtrip() {
        let source = "service App:\n    x = \"\"\"\n        a\\\"\\\"\\\"b {\"k\": \"\"}\n        c\n        \"\"\"\n";
        roundtrip(source);
        idempotent(source);
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("a\"\"\\\"b"), "{formatted}");
        assert!(formatted.contains("{\"k\": \"\"}"), "{formatted}");
    }

    /// Value TYPES survive the round-trip across the template boundary: a
    /// literal `{{` (writable only via `\u{7B}`) re-escapes its first brace
    /// so raw-text template detection cannot re-fire, while a real template
    /// keeps raw braces and re-detects.
    #[test]
    fn literal_braces_stay_string_templates_stay_templates() {
        let source = "service App:\n    lit = \"\\u{7B}\\u{7B}x}}\"\n    tpl = \"{{args.x}}\"\n";
        roundtrip(source);
        idempotent(source);
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("\\u{7B}{x}}"), "{formatted}");
        assert!(formatted.contains("\"{{args.x}}\""), "{formatted}");
    }

    /// RFC 0032: directives survive formatting in canonical one-space form,
    /// args included, and formatting is idempotent over them. They survive
    /// because the printer copies tokens: there is no render arm a new
    /// construct could be missing from, which is what used to make a
    /// silently deleted field possible.
    #[test]
    fn test_directives_survive_formatting_canonically() {
        let source = "model server:\n    ceiling capabilitySet   #live\n    hosts set<string> #live   #key(\"host\")\n";
        let formatted = format_source(source).unwrap();
        assert!(
            formatted.contains("ceiling capabilitySet #live\n"),
            "{formatted}"
        );
        assert!(
            formatted.contains("hosts set<string> #live #key(\"host\")\n"),
            "{formatted}"
        );
        idempotent(source);
    }

    /// RFC 0032: `set<T>` spacing is the printer's (`set <T>` loses the
    /// space — the angle brackets bind tight), while the author's GROUPING
    /// stands: `set<(a | b)>` keeps its parentheses. Redundant parentheses
    /// are a simplification, not a formatting — rustfmt, gofmt and prettier
    /// all leave them alone, and restructuring a type expression is the one
    /// way a formatter could change what a schema means.
    #[test]
    fn set_type_spacing_is_canonical_and_grouping_is_the_authors() {
        let source = "model m:\n    xs set <string>\n    ys set<(string | number)>\n";
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("xs set<string>\n"), "{formatted}");
        assert!(
            formatted.contains("ys set<(string | number)>\n"),
            "{formatted}"
        );
        idempotent(source);
    }

    /// RFC 0032: MODIFIER-declaration directives survive formatting too (the
    /// same deletion class the field fix closed) and stay idempotent.
    #[test]
    fn test_modifier_directives_survive_formatting() {
        let source = "model sandboxCeiling:\n    |block []string?   #live\n";
        let formatted = format_source(source).unwrap();
        assert!(
            formatted.contains("|block []string? #live\n"),
            "{formatted}"
        );
        idempotent(source);
    }

    #[test]
    fn test_format_scalar_item_with_body_roundtrips() {
        // `- "/admin":` + body survives formatting (scalar-key-with-body).
        let source = "[]resource resources:\n    - \"/admin\":\n        method = \"POST\"\n";
        let formatted = fmt(source);
        assert!(formatted.contains("- \"/admin\":"), "{formatted}");
        assert!(formatted.contains("method = \"POST\""), "{formatted}");
        roundtrip(source);
        idempotent(source);
    }

    #[test]
    fn test_format_shorthand_and_optional_suffixes_roundtrip() {
        // `+` (positional shorthand), `?` (optional), and canonical `?+` survive formatting.
        let source = "model resource:\n    name string?\n    path path+\n    slug string?+\n";
        let formatted = fmt(source);
        assert!(formatted.contains("path path+"), "{formatted}");
        assert!(formatted.contains("slug string?+"), "{formatted}");
        assert!(formatted.contains("name string?"), "{formatted}");
        roundtrip(source);
        idempotent(source);
    }

    /// `spec/style.md` §4: the arrow column belongs to the AUTHOR. The
    /// formatter never invents alignment and never destroys it — an aligned
    /// run stays aligned byte for byte, an unaligned one keeps its single
    /// space, and neither is rewritten when a sibling arm changes width.
    /// (The old formatter padded every arm to the widest selector in its
    /// run, so adding one long arm rewrote every line around it.)
    #[test]
    fn arrow_alignment_is_the_authors_both_ways() {
        let aligned = "oneof email by provider:\n    \"log\"      -> emailLog\n    \"postmark\" -> emailPostmark\n";
        assert_eq!(fmt(aligned), aligned, "an aligned run is left alone");
        roundtrip(aligned);
        idempotent(aligned);

        let plain = "oneof email by provider:\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n";
        assert_eq!(fmt(plain), plain, "an unaligned run is not padded");
        roundtrip(plain);
        idempotent(plain);
    }

    #[test]
    fn test_format_arm_set_field_types_roundtrip() {
        // RFC 0007 §5: the arm-set TYPE renders canonically as `(K -> V)`,
        // composes with unions on either side, keeps the field-suffix `?`
        // outside the parens, round-trips, and is idempotent.
        let source =
            "model mount:\n    denial (string | (role -> denial))?\n    route (role -> (a | b))\n";
        let formatted = fmt(source);
        assert!(
            formatted.contains("denial (string | (role -> denial))?"),
            "union-of-scalar-and-arm-set renders canonically:\n{formatted}"
        );
        assert!(
            formatted.contains("route (role -> (a | b))"),
            "arm set to a union target renders canonically:\n{formatted}"
        );
        roundtrip(source);
        idempotent(source);
    }

    #[test]
    fn test_format_denial_arms_roundtrip() {
        // Arms inside a plain block (RFC 0007) print as written: the arrow
        // column is the author's (`spec/style.md` §4).
        let source =
            "service App:\n    denial:\n        @plan/Pro -> ProUpsell\n        else -> Generic\n";
        assert_eq!(fmt(source), source, "arms print as written");
        let aligned = "service App:\n    denial:\n        @plan/Pro -> ProUpsell\n        else      -> Generic\n";
        assert_eq!(fmt(aligned), aligned, "and an aligned run stays aligned");
        roundtrip(source);
        idempotent(source);
        idempotent(aligned);
    }

    #[test]
    fn test_format_arm_literal_targets_roundtrip() {
        // RFC 0007 §6: a string-literal arm target (path/url for flat routers)
        // renders quoted, aligns like any arm, round-trips, and is idempotent.
        let source = "service App:\n    dispatch:\n        @role/admin -> \"admin.workflow.nml\"\n        else -> \"default.workflow.nml\"\n";
        let formatted = fmt(source);
        assert!(
            formatted.contains("        @role/admin -> \"admin.workflow.nml\"\n"),
            "literal target renders quoted:\n{formatted}"
        );
        assert!(
            formatted.contains("        else -> \"default.workflow.nml\"\n"),
            "the arrow column is the author's (`spec/style.md` §4):\n{formatted}"
        );
        roundtrip(source);
        idempotent(source);
    }

    #[test]
    fn test_format_arm_inline_targets_roundtrip() {
        let source = "service App:\n    routing:\n        @role/admin -> adminLanding:\n            label = 4\n        else -> defaultLanding\n";
        let formatted = fmt(source);
        assert!(
            formatted.contains("@role/admin -> adminLanding:\n"),
            "inline arm header:\n{formatted}"
        );
        assert!(
            formatted.contains("            label = 4\n"),
            "inline arm body indented:\n{formatted}"
        );
        roundtrip(source);
        idempotent(source);
    }

    #[test]
    fn test_format_arm_string_selector_roundtrip() {
        let source = "service App:\n    routing:\n        \"plan\" -> \"upsell\"\n        @role/admin -> admin\n";
        let formatted = fmt(source);
        assert!(
            formatted.contains("\"plan\"") && formatted.contains("-> \"upsell\""),
            "string selector arm:\n{formatted}"
        );
        roundtrip(source);
        idempotent(source);
    }

    #[test]
    fn test_format_oneof_default_discriminator_roundtrips() {
        let source = "oneof email by provider = \"log\":\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n";
        let formatted = fmt(source);
        assert!(
            formatted.contains("oneof email by provider = \"log\":"),
            "default discriminator must survive formatting:\n{formatted}"
        );
        roundtrip(source);
        idempotent(source);
    }

    #[test]
    fn test_format_oneof_enum_typed_discriminator_roundtrips() {
        let source = "oneof email by provider as providerKind = \"log\":\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n";
        let formatted = fmt(source);
        assert!(
            formatted.contains("oneof email by provider as providerKind = \"log\":"),
            "enum type + default must survive formatting:\n{formatted}"
        );
        roundtrip(source);
        idempotent(source);
    }

    /// Comment-preserving formatting must reparse cleanly, keep every
    /// comment, and be idempotent.
    fn roundtrip_comments(source: &str) -> String {
        let formatted = format_source(source)
            .unwrap_or_else(|e| panic!("failed to format:\n{}\nerror: {}", source, e.message()));
        let original = comment_texts(source);
        let kept = comment_texts(&formatted);
        assert_eq!(original, kept, "comments lost or reordered:\n{formatted}");

        let again = format_source(&formatted).unwrap();
        assert_eq!(formatted, again, "comment formatting is not idempotent");
        formatted
    }

    // -------------------------------------------------------------------
    // The property battery (`spec/style.md` §8, guarantees 4 to 8)
    //
    // The `format` fuzz target owns guarantees 1 to 3 (the output parses,
    // formatting is idempotent, the lowered tree is preserved) on arbitrary
    // input. It cannot see the rest: the AST it compares has already thrown
    // away the tokens' spelling, the comments, and the line endings — which
    // is exactly the information a formatter is trusted with. These are
    // those guarantees, over a battery that covers every grammar shape and
    // every layout `spec/style.md` names.
    // -------------------------------------------------------------------

    /// One document per grammar shape and per layout the style specification
    /// names, plus four real fixtures from the repository's corpus.
    fn battery() -> Vec<(&'static str, String)> {
        let mut docs: Vec<(&'static str, String)> = [
            ("blocks", "// header\nservice App: // trailing\n    // own-line\n    port = 8080\n\n    nested:\n        host = \"h\"\n    // closes the block\n\nservice Other:\n    x = 1\n"),
            ("model", "model service:\n    host string\n    port number(min = 1, max = 65535)\n    regions set<string>?\n    contact (string | []string)?\n    landing (role -> string)?\n    tags []string #live #key(\"host\")\n    slug string?+\n    retry duration(max = 1h30m) = 30s\n"),
            ("oneof aligned", "oneof notifier by kind as string = \"log\":\n    \"log\"     -> notifierLog\n    \"email\"   -> notifierEmail\n    \"webhook\" -> notifierWebhook\n"),
            ("oneof plain", "oneof notifier by kind:\n    \"log\" -> notifierLog\n    \"email\" -> notifierEmail\n"),
            ("arms", "service App:\n    landing:\n        @role/admin -> \"ops\"\n        else -> \"status\"\n    denial:\n        @plan/Pro   -> ProUpsell\n        else        -> Generic\n"),
            ("array decl", "[]validator validators:\n    .shared:\n        z = 1\n    |deny:\n        - \"a\"\n        - \"b\"\n\n    - named:\n        files:\n            - \"x/**\"\n    - \"shorthand\"\n    - Reference\n    - @role/admin\n"),
            ("values", "service App:\n    n = 10000\n    neg = -5\n    frac = 2.50\n    money = 19.99 USD\n    dur = 1h30m\n    b = true\n    s = \"text\"\n    tpl = \"hi {{args.name}}\"\n    arr = [1, 2, 3]\n    fb = $ENV.PORT | 8080\n    gate = @role/admin & @role/ops\n    ref = SomeConst\n"),
            ("const inline", "const Port = 8000\nconst Prompt = \"short\"\n"),
            ("const broken", "const Prompt =\n    \"\"\"\n    You are an intent classifier.\n    Analyze the message.\n    \"\"\"\n"),
            ("property broken", "service App:\n    system =\n        \"\"\"\n        line one\n        line two\n        \"\"\"\n"),
            ("template", "template Prompt:\n    \"\"\"\n    You are an intent classifier.\n    \"\"\"\n"),
            ("inline multiline", "service App:\n    system = \"\"\"\n        line one\n        line two\n        \"\"\"\n"),
            ("layers", "flow Base:\n    entrypoint = \"search\"\n\nflow Tenant is Base uses Base:\n    entrypoint = \"custom\"\n"),
            ("blank runs", "const A = 1\nconst B = 2\n\n\nservice App:\n    x = 1\n\n    y = 2\n"),
            ("comment only", "// a note\n// another\n"),
            ("escapes", "service App:\n    a = \"quote \\\" and \\\\ and tab \\t\"\n    b = \"caf\\u{E9}\"\n    c = \"\\u{7B}\\u{7B}literal}}\"\n"),
            ("edge spaces", "service App:\n    a = \"\"\"\n        \\sleading and trailing\\s\n        \"\"\"\n"),
            ("multiline blank lines", "service App:\n    prompt = \"\"\"\n        first\n\n        second\n        \"\"\"\n"),
            ("line-break values", "service App:\n    only = \"\\n\"\n    lead = \"\\nx\"\n    tail = \"x\\n\"\n    both = \"a\\n\\nb\"\n"),
            ("block body escapes", "service App:\n    a = \"\"\"\n        c:\\\\path\\\\to\n        one\\rtwo\n        esc\\u{1B}[0m\n        bidi\\u{202E}flip\n        \"\"\"\n    t = \"\"\"\n        hi {{args.n}} \\\\ x\n        \"\"\"\n"),
            ("no final terminator", "const A = 1\nconst B = 2"),
        ]
        .iter()
        .map(|(n, s)| (*n, (*s).to_string()))
        .collect();
        for (name, text) in [
            (
                "fixture: full-service",
                include_str!("../../../tests/fixtures/valid/full-service.nml"),
            ),
            (
                "fixture: web-server",
                include_str!("../../../tests/fixtures/valid/web-server.nml"),
            ),
            (
                "fixture: number-boundaries",
                include_str!("../../../tests/fixtures/valid/number-boundaries.nml"),
            ),
            (
                "fixture: spec service",
                include_str!("../../../spec/examples/service.nml"),
            ),
            (
                "fixture: spec service model",
                include_str!("../../../spec/examples/service.model.nml"),
            ),
            (
                "fixture: tutorial 09",
                include_str!("../../../docs/tutorial/examples/09/app.nml"),
            ),
        ] {
            docs.push((name, text.to_string()));
        }
        // A battery with no documents proves nothing, and every property
        // below is a `for … in battery()`: a corpus that shrank — a
        // deleted entry, a fixture that moved out from under an
        // `include_str!` — would pass all seven in silence. The floor is
        // today's population, so it ratchets one way.
        assert!(
            docs.len() >= 27,
            "the property battery shrank to {} documents",
            docs.len()
        );
        docs
    }

    /// The significant tokens of a document, as `(kind, text)`.
    fn significant_tokens(source: &str) -> Vec<(nml_core::cst::SyntaxKind, String)> {
        nml_core::cst::parse(source)
            .syntax()
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind().is_significant())
            .map(|t| (t.kind(), t.text().to_string()))
            .collect()
    }

    /// `spec/style.md` §8.4 and §8.6: every significant token survives, in
    /// order, with its KIND unchanged; its text is unchanged except for the
    /// §5 normalizations, and where a normalization applies the token's
    /// VALUE is unchanged. Nothing is added, dropped, reordered or
    /// retyped — the guarantee the AST-shape comparison cannot make, because
    /// the AST has already discarded the spelling.
    #[test]
    fn every_significant_token_survives_with_its_value() {
        use nml_core::cst::SyntaxKind as K;
        for (name, source) in battery() {
            let before = significant_tokens(&source);
            let after = significant_tokens(&fmt(&source));
            assert_eq!(
                before.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
                after.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
                "{name}: the token KIND sequence moved"
            );
            for ((kind, was), (_, now)) in before.iter().zip(&after) {
                if was == now {
                    continue;
                }
                match kind {
                    // §5: separators and magnitude spelling normalize; the
                    // VALUE is what must not move.
                    K::Number => {
                        let a = nml_core::decimal::Number::parse_literal(was);
                        let b = nml_core::decimal::Number::parse_literal(now);
                        assert_eq!(a, b, "{name}: number {was:?} became {now:?}");
                    }
                    // §5: a multi-line body re-indents and edge spaces are
                    // protected; the decoded value is what must not move.
                    K::String => {
                        let a = decoded(was);
                        let b = decoded(now);
                        assert_eq!(a, b, "{name}: string {was:?} became {now:?}");
                    }
                    _ => panic!("{name}: {kind:?} token {was:?} was rewritten to {now:?}"),
                }
            }
        }
    }

    /// Decode a string literal's text the way the parser does, by handing
    /// the token back through a document. Used only as the property
    /// battery's oracle.
    fn decoded(literal: &str) -> String {
        let src = format!("service X:\n    v = {literal}\n");
        let tok = nml_core::cst::parse(&src)
            .syntax()
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind() == nml_core::cst::SyntaxKind::String)
            .expect("the literal is a string token");
        nml_core::cst::decode_string(&tok).expect("the literal decodes")
    }

    /// The walk's depth arithmetic rests on two facts of the tree, pinned
    /// here rather than assumed:
    ///
    /// 1. A node's indentation opens once and closes with the node, so its
    ///    `Dedent` is the last thing in it.
    /// 2. Only trivia precedes a layout marker on its line, so no marker
    ///    ever lands between two significant tokens of the same line.
    ///
    /// Together they say a LINE's depth is its first token's, and that no
    /// marker can leak past the node that owns it — which is why the walk
    /// carries depth down rather than counting markers along a flat stream.
    /// A grammar that ever re-indented mid-line or mid-node would need the
    /// counting version, and this is the test that would say so.
    #[test]
    fn a_nodes_indentation_closes_with_the_node() {
        fn check(node: &nml_core::cst::SyntaxNode, name: &str) {
            let mut opens = 0usize;
            let mut closed_at: Option<usize> = None;
            for (i, child) in node.children_with_tokens().enumerate() {
                match child.kind() {
                    SyntaxKind::Indent => {
                        assert!(closed_at.is_none(), "{name}: re-indents after a dedent");
                        opens += 1;
                        assert_eq!(opens, 1, "{name}: two indents in one node");
                    }
                    SyntaxKind::Dedent => closed_at = Some(i),
                    k if k.is_trivia() => {}
                    _ => assert!(
                        closed_at.is_none(),
                        "{name}: content after the node's dedent"
                    ),
                }
            }
            assert_eq!(
                opens,
                usize::from(closed_at.is_some()),
                "{name}: unbalanced layout markers"
            );
            for child in node.children() {
                check(&child, name);
            }
        }
        for (name, source) in battery() {
            let tree = nml_core::cst::parse(&source).syntax();
            check(&tree, name);
            // Fact 2: walking back from a marker over its line reaches the
            // line break (or the start of the file) without passing code.
            for tok in tree
                .descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind().is_layout())
            {
                let mut prev = tok.prev_token();
                while let Some(t) = prev {
                    match t.kind() {
                        SyntaxKind::Newline => break,
                        k if k.is_trivia() || k.is_layout() => prev = t.prev_token(),
                        k => panic!("{name}: a layout marker after {k:?} on its own line"),
                    }
                }
            }
        }
    }

    /// `spec/style.md` §8.5: every comment survives, in order, with its
    /// text unchanged but for trailing whitespace (§3).
    #[test]
    fn every_comment_survives_in_order() {
        for (name, source) in battery() {
            let before: Vec<String> = comment_texts(&source)
                .iter()
                .map(|c| c.trim_end().to_string())
                .collect();
            let after: Vec<String> = comment_texts(&fmt(&source))
                .iter()
                .map(|c| c.trim_end().to_string())
                .collect();
            assert_eq!(before, after, "{name}: comments moved or changed");
        }
    }

    /// `spec/style.md` §8.7: nothing is duplicated. The output's content is
    /// the input's tokens and comments, so its size is bounded by the
    /// input's plus the indentation written — which cannot exceed one
    /// nesting unit per level per line.
    #[test]
    fn output_is_bounded_by_its_input_plus_indentation() {
        for (name, source) in battery() {
            let formatted = fmt(&source);
            let lines = formatted.lines().count();
            let depth = formatted
                .lines()
                .map(|l| l.len() - l.trim_start().len())
                .max()
                .unwrap_or(0);
            // Escapes can widen a character (a protected edge space, a
            // policy escape), so the slack is per LINE, not per byte.
            let bound = source.len() + lines * (depth + 8) + 64;
            assert!(
                formatted.len() <= bound,
                "{name}: {} bytes in, {} out, bound {bound}",
                source.len(),
                formatted.len()
            );
        }
    }

    /// `spec/style.md` §6 and §8.8: a CRLF file stays a CRLF file, with no
    /// lone terminator anywhere, and its content is its LF twin's.
    #[test]
    fn crlf_is_preserved_and_matches_its_lf_twin() {
        for (name, source) in battery() {
            if source.is_empty() {
                continue;
            }
            let lf = fmt(&source);
            let crlf = fmt(&source.replace('\n', "\r\n"));
            assert_eq!(
                crlf.matches("\r\n").count(),
                crlf.matches('\n').count(),
                "{name}: a lone LF in a CRLF file"
            );
            assert!(
                !crlf.contains("\r\r") && !crlf.replace("\r\n", "").contains('\r'),
                "{name}: a stray CR"
            );
            assert_eq!(
                crlf.replace("\r\n", "\n"),
                lf,
                "{name}: CRLF and LF runs disagree on content"
            );
            assert_eq!(fmt(&crlf), crlf, "{name}: the CRLF run is not idempotent");
        }
    }

    /// `spec/style.md` §7: an invalid document is REFUSED — the error is
    /// returned, never a panic and never a rewritten document. The caller
    /// leaves the file untouched.
    #[test]
    fn an_invalid_document_is_refused_not_rewritten() {
        for (name, source) in [
            ("stray token", "service A:\n    x = = 1\n"),
            ("repeated entry", "service A:\n    v = 1\n    v = 2\n"),
            (
                "repeated declaration",
                "service A:\n    v = 1\n\nservice A:\n    v = 2\n",
            ),
            ("bad number", "service A:\n    v = 1.\n"),
            ("unknown currency", "service A:\n    v = 1 XYZ\n"),
            ("tab indentation", "service A:\n\tv = 1\n"),
            ("bare dedent", "service A:\n        v = 1\n    w = 2\n"),
            ("fat arrow", "oneof k by kind:\n    \"a\" => A\n"),
            (
                "content on the opening line",
                "service A:\n    v = \"\"\"text\n        \"\"\"\n",
            ),
            ("unterminated string", "service A:\n    v = \"open\n"),
            ("dangling fallback", "service A:\n    v = 1 |\n"),
            ("dangling arrow", "oneof k by kind:\n    \"a\" ->\n"),
        ] {
            let outcome = format_source(source);
            assert!(
                outcome.is_err(),
                "{name}: should be refused, got {outcome:?}"
            );
        }
    }

    /// `spec/style.md` §2: the blank-line policy, every clause of it.
    #[test]
    fn blank_lines_follow_the_policy() {
        for (name, source, want) in [
            (
                "a run inside a block caps at one",
                "service A:\n    x = 1\n\n\n\n    y = 2\n",
                "service A:\n    x = 1\n\n    y = 2\n",
            ),
            (
                "a run between declarations caps at two",
                "const A = 1\n\n\n\n\nconst B = 2\n",
                "const A = 1\n\n\nconst B = 2\n",
            ),
            (
                "a blank after a header is removed",
                "service A:\n\n    x = 1\n",
                "service A:\n    x = 1\n",
            ),
            (
                "a blank leaving a block is kept",
                "service A:\n    b:\n        x = 1\n\n    y = 2\n",
                "service A:\n    b:\n        x = 1\n\n    y = 2\n",
            ),
            (
                "the file opens and closes without blanks",
                "\n\nservice A:\n    x = 1\n\n\n",
                "service A:\n    x = 1\n",
            ),
            (
                "no blank is invented between declarations",
                "const A = 1\nconst B = 2\n",
                "const A = 1\nconst B = 2\n",
            ),
            (
                "no blank is invented before a shared property's items",
                "workflow W:\n    .shared = 1\n    - A:\n        n = \"a\"\n",
                "workflow W:\n    .shared = 1\n    - A:\n        n = \"a\"\n",
            ),
        ] {
            assert_eq!(fmt(source), want, "{name}");
            idempotent(source);
        }
    }

    /// `spec/style.md` §5: the author's line break between `=` (or `:`) and
    /// the value stands, both ways, and the body of a multi-line string
    /// lands under its opening delimiter either way. The old formatter
    /// joined every one of these, which is why 18 of 18 tutorial chapters
    /// failed `nml fmt --check`.
    #[test]
    fn the_authors_value_line_break_stands_both_ways() {
        for (name, source) in [
            (
                "const, broken",
                "const P =\n    \"\"\"\n    one\n    two\n    \"\"\"\n",
            ),
            (
                "const, inline",
                "const P = \"\"\"\n    one\n    two\n    \"\"\"\n",
            ),
            ("const, scalar on the next line", "const P =\n    8000\n"),
            (
                "property, broken",
                "service A:\n    p =\n        \"\"\"\n        one\n        \"\"\"\n",
            ),
            (
                "property, inline",
                "service A:\n    p = \"\"\"\n        one\n        \"\"\"\n",
            ),
            (
                "template, own line",
                "template P:\n    \"\"\"\n    one\n    \"\"\"\n",
            ),
        ] {
            assert_eq!(fmt(source), source, "{name}: the author's shape moved");
            idempotent(source);
            // The value is the same whichever shape it is written in.
            assert_eq!(
                nml_core::parse(source).unwrap().declarations.len(),
                1,
                "{name}"
            );
        }
    }

    /// `spec/style.md` §4: re-indentation moves a multi-line string's body
    /// with its line, so the VALUE is untouched — the reason the body's
    /// indentation is computed rather than copied.
    #[test]
    fn reindentation_does_not_change_a_multiline_value() {
        // The decoded content of every string in the document, in order —
        // the value, with no span in it.
        let values = |src: &str| -> Vec<String> {
            nml_core::cst::parse(src)
                .syntax()
                .descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == nml_core::cst::SyntaxKind::String)
                .map(|t| nml_core::cst::decode_string(&t).expect("decodes"))
                .collect()
        };
        // A two-space file reformats to four, three levels deep: the body
        // of the string moves with its line, and everything it carries —
        // relative indentation included, protected where dedent or a
        // trim-on-save could eat it — reaches the same value.
        for source in [
            "service A:\n  b:\n    p = \"\"\"\n      one\n        two\n      \"\"\"\n",
            "service A:\n  b:\n    p =\n      \"\"\"\n      one\n        two\n      \"\"\"\n",
            "service A:\n        b:\n                p = \"\"\"\n                        one\n                        \"\"\"\n",
        ] {
            let formatted = fmt(source);
            assert_eq!(
                values(source),
                values(&formatted),
                "the value moved:\n{formatted}"
            );
            idempotent(source);
        }
    }

    /// `spec/style.md` §5: a facet's bound is a bare `Number` token with no
    /// value node around it, and the normative number spelling reaches it
    /// too — separators are spelling, never value, and `007` is `7`.
    #[test]
    fn a_facets_number_normalizes_like_any_other() {
        assert_eq!(
            fmt("model m:\n    p number(min = 1_000, max = 0065_535)\n"),
            "model m:\n    p number(min = 1000, max = 65535)\n"
        );
        idempotent("model m:\n    p number(min = 1_000, max = 0065_535)\n");
    }

    /// `spec/style.md` §5: the author's DELIMITER stands. A `"""` string
    /// whose content fits one line is not collapsed into an escape-heavy
    /// `"…"`, which is how a JSON or regex payload stays readable.
    #[test]
    fn the_authors_string_delimiter_stands() {
        let triple = "const Payload =\n    \"\"\"\n    {\"k\": \"v\"}\n    \"\"\"\n";
        assert_eq!(
            fmt(triple),
            triple,
            "a one-line `\"\"\"` value is not collapsed"
        );
        idempotent(triple);
        let plain = "const Name = \"value\"\n";
        assert_eq!(fmt(plain), plain, "a `\"…\"` value is not promoted");
        // An EMPTY triple-quoted value has no body to write, so it renders
        // as the empty string.
        assert_eq!(
            fmt("service A:\n    v = \"\"\"\"\"\"\n"),
            "service A:\n    v = \"\"\n"
        );
    }

    /// `spec/style.md` §8.1 and §8.2, over the whole battery: the output
    /// parses (it re-enters the formatter) and formatting is idempotent.
    /// The `format` fuzz target owns these on arbitrary bytes; it reaches
    /// structured documents by luck, and the three shapes below — a value
    /// only a literal can carry, a blank line in a block body, a type's
    /// arrow — are the ones it never reached in 1.7M executions.
    #[test]
    fn the_output_parses_and_formatting_is_idempotent() {
        for (name, source) in battery() {
            let once = fmt(&source);
            let twice = format_source(&once)
                .unwrap_or_else(|e| panic!("{name}: the output does not parse: {e}\n{once}"));
            assert_eq!(once, twice, "{name}: formatting is not idempotent");
        }
    }

    /// `spec/style.md` §4: trailing whitespace is removed — in the
    /// formatter's OWN output, on every line, a multi-line string's body
    /// included. A blank line inside a `"""` body used to be written as a
    /// run of indentation with nothing after it: the file was fmt-clean
    /// until an editor's trim-on-save touched it, after which
    /// `nml fmt --check` failed and formatting put the spaces back —
    /// a save-loop between the two tools.
    #[test]
    fn the_output_never_carries_trailing_whitespace() {
        for (name, source) in battery() {
            let formatted = fmt(&source);
            for (i, line) in formatted.lines().enumerate() {
                assert_eq!(
                    line,
                    line.trim_end(),
                    "{name}: trailing whitespace on line {}",
                    i + 1
                );
            }
            // And the trimmed output is the SAME document: a trim-on-save
            // cannot put the file out of canonical style.
            let trimmed: String = formatted
                .lines()
                .map(|l| format!("{}\n", l.trim_end()))
                .collect();
            assert_eq!(trimmed, formatted, "{name}: trim-on-save changes the file");
        }
    }

    /// A `"""` BODY is spelled by this crate's own renderer, not the
    /// core's literal speller, so every escape the parser's
    /// source-character policy demands must be written HERE: a backslash
    /// doubles, a carriage return spells `\r`, and a banned character
    /// takes the policy's own `\u{…}`. Emitted raw, each one writes a
    /// document the parser REFUSES — over a file that was valid a moment
    /// earlier, at exit 0, with the value gone. The one-line branch is
    /// covered by the core's speller; this branch is this crate's own.
    #[test]
    fn a_block_body_escapes_every_character_the_parser_bans_raw() {
        let values = |src: &str| -> Vec<String> {
            nml_core::cst::parse(src)
                .syntax()
                .descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == nml_core::cst::SyntaxKind::String)
                .map(|t| nml_core::cst::decode_string(&t).expect("a string token decodes"))
                .collect()
        };
        for (name, source) in [
            (
                "a backslash",
                "service App:\n    a = \"\"\"\n        c:\\\\path\\\\to\n        \"\"\"\n",
            ),
            (
                "a carriage return",
                "service App:\n    a = \"\"\"\n        one\\rtwo\n        \"\"\"\n",
            ),
            (
                "an ESC control",
                "service App:\n    a = \"\"\"\n        esc\\u{1B}[0m\n        \"\"\"\n",
            ),
            (
                "a bidi override",
                "service App:\n    a = \"\"\"\n        bidi\\u{202E}flip\n        \"\"\"\n",
            ),
            (
                "a template body's backslash",
                "service App:\n    a = \"\"\"\n        hi {{args.n}} \\\\ x\n        \"\"\"\n",
            ),
        ] {
            let formatted = fmt(source);
            let again = format_source(&formatted).unwrap_or_else(|e| {
                panic!("{name}: the output is a document the parser refuses: {e}\n{formatted}")
            });
            assert_eq!(formatted, again, "{name}: formatting is not idempotent");
            assert_eq!(
                values(source),
                values(&formatted),
                "{name}: the value moved"
            );
        }
    }

    /// `spec/style.md` §6: a file that does not end in a line terminator
    /// is given one — its last line is WRITTEN, not dropped. That line
    /// reaches the printer unterminated, and finishing without flushing it
    /// would delete it: `nml fmt` writes this output over the file it
    /// read, so a dropped last line is a deleted declaration.
    #[test]
    fn a_file_with_no_final_terminator_keeps_its_last_line() {
        for (name, source, want) in [
            ("one declaration", "const A = 1", "const A = 1\n"),
            (
                "a property",
                "service App:\n    x = 1",
                "service App:\n    x = 1\n",
            ),
            ("a comment", "// note", "// note\n"),
            (
                "a block string",
                "const A = \"\"\"\n    body\n    \"\"\"",
                "const A = \"\"\"\n    body\n    \"\"\"\n",
            ),
            (
                "a CRLF file",
                "const A = 1\r\nconst B = 2",
                "const A = 1\r\nconst B = 2\r\n",
            ),
        ] {
            assert_eq!(fmt(source), want, "{name}");
            idempotent(source);
        }
    }

    /// `spec/style.md` §5: the author's DELIMITER stands — including for a
    /// string OUTSIDE a value node, whose one-line body gives the speller
    /// no line break to infer it from. A `"""` arm selector collapsed into
    /// a `"…"` is a spelling the formatter does not own.
    #[test]
    fn a_one_line_block_string_outside_a_value_keeps_its_delimiter() {
        let source =
            "oneof n by kind:\n    \"\"\"\n    log\n    \"\"\" -> nLog\n    \"email\" -> nEmail\n";
        assert_eq!(fmt(source), source, "the author's delimiter did not stand");
        idempotent(source);
        roundtrip(source);
    }

    /// `spec/style.md` §4 gives the author exactly two columns — the gap
    /// before an arm's `->` and the gap before a trailing comment — and
    /// the rule is the gap the author WROTE, whatever whitespace it is
    /// made of. A tab there stands. The same tab before a TYPE's arrow,
    /// which is not a column, canonicalizes like every other gap.
    #[test]
    fn a_tab_in_an_author_owned_column_is_the_gap_the_author_wrote() {
        for (name, source) in [
            (
                "a oneof arm's arrow",
                "oneof n by kind:\n    \"log\"\t-> nLog\n    \"email\" -> nEmail\n",
            ),
            (
                "a denial arm's arrow",
                "service App:\n    denial:\n        @plan/Pro\t-> ProUpsell\n        else -> Generic\n",
            ),
            ("a trailing comment", "service App:\n    x = 1\t// note\n"),
        ] {
            assert_eq!(
                fmt(source),
                source,
                "{name}: the author's gap did not stand"
            );
            idempotent(source);
        }
        assert_eq!(
            fmt("model m:\n    landing (role\t-> string)?\n"),
            "model m:\n    landing (role -> string)?\n",
            "a type's arrow is not a column"
        );
    }

    /// A value a `"""` block cannot carry stays a LITERAL. A block's
    /// closing delimiter must align with its content's indentation
    /// (NML0020), and a body whose every line is empty has no content
    /// column — rendering `"\n"` as a block wrote a document the parser
    /// rejects, over a file that was valid a moment earlier, at exit 0.
    /// The author could not even re-format it: `nml fmt` refuses an
    /// invalid document.
    #[test]
    fn a_value_no_block_can_carry_stays_a_literal() {
        for (name, source, want) in [
            (
                "one break",
                "service App:\n    x = \"\\n\"\n",
                "service App:\n    x = \"\\n\"\n",
            ),
            (
                "two breaks",
                "service App:\n    x = \"\\n\\n\"\n",
                "service App:\n    x = \"\\n\\n\"\n",
            ),
            (
                "at the file scope",
                "const X = \"\\n\"\n",
                "const X = \"\\n\"\n",
            ),
            (
                // The author DID write a block; it cannot hold this value.
                "an authored block of nothing but breaks",
                "const X = \"\"\"\n\n\n\"\"\"\n",
                "const X = \"\\n\"\n",
            ),
        ] {
            assert_eq!(fmt(source), want, "{name}");
            roundtrip(source);
            idempotent(source);
            // The VALUE is what must not move.
            let value = |src: &str| -> Vec<String> {
                nml_core::cst::parse(src)
                    .syntax()
                    .descendants_with_tokens()
                    .filter_map(|e| e.into_token())
                    .filter(|t| t.kind() == nml_core::cst::SyntaxKind::String)
                    .map(|t| nml_core::cst::decode_string(&t).expect("decodes"))
                    .collect()
            };
            assert_eq!(
                value(source),
                value(&fmt(source)),
                "{name}: the value moved"
            );
        }
    }

    /// `spec/style.md` §4: the right-hand column belongs to the author at
    /// an ARM's arrow. The same `->` inside a type is the map constructor,
    /// where there is no column to keep and the canonical single space
    /// applies — `->` is the fifth token the grammar gives two jobs.
    #[test]
    fn an_arms_arrow_keeps_its_column_and_a_types_arrow_does_not() {
        // An arm run, aligned by its author, survives byte for byte.
        let arms = "oneof notifier by kind:\n    \"log\"     -> notifierLog\n    \"email\"   -> notifierEmail\n";
        assert_eq!(fmt(arms), arms, "an arm's column is the author's");
        let routing = "service A:\n    denial:\n        @plan/Pro   -> ProUpsell\n        else        -> Generic\n";
        assert_eq!(fmt(routing), routing, "a routing arm's column too");
        // A type's arrow is layout the formatter owns, on both sides.
        assert_eq!(
            fmt("model m:\n    landing (role      -> string)?\n    r (string -> landing)?\n"),
            "model m:\n    landing (role -> string)?\n    r (string -> landing)?\n"
        );
        assert_eq!(
            fmt("model m:\n    r set<(role   ->   string)>\n"),
            "model m:\n    r set<(role -> string)>\n"
        );
        idempotent("model m:\n    landing (role      -> string)?\n");
    }

    /// `spec/style.md` §4, the spacing table the §4 example spells out, one
    /// row at a time. Each row is a separator the printer decides from two
    /// token KINDS, and each is a place a wrong decision writes a file that
    /// still parses — so only an assertion on the BYTES can see it.
    #[test]
    fn the_spacing_table_is_written_as_section_four_spells_it() {
        for (name, source, want) in [
            // A fallback chain's `|` is an operator, spaced both sides.
            (
                "a fallback chain",
                "service A:\n    x = $ENV.P|8080\n",
                "service A:\n    x = $ENV.P | 8080\n",
            ),
            // A trailing comment's column is the author's, but never
            // nothing: a comment glued to the code it follows is not a
            // column, it is a collision.
            (
                "a comment behind no gap at all",
                "service A:\n    x = 1// c\n",
                "service A:\n    x = 1 // c\n",
            ),
            // The array TYPE's brackets bind; the array VALUE's `]` does
            // not — `#live` is a separate thing on the line.
            (
                "a directive behind an array value",
                "model m:\n    xs []number = [1, 2]#live\n",
                "model m:\n    xs []number = [1, 2] #live\n",
            ),
            (
                "an array type and an array declaration still bind",
                "[]validator vs:\n    - a:\n        xs []  number\n",
                "[]validator vs:\n    - a:\n        xs []number\n",
            ),
            // An array's items are `, `-joined, a conjunction is spaced.
            (
                "array items and a role conjunction",
                "service A:\n    xs = [1,2,3]\n    g = @role/a&@role/b\n",
                "service A:\n    xs = [1, 2, 3]\n    g = @role/a & @role/b\n",
            ),
        ] {
            assert_eq!(fmt(source), want, "{name}");
            idempotent(source);
        }
    }

    /// A multi-line string OUTSIDE a value node — an arm's selector or
    /// target, a `oneof` arm's selector — re-indents with the line it is
    /// on, exactly as one inside a value does. Its body's depth is read
    /// from the line UNDER CONSTRUCTION, so the line must be open before
    /// the token is spelled; spelled first, the delimiter moved to the new
    /// indentation and the body stayed at the old one.
    #[test]
    fn a_multiline_string_outside_a_value_reindents_with_its_line() {
        let values = |src: &str| -> Vec<String> {
            nml_core::cst::parse(src)
                .syntax()
                .descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == nml_core::cst::SyntaxKind::String)
                .map(|t| nml_core::cst::decode_string(&t).expect("decodes"))
                .collect()
        };
        for (name, source, want) in [
            (
                "a oneof arm's selector",
                "model a:\n  x number\n\noneof n by k:\n  \"\"\"\n  multi\n  line\n  \"\"\" -> a\n",
                "model a:\n    x number\n\noneof n by k:\n    \"\"\"\n    multi\n    line\n    \"\"\" -> a\n",
            ),
            (
                "a routing arm's selector",
                "model landing:\n  label number\n\nmodel svc:\n  routing (string -> landing)?\n\nsvc Api:\n  routing:\n    \"\"\"\n    multi\n    line\n    \"\"\" -> \"up\"\n",
                "model landing:\n    label number\n\nmodel svc:\n    routing (string -> landing)?\n\nsvc Api:\n    routing:\n        \"\"\"\n        multi\n        line\n        \"\"\" -> \"up\"\n",
            ),
        ] {
            let formatted = fmt(source);
            assert_eq!(formatted, want, "{name}");
            assert_eq!(
                values(source),
                values(&formatted),
                "{name}: the value moved"
            );
            idempotent(source);
        }
    }

    /// A literal `{{` re-escapes in a `"""` body too, not only in a
    /// one-line literal: raw, the pair would make the value a TEMPLATE
    /// string on reparse — a different TYPE, whose segments a consumer
    /// reads differently. The §3 comment rule rides along: a comment's
    /// trailing whitespace goes, its text does not.
    #[test]
    fn a_blocks_braces_and_a_comments_tail_are_handled_like_everywhere_else() {
        let src = "service App:\n  y:\n    x = \"\"\"\n      a \\u{7B}{b}} c\n      \"\"\"\n";
        let formatted = fmt(src);
        assert!(formatted.contains("\\u{7B}{b}}"), "{formatted}");
        // The value is still a String, not a template.
        let value = |s: &str| match &nml_core::parse(s).unwrap().declarations[0].kind {
            nml_core::ast::DeclarationKind::Block(b) => match &b.body.entries[0].kind {
                nml_core::ast::BodyEntryKind::NestedBlock(n) => match &n.body.entries[0].kind {
                    nml_core::ast::BodyEntryKind::Property(p) => p.value.value.clone(),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        assert!(
            matches!(value(&formatted), nml_core::types::Value::String(_)),
            "a literal `{{{{` became a template: {formatted}"
        );
        assert_eq!(value(src), value(&formatted), "the value moved");
        idempotent(src);

        // §3: trailing whitespace inside a comment is removed; the rest of
        // the comment's text is untouched.
        assert_eq!(
            fmt("service A: // head   \n    x = 1\n    //  spaced   body   \n"),
            "service A: // head\n    x = 1\n    //  spaced   body\n"
        );
    }

    // -------------------------------------------------------------------
    // Round-trip tests
    // -------------------------------------------------------------------

    #[test]
    fn roundtrip_simple_block() {
        roundtrip("service App:\n    port = 8080\n    host = \"localhost\"\n");
    }

    #[test]
    fn roundtrip_uses_clause() {
        roundtrip("flow F uses base:\n    a = 1\n");
    }

    #[test]
    fn roundtrip_uses_multi_ref() {
        roundtrip("flow F uses alpha, beta, gamma:\n    a = 1\n");
    }

    #[test]
    fn roundtrip_uses_bodyless_no_colon() {
        // Pure stack assembly formats without a colon, mirroring `is`.
        assert_eq!(fmt("flow F uses base\n"), "flow F uses base\n");
        roundtrip("flow F uses base\n");
    }

    #[test]
    fn roundtrip_is_and_uses() {
        roundtrip("flow F is T uses base:\n    a = 1\n");
    }

    #[test]
    fn uses_clause_survives_format() {
        // The silent-drop hazard: an unemitted clause is DELETED by fmt.
        let formatted = fmt("flow F uses base:\n    a = 1\n");
        assert!(
            formatted.contains("uses base"),
            "clause dropped: {formatted}"
        );
    }

    #[test]
    fn roundtrip_nested_block() {
        roundtrip("server App:\n    port = 8080\n    db:\n        backend = \"postgres\"\n");
    }

    #[test]
    fn roundtrip_const() {
        roundtrip("const Port = 8080\n");
    }

    #[test]
    fn roundtrip_oneof_with_comments() {
        // Own-line comment before an arm, trailing comment on the header, and
        // trailing comment on an arm must all survive and be idempotent.
        roundtrip_comments(
            "// email transport\noneof email by provider: // tagged\n    // dev default\n    \"log\" -> emailLog // no delivery\n    \"postmark\" -> emailPostmark\n",
        );
    }

    #[test]
    fn roundtrip_multiple_blocks() {
        roundtrip("service A:\n    x = 1\n\nservice B:\n    y = 2\n");
    }

    #[test]
    fn roundtrip_preserves_nominal_annotation() {
        use nml_core::ast::{Body, BodyEntryKind, DeclarationKind};
        // RFC 0015 data-integrity guarantee: `as <Variant>` must survive —
        // dropping it would silently change a same-class union's variant.
        // Assert the text is preserved AND that the annotation survives a
        // parse→format→parse round-trip semantically.
        let field = "host H:\n    slot as modelB:\n        b = \"x\"\n";
        let elem = "host H:\n    slots:\n        - one as modelB:\n            b = \"x\"\n";

        fn slot_annotation(body: &Body, name: &str) -> Option<String> {
            body.entries.iter().find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == name => {
                    Some(nb.body.type_annotation.as_ref()?.name.clone())
                }
                _ => None,
            })
        }
        fn host_body(src: &str) -> Body {
            let formatted = fmt(src);
            // Text preserved verbatim (idempotent canonical form).
            assert_eq!(formatted, src, "annotation must survive formatting");
            match &parse(&formatted).unwrap().declarations[0].kind {
                DeclarationKind::Block(b) => b.body.clone(),
                _ => panic!("expected block"),
            }
        }

        assert_eq!(
            slot_annotation(&host_body(field), "slot").as_deref(),
            Some("modelB"),
            "field-level annotation must survive parse→format→parse"
        );
        // List-element level: descend into `slots` then find the named item.
        let elem_body = host_body(elem);
        let slots = elem_body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "slots" => Some(&nb.body),
                _ => None,
            })
            .unwrap();
        let item_ann = slots.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::ListItem(it) => match &it.kind {
                nml_core::ast::ListItemKind::Named { body, .. } => {
                    Some(body.type_annotation.as_ref()?.name.clone())
                }
                _ => None,
            },
            _ => None,
        });
        assert_eq!(
            item_ann.as_deref(),
            Some("modelB"),
            "list-element annotation must survive parse→format→parse"
        );
    }

    #[test]
    fn roundtrip_list_items() {
        roundtrip(
            "workflow W:\n    steps:\n        - step1:\n            x = 1\n        - step2:\n            y = 2\n",
        );
    }

    #[test]
    fn roundtrip_shorthand_list() {
        roundtrip("service S:\n    items:\n        - \"a\"\n        - \"b\"\n");
    }

    #[test]
    fn roundtrip_array_property() {
        roundtrip("service App:\n    tags = [\"web\", \"api\"]\n");
    }

    #[test]
    fn roundtrip_bool_values() {
        roundtrip("service App:\n    debug = true\n    verbose = false\n");
    }

    #[test]
    fn roundtrip_secret() {
        roundtrip("service App:\n    key = $ENV.SECRET\n");
    }

    #[test]
    fn roundtrip_fallback() {
        roundtrip("const Port = $ENV.PORT | 3000\n");
    }

    #[test]
    fn roundtrip_model() {
        roundtrip("model User:\n    name string\n    age number\n");
    }

    #[test]
    fn roundtrip_block_extends_with_body() {
        roundtrip("model plan is role:\n    name string\n");
    }

    #[test]
    fn roundtrip_block_extends_no_body() {
        roundtrip("model plan is role\n");
    }

    #[test]
    fn roundtrip_block_extends_multi_parent() {
        roundtrip("model admin is role, auditable:\n    level number\n");
    }

    #[test]
    fn roundtrip_enum() {
        roundtrip("enum Status:\n    - \"active\"\n    - \"inactive\"\n");
    }

    #[test]
    fn roundtrip_shared_property() {
        roundtrip(
            "workflow W:\n    .defaults:\n        retries = 3\n    - step1:\n        x = 1\n",
        );
    }

    #[test]
    fn roundtrip_scalar_shared_property() {
        roundtrip("workflow W:\n    .interval = 7200\n    - step1:\n        x = 1\n");
    }

    #[test]
    fn roundtrip_scalar_and_block_shared_property() {
        roundtrip(
            "workflow W:\n    .interval = 900\n    .defaults:\n        retries = 3\n    - step1:\n        x = 1\n",
        );
    }

    #[test]
    fn roundtrip_array_scalar_shared_property() {
        roundtrip("[]mount mounts:\n    .interval = 300\n    - Main:\n        path = \"/\"\n");
    }

    #[test]
    fn roundtrip_modifier() {
        roundtrip("service App:\n    port = 8080\n    |allow:\n        - @role/admin\n");
    }

    #[test]
    fn roundtrip_array_with_modifier() {
        roundtrip(
            "[]mount mounts:\n    |allow = [@authenticated]\n    - Main:\n        path = \"/\"\n",
        );
    }

    #[test]
    fn roundtrip_array_with_block_modifier() {
        roundtrip(
            "[]resource resources:\n    |allow:\n        - @role/admin\n        - @role/editor\n\n    - Dashboard:\n        path = \"/dashboard\"\n",
        );
    }

    #[test]
    fn roundtrip_array_with_multiple_modifiers() {
        roundtrip(
            "[]route routes:\n    |allow = [@authenticated]\n    |deny = [@anonymous]\n\n    - Home:\n        path = \"/\"\n",
        );
    }

    #[test]
    fn idempotent_array_with_modifier() {
        idempotent(
            "[]mount mounts:\n    |allow = [@authenticated]\n\n    - Main:\n        path = \"/\"\n",
        );
    }

    // -------------------------------------------------------------------
    // Idempotency tests
    // -------------------------------------------------------------------

    #[test]
    fn idempotent_simple() {
        idempotent("service App:\n    port = 8080\n    host = \"localhost\"\n");
    }

    #[test]
    fn idempotent_nested() {
        idempotent("server S:\n    db:\n        url = \"postgres://localhost\"\n");
    }

    #[test]
    fn idempotent_complex() {
        idempotent(
            "workflow W:\n    steps:\n        - s1:\n            provider = \"fast\"\n        - s2:\n            provider = \"slow\"\n",
        );
    }

    // -------------------------------------------------------------------
    // Edge cases
    // -------------------------------------------------------------------

    #[test]
    fn format_empty_file() {
        let formatted = fmt("");
        assert!(formatted.is_empty() || formatted.trim().is_empty());
    }

    #[test]
    fn format_negative_number() {
        roundtrip("service App:\n    offset = -10\n");
    }

    #[test]
    fn format_float_number() {
        roundtrip("service App:\n    rate = 0.75\n");
    }

    #[test]
    fn format_empty_array() {
        roundtrip("service App:\n    tags = []\n");
    }

    #[test]
    fn format_single_item_array() {
        roundtrip("service App:\n    tags = [\"web\"]\n");
    }

    #[test]
    fn formats_cst_only_syntax() {
        // Nested array types (`[][]string`, `[](string | int)`) reach the
        // printer as a shape no other case here exercises: the element type
        // is itself a type expression. Format and re-format, so a printer
        // that renders the inner brackets once and the outer twice fails on
        // the second pass rather than on nobody's.
        let src = "model M:\n    grid [][]string\n    pairs [](string | int)\n";
        let out = format_source(src).expect("CST-only syntax should format");
        assert!(out.contains("grid [][]string"), "{out}");
        assert_eq!(out, format_source(&out).unwrap(), "must be idempotent");
    }

    // -------------------------------------------------------------------
    // Comment preservation
    // -------------------------------------------------------------------

    #[test]
    fn comments_file_header() {
        let out = roundtrip_comments(
            "// Application config\n// Edit with care.\nservice App:\n    port = 8080\n",
        );
        assert!(out.starts_with("// Application config\n// Edit with care.\nservice App:"));
    }

    #[test]
    fn comments_between_declarations() {
        let out = roundtrip_comments(
            "service A:\n    x = 1\n\n// second service\nservice B:\n    y = 2\n",
        );
        assert!(out.contains("\n\n// second service\nservice B:"));
    }

    #[test]
    fn comments_inside_body() {
        let out = roundtrip_comments(
            "service App:\n    // network settings\n    port = 8080\n    host = \"localhost\"\n",
        );
        assert!(out.contains("\n    // network settings\n    port = 8080\n"));
    }

    #[test]
    fn comments_trailing_property() {
        let out = roundtrip_comments("service App:\n    port = 8080 // default port\n");
        assert!(out.contains("port = 8080 // default port\n"));
    }

    #[test]
    fn comments_trailing_block_header() {
        let out = roundtrip_comments("service App: // main entry\n    port = 8080\n");
        assert!(out.contains("service App: // main entry\n"));
    }

    #[test]
    fn comments_trailing_nested_block_header() {
        let out = roundtrip_comments(
            "server S:\n    db: // database settings\n        url = \"postgres://x\"\n",
        );
        assert!(out.contains("    db: // database settings\n"));
    }

    #[test]
    fn comments_in_nested_body() {
        let out = roundtrip_comments(
            "server S:\n    db:\n        // connection\n        url = \"postgres://x\"\n",
        );
        assert!(out.contains("\n        // connection\n        url = \"postgres://x\"\n"));
    }

    #[test]
    fn comments_after_last_entry_keep_nesting() {
        let out = roundtrip_comments(
            "server S:\n    db:\n        url = \"x\"\n        // todo: add pool size\n    port = 1\n",
        );
        assert!(
            out.contains("\n        // todo: add pool size\n    port = 1\n"),
            "comment should keep its original nesting:\n{out}"
        );
    }

    #[test]
    fn comments_at_end_of_file() {
        let out = roundtrip_comments("service App:\n    port = 8080\n    // end of config\n");
        assert!(out.ends_with("    // end of config\n"));
    }

    #[test]
    fn comments_only_file() {
        let out = roundtrip_comments("// nothing here yet\n");
        assert_eq!(out, "// nothing here yet\n");
    }

    #[test]
    fn comments_on_const_and_template() {
        let out = roundtrip_comments(
            "// the port\nconst Port = 8080 // tcp\n\ntemplate Greeting: // says hi\n    \"hello\"\n",
        );
        assert!(out.contains("// the port\nconst Port = 8080 // tcp\n"));
        assert!(out.contains("template Greeting: // says hi\n"));
    }

    #[test]
    fn comments_in_list_items() {
        let out = roundtrip_comments(
            "workflow W:\n    steps:\n        // first step\n        - s1: // classify\n            x = 1\n",
        );
        assert!(out.contains("\n        // first step\n        - s1: // classify\n"));
    }

    #[test]
    fn comments_in_array_declaration() {
        let out = roundtrip_comments(
            "[]mount mounts:\n    // defaults for every mount\n    .interval = 300\n\n    // the root mount\n    - Main:\n        path = \"/\"\n",
        );
        assert!(out.contains("// defaults for every mount"));
        assert!(out.contains("// the root mount"));
    }

    #[test]
    fn comments_divider_preserved_verbatim() {
        let out = roundtrip_comments("//// section ////\nservice App:\n    port = 1\n");
        assert!(out.starts_with("//// section ////\n"));
    }

    #[test]
    fn comment_like_string_untouched() {
        let out =
            roundtrip_comments("service App:\n    url = \"https://example.com\" // real comment\n");
        assert!(out.contains("url = \"https://example.com\" // real comment\n"));
    }

    #[test]
    fn comments_format_source_full_document() {
        let source = "\
// Demo configuration
service App: // main
    // network
    port = 8080 // tcp
    host = \"localhost\"
    db:
        // creds come from env
        url = $ENV.DB_URL | \"postgres://localhost\"
    // misc below
    debug = true

// roles
[]mount mounts:
    - Main: // root
        path = \"/\"
// eof note
";
        roundtrip_comments(source);
    }

    /// There is no comment-less entry point any more. The AST-only
    /// `format(&File)` had no consumer and one guarantee — that it DROPPED
    /// every comment — so it left with the AST re-emitter. A comment now
    /// survives every door into this crate, because comments are tokens in
    /// the tree the printer walks.
    #[test]
    fn every_door_preserves_comments() {
        let source = "service App:\n    port = 8080 // kept\n";
        assert_eq!(fmt(source), source);
    }

    /// RFC 0014: the formatter renders role conjunctions from the LOWERED
    /// value, so tight/ragged source spacing canonicalizes to `" & "`
    /// automatically — and the output is idempotent.
    #[test]
    fn role_conjunction_canonicalizes_and_is_idempotent() {
        let src = "service App:\n    gate = @role/a&@role/b\n    allow = [@role/x  &  @role/y, @plan/Pro]\n";
        let formatted = format_source(src).unwrap();
        assert!(
            formatted.contains("gate = @role/a & @role/b"),
            "{formatted}"
        );
        assert!(
            formatted.contains("[@role/x & @role/y, @plan/Pro]"),
            "{formatted}"
        );
        let again = format_source(&formatted).unwrap();
        assert_eq!(formatted, again, "conjunction formatting is not idempotent");
    }

    // -------------------------------------------------------------------
    // Fixture round-trip tests
    // -------------------------------------------------------------------

    #[test]
    fn roundtrip_fixture_minimal_service() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/minimal-service.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_fixture_full_service() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/full-service.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_fixture_web_server() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/web-server.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_fixture_role_templates() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/role-templates.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_fixture_secret_values() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/secret-values.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_fixture_money_values() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/money-values.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_fixture_pricing() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/pricing.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_inline_role_refs() {
        roundtrip("mount Api:\n    path = \"/api\"\n    |allow = [@public, @role/admin]\n");
    }

    #[test]
    fn roundtrip_block_role_ref_list() {
        roundtrip(
            "role admin:\n    members:\n        - @role/editor\n        - @user/test@example.com\n",
        );
    }

    #[test]
    fn roundtrip_value_role_property() {
        roundtrip("service App:\n    access = @role/admin\n");
    }
}
