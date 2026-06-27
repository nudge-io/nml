//! Lossless lexer for the NML CST (RFC 0004 P1).
//!
//! Unlike the legacy lexer (which discards whitespace, sidelines comments, and
//! *decodes* literals into values), this emits a flat token stream that **covers
//! 100% of source bytes**: every byte lands in exactly one token, including
//! whitespace/comments (trivia) and unrecognized input (`ErrorToken`). The
//! offside rule (RFC 0004 §4.2.1) is carried by **zero-width** `Indent`/`Dedent`
//! structural tokens; the literal indentation is separate `Whitespace` trivia.
//! Multi-line `"""…"""` strings are a single token, so layout is **suppressed
//! inside them** — the key NML-specific correctness property.
//!
//! Two deliberate departures from the legacy lexer, both strictly cleaner:
//!
//! - **Tokens carry raw source text; the lexer does not interpret.** Escape
//!   decoding, multiline dedent, number range-checking, secret-namespace
//!   validation, and `true`/`false`/currency classification are *semantic* and
//!   move to the value-interpretation layer (P3/P5). The lexer is purely
//!   syntactic and lossless. (Capability is preserved — relocated, not dropped.)
//! - **Context-free tokenization.** The legacy stateful look-back for negative
//!   numbers and look-ahead for the `[]` array-prefix are removed: `-`, `[`, `]`
//!   are atoms (`Dash`/`LBracket`/`RBracket`) and the parser composes negative
//!   numbers and `[]type`. No token depends on prior tokens.
//!
//! The lexer is *resilient*: malformed input (bad escape, tab indentation, …)
//! yields a diagnostic and a token, never an abort — diagnostics are bounded
//! (`super::MAX_ERRORS`).

use crate::cst::syntax::SyntaxKind;
use crate::error::NmlError;
use crate::span::Span;

/// A lexed token: its kind and the exact source slice it covers (empty for the
/// zero-width markers `Indent`/`Dedent`).
pub(super) struct LexToken<'a> {
    pub kind: SyntaxKind,
    pub text: &'a str,
    pub offset: usize,
}

/// The full lossless token stream plus any lexical diagnostics.
pub(super) struct Lexed<'a> {
    pub tokens: Vec<LexToken<'a>>,
    pub errors: Vec<NmlError>,
}

pub(super) fn lex(src: &str) -> Lexed<'_> {
    Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        indent: vec![0],
        at_line_start: true,
        tokens: Vec::new(),
        errors: Vec::new(),
    }
    .run()
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    /// Offside indent stack (column widths). Always non-empty (`[0, …]`).
    indent: Vec<usize>,
    at_line_start: bool,
    tokens: Vec<LexToken<'a>>,
    errors: Vec<NmlError>,
}

impl<'a> Lexer<'a> {
    fn run(mut self) -> Lexed<'a> {
        while self.pos < self.bytes.len() {
            if self.at_line_start {
                self.handle_line_start();
            } else {
                self.scan_token();
            }
        }
        // Close any open blocks. No explicit `Eof` token is emitted: the parser
        // synthesizes the end sentinel when its cursor runs off the end, so a
        // physical zero-width `Eof` token would only litter the tree.
        while self.indent.len() > 1 {
            self.indent.pop();
            self.push_empty(SyntaxKind::Dedent);
        }
        Lexed {
            tokens: self.tokens,
            errors: self.errors,
        }
    }

    /// At the first column of a logical line: emit offside markers for the
    /// indentation change, then the leading whitespace as trivia. Blank and
    /// comment-only lines do **not** affect indentation (offside rule).
    fn handle_line_start(&mut self) {
        self.at_line_start = false;
        let col_start = self.pos;

        let mut ws_end = self.pos;
        let mut width = 0usize;
        let mut has_tab = false;
        while let Some(b @ (b' ' | b'\t')) = self.bytes.get(ws_end).copied() {
            has_tab |= b == b'\t';
            ws_end += 1;
            width += 1;
        }
        // Blank and comment-only lines do not affect indentation (offside rule).
        let blank = is_blank_after(self.bytes, ws_end);
        // Spec: tabs are not permitted in indentation. Resilient (diagnose and
        // continue) rather than the legacy hard error; the tab stays in the tree
        // as whitespace, so losslessness holds. (Blank lines are exempt — their
        // indentation is meaningless.)
        if has_tab && !blank {
            self.push_error(
                "tabs are not permitted in indentation; use spaces",
                Span::empty(self.pos),
            );
        }
        if blank {
            self.emit_ws(col_start, ws_end);
            return;
        }

        let top = *self.indent.last().expect("indent stack is never empty");
        if width > top {
            self.indent.push(width);
            self.push_empty_at(SyntaxKind::Indent, col_start);
        } else if width < top {
            while *self.indent.last().expect("non-empty") > width {
                self.indent.pop();
                self.push_empty_at(SyntaxKind::Dedent, col_start);
            }
            if *self.indent.last().expect("non-empty") != width {
                // Inconsistent dedent (a column matching no open level): recover
                // by snapping to this level and recording a diagnostic — never a
                // panic (RFC 0004 §4.2.1).
                self.indent.push(width);
                self.push_empty_at(SyntaxKind::ErrorToken, col_start);
                self.push_error(
                    "inconsistent dedent: indentation matches no enclosing block",
                    Span::empty(col_start),
                );
            }
        }
        self.emit_ws(col_start, ws_end);
    }

    fn emit_ws(&mut self, start: usize, end: usize) {
        if end > start {
            self.push(SyntaxKind::Whitespace, start, end);
            self.pos = end;
        }
    }

    fn scan_token(&mut self) {
        let start = self.pos;
        match self.bytes[start] {
            // `\r` is treated as inter-token whitespace, so CRLF line endings lex
            // cleanly (the `\n` still drives the layout pass).
            b' ' | b'\t' | b'\r' => {
                let mut end = start;
                while matches!(self.bytes.get(end), Some(b' ') | Some(b'\t') | Some(b'\r')) {
                    end += 1;
                }
                self.push(SyntaxKind::Whitespace, start, end);
                self.pos = end;
            }
            b'\n' => {
                self.push(SyntaxKind::Newline, start, start + 1);
                self.pos = start + 1;
                self.at_line_start = true;
            }
            b'/' if self.bytes.get(start + 1) == Some(&b'/') => {
                let mut end = start;
                while !matches!(self.bytes.get(end), None | Some(b'\n')) {
                    end += 1;
                }
                self.push(SyntaxKind::Comment, start, end);
                self.pos = end;
            }
            b'"' => self.scan_string(),
            b'=' if self.bytes.get(start + 1) == Some(&b'>') => {
                self.fixed(SyntaxKind::FatArrow, 2)
            }
            b'=' => self.single(SyntaxKind::Eq),
            b':' => self.single(SyntaxKind::Colon),
            b'-' => self.single(SyntaxKind::Dash),
            b'|' => self.single(SyntaxKind::Pipe),
            b'.' => self.single(SyntaxKind::Dot),
            b'[' => self.single(SyntaxKind::LBracket),
            b']' => self.single(SyntaxKind::RBracket),
            b'(' => self.single(SyntaxKind::LParen),
            b')' => self.single(SyntaxKind::RParen),
            b',' => self.single(SyntaxKind::Comma),
            b'?' => self.single(SyntaxKind::Question),
            b'@' => self.scan_role(),
            b'$' => self.scan_secret(),
            c if is_ident_start(c) => {
                let mut end = start;
                while self.bytes.get(end).copied().is_some_and(is_ident_continue) {
                    end += 1;
                }
                self.push(SyntaxKind::Ident, start, end);
                self.pos = end;
            }
            c if c.is_ascii_digit() => {
                let mut end = start;
                while self
                    .bytes
                    .get(end)
                    .copied()
                    .is_some_and(|b| b.is_ascii_digit() || b == b'.')
                {
                    end += 1;
                }
                self.push(SyntaxKind::Number, start, end);
                self.pos = end;
            }
            c => {
                // Unrecognized: consume one whole UTF-8 char (so slices stay on
                // char boundaries) into an ErrorToken — losslessness preserved.
                let end = start + utf8_len(c);
                self.push(SyntaxKind::ErrorToken, start, end);
                self.pos = end;
                self.push_error("unexpected character", Span::new(start, end));
            }
        }
    }

    /// A `"…"` or `"""…"""` string. Multi-line triple-quoted strings consume
    /// their internal newlines as token content, so the offside layout pass
    /// never runs inside them (RFC 0004 §4.2.1).
    fn scan_string(&mut self) {
        let start = self.pos;
        if self.bytes[start..].starts_with(b"\"\"\"") {
            let mut end = start + 3;
            loop {
                // Escape-aware boundary: a `\X` pair is skipped, so an escaped
                // quote cannot count toward the closing `"""` (parity with the
                // legacy lexer). Text stays raw; decoding is the value layer's job.
                if self.bytes.get(end) == Some(&b'\\') && end + 1 < self.bytes.len() {
                    end += 2;
                    continue;
                }
                if self.bytes[end..].starts_with(b"\"\"\"") {
                    end += 3;
                    break;
                }
                if end >= self.bytes.len() {
                    self.push_error("unterminated multi-line string", Span::new(start, end));
                    break;
                }
                end += 1;
            }
            self.push(SyntaxKind::String, start, end);
            self.pos = end;
        } else {
            let mut end = start + 1;
            loop {
                match self.bytes.get(end) {
                    None | Some(b'\n') => {
                        self.push_error("unterminated string", Span::new(start, end));
                        break;
                    }
                    Some(b'"') => {
                        end += 1;
                        break;
                    }
                    Some(b'\\') if end + 1 < self.bytes.len() => end += 2,
                    Some(_) => end += 1,
                }
            }
            self.push(SyntaxKind::String, start, end);
            self.pos = end;
        }
    }

    fn single(&mut self, kind: SyntaxKind) {
        self.fixed(kind, 1);
    }

    fn fixed(&mut self, kind: SyntaxKind, len: usize) {
        let start = self.pos;
        self.push(kind, start, start + len);
        self.pos = start + len;
    }

    /// A role reference `@…` (`@role/admin`, `@public`). Raw text; the value
    /// layer interprets it. A trailing `:` at line end is given back so
    /// `@public:` lexes as a role plus a `Colon`.
    fn scan_role(&mut self) {
        let start = self.pos;
        let mut end = start + 1; // skip '@'
        while self
            .bytes
            .get(end)
            .copied()
            .is_some_and(is_role_continue)
        {
            end += 1;
        }
        if self.bytes.get(end - 1) == Some(&b':')
            && matches!(
                self.bytes.get(end).copied(),
                None | Some(b'\n') | Some(b' ') | Some(b'\t')
            )
        {
            end -= 1;
        }
        self.push(SyntaxKind::Role, start, end);
        self.pos = end;
    }

    /// A variable reference `$…` (`$ENV.MY_VAR`). Raw text spanning `$` through
    /// the dotted key; namespace validity is checked at the value layer, not
    /// here (the lexer stays context-free and resilient).
    fn scan_secret(&mut self) {
        let start = self.pos;
        let mut end = start + 1; // skip '$'
        while self
            .bytes
            .get(end)
            .copied()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
        {
            end += 1;
        }
        self.push(SyntaxKind::Secret, start, end);
        self.pos = end;
    }

    fn push(&mut self, kind: SyntaxKind, start: usize, end: usize) {
        self.tokens.push(LexToken {
            kind,
            text: &self.src[start..end],
            offset: start,
        });
    }

    fn push_empty(&mut self, kind: SyntaxKind) {
        self.push_empty_at(kind, self.pos);
    }

    fn push_empty_at(&mut self, kind: SyntaxKind, at: usize) {
        self.tokens.push(LexToken {
            kind,
            text: &self.src[at..at],
            offset: at,
        });
    }

    /// Record a diagnostic, capped at [`MAX_ERRORS`](super::MAX_ERRORS) so a
    /// pathological file cannot grow the error list without bound *during*
    /// lexing (RFC 0004 §9, "bounded output"). Tokens are still emitted, so
    /// losslessness is unaffected by the cap.
    fn push_error(&mut self, message: &str, span: Span) {
        if self.errors.len() < super::MAX_ERRORS {
            self.errors.push(NmlError::lex(message, span));
        }
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_role_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'/' | b':' | b'-' | b'_' | b'.' | b'@' | b'{' | b'}' | b'+')
}

/// True if the line at byte `i` (after its leading whitespace) is blank or a
/// comment-only line — such lines do not affect indentation (offside rule).
fn is_blank_after(bytes: &[u8], i: usize) -> bool {
    match bytes.get(i) {
        None | Some(b'\n') => true,
        Some(b'\r') if bytes.get(i + 1) == Some(&b'\n') => true, // CRLF blank line
        Some(b'/') if bytes.get(i + 1) == Some(&b'/') => true,
        _ => false,
    }
}

/// Byte length of the UTF-8 char beginning with lead byte `b` (continuation
/// bytes — never a valid start — count as 1, defensively).
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Non-trivia token kinds, in order.
    fn kinds(src: &str) -> Vec<SyntaxKind> {
        lex(src)
            .tokens
            .iter()
            .filter(|t| !t.kind.is_trivia())
            .map(|t| t.kind)
            .collect()
    }

    /// Foundational invariant: token texts concatenate back to the source.
    fn assert_lossless(src: &str) {
        let joined: String = lex(src).tokens.iter().map(|t| t.text).collect();
        assert_eq!(joined, src, "token texts must reconstruct the source");
    }

    #[test]
    fn all_atom_and_punctuation_kinds() {
        use SyntaxKind::*;
        // A line exercising every atom and punctuation token.
        let src = "x = \"s\" 12.5 @role/admin $ENV.KEY => : - | . [ ] ( ) , ?";
        assert_eq!(
            kinds(src),
            vec![
                Ident, Eq, String, Number, Role, Secret, FatArrow, Colon, Dash, Pipe, Dot,
                LBracket, RBracket, LParen, RParen, Comma, Question,
            ]
        );
        assert_lossless(src);
    }

    #[test]
    fn context_free_negative_number_and_array_prefix() {
        // No stateful look-back/ahead: `-` is Dash, `[]` is two brackets; the
        // parser composes negatives and `[]type`.
        assert_eq!(kinds("x = -5"), vec![SyntaxKind::Ident, SyntaxKind::Eq, SyntaxKind::Dash, SyntaxKind::Number]);
        assert_eq!(kinds("[]route"), vec![SyntaxKind::LBracket, SyntaxKind::RBracket, SyntaxKind::Ident]);
    }

    #[test]
    fn hyphen_in_identifier_vs_dash() {
        // `my-svc` is one Ident; a leading `-` (list dash) is Dash.
        assert_eq!(kinds("my-svc"), vec![SyntaxKind::Ident]);
        assert_eq!(kinds("- item"), vec![SyntaxKind::Dash, SyntaxKind::Ident]);
    }

    #[test]
    fn role_trailing_colon_given_back() {
        // `@public:` at line end → Role + Colon (the `:` is structural).
        assert_eq!(kinds("- @public:\n"), vec![SyntaxKind::Dash, SyntaxKind::Role, SyntaxKind::Colon]);
        let toks = lex("@public:\n");
        assert_eq!(toks.tokens[0].text, "@public");
    }

    #[test]
    fn secret_is_raw_text_no_validation() {
        // The lexer does not validate the namespace; it spans `$…` and defers.
        let toks = lex("k = $ENV.MY_VAR\n");
        let secret = toks.tokens.iter().find(|t| t.kind == SyntaxKind::Secret).unwrap();
        assert_eq!(secret.text, "$ENV.MY_VAR");
        // Even an unknown namespace lexes losslessly (value layer flags it).
        assert_lossless("k = $NOPE.X\n");
    }

    #[test]
    fn strings_keep_raw_text_including_escapes_and_newlines() {
        // No decoding: the String token covers the raw bytes verbatim.
        let toks = lex("s = \"a\\nb\"\n");
        let s = toks.tokens.iter().find(|t| t.kind == SyntaxKind::String).unwrap();
        assert_eq!(s.text, "\"a\\nb\"");
        let ml = "s = \"\"\"\n  raw\n\"\"\"\n";
        assert_lossless(ml);
        assert_eq!(kinds(ml).iter().filter(|k| **k == SyntaxKind::String).count(), 1);
    }

    #[test]
    fn tab_indentation_is_diagnosed_but_lossless() {
        let src = "service App:\n\tport = 1\n";
        let lexed = lex(src);
        assert!(
            lexed.errors.iter().any(|e| e.message().contains("tabs are not permitted")),
            "expected a tab diagnostic"
        );
        assert_lossless(src); // resilient: diagnosed, not aborted
    }

    #[test]
    fn tab_inside_value_is_fine() {
        // Tabs are only forbidden in *indentation*; inter-token/string tabs are ok.
        assert!(lex("x =\t1\n").errors.is_empty());
        assert_lossless("x =\t1\n");
    }

    #[test]
    fn crlf_line_endings_lex_cleanly() {
        // CRLF must not produce per-line errors (exceeds the legacy hard-error).
        let src = "service App:\r\n    port = 1\r\n\r\n    host = 2\r\n";
        let lexed = lex(src);
        assert!(lexed.errors.is_empty(), "CRLF should lex clean: {:?}", lexed.errors);
        assert_lossless(src);
        // The blank CRLF line must not spuriously dedent: the body stays open,
        // so there is exactly one Dedent (at EOF) for the one block.
        let dedents = lexed.tokens.iter().filter(|t| t.kind == SyntaxKind::Dedent).count();
        assert_eq!(dedents, 1, "blank CRLF line must not dedent");
    }

    #[test]
    fn triple_string_boundary_is_escape_aware() {
        // `\"""` must not be read as content-plus-close: the `\"` is an escaped
        // quote, so this triple string is unterminated (parity with legacy).
        let lexed = lex("s = \"\"\"abc\\\"\"\"");
        assert!(
            lexed.errors.iter().any(|e| e.message().contains("unterminated")),
            "escaped quote should not close the triple string: {:?}",
            lexed.errors
        );
        assert_lossless("s = \"\"\"abc\\\"\"\"");
    }
}
