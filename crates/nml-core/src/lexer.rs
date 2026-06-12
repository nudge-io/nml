//! Lexer (tokenizer) for NML source text.
//!
//! Converts raw NML source into a stream of [`Token`]s with span information,
//! handling indentation-based scoping (similar to Python).

use crate::error::{NmlError, NmlResult};
use crate::span::Span;

/// The kind of token produced by the lexer.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Structure
    Indent,
    Dedent,
    Newline,
    Eof,

    // Literals
    StringLiteral(String),
    NumberLiteral(crate::types::Number),
    BoolLiteral(bool),
    Identifier(String),
    Role(String),         // @role/admin, @public, etc.
    SecretRef(String),    // $ENV.MY_SECRET
    CurrencyCode(String), // USD, GBP, etc.

    // Punctuation
    Colon,        // :
    Equals,       // =
    Dash,         // -
    Pipe,         // |
    Dot,          // .
    BracketOpen,  // [
    BracketClose, // ]
    ArrayPrefix,  // []
    Comma,        // ,
    Question,     // ?
    ParenOpen,    // (
    ParenClose,   // )
}

/// A single token with its source location.
#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

/// A source comment captured during lexing.
///
/// Comments are not part of the token stream or the AST; they are exposed
/// as a side channel (see [`crate::parser::parse_with_comments`]) so that
/// tools like the formatter can preserve them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// Comment text after the leading `//`, verbatim except for trailing
    /// whitespace (so `////` dividers and deliberate spacing survive).
    pub text: String,
    /// Span covering the comment from `//` to end of line (exclusive of
    /// the newline).
    pub span: Span,
    /// True when the comment is alone on its line (only whitespace before
    /// it); false when it trails code on the same line.
    pub own_line: bool,
}

/// Indent-aware tokenizer for NML source text.
pub struct Lexer<'a> {
    source: &'a str,
    chars: Vec<char>,
    byte_offsets: Vec<usize>,
    pos: usize,
    indent_stack: Vec<usize>,
    at_line_start: bool,
    comments: Vec<Comment>,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        let mut chars = Vec::new();
        let mut byte_offsets = Vec::new();
        for (byte_idx, ch) in source.char_indices() {
            chars.push(ch);
            byte_offsets.push(byte_idx);
        }
        Self {
            source,
            chars,
            byte_offsets,
            pos: 0,
            indent_stack: vec![0],
            at_line_start: true,
            comments: Vec::new(),
        }
    }

    /// Consume the comments recorded during [`Lexer::tokenize`], in source
    /// order.
    pub fn take_comments(&mut self) -> Vec<Comment> {
        std::mem::take(&mut self.comments)
    }

    fn byte_pos(&self) -> usize {
        self.byte_offsets
            .get(self.pos)
            .copied()
            .unwrap_or(self.source.len())
    }

    fn byte_pos_at(&self, char_idx: usize) -> usize {
        self.byte_offsets
            .get(char_idx)
            .copied()
            .unwrap_or(self.source.len())
    }

    pub fn tokenize(&mut self) -> NmlResult<Vec<Token>> {
        let mut tokens = Vec::new();

        loop {
            if self.pos >= self.chars.len() {
                while self.indent_stack.len() > 1 {
                    self.indent_stack.pop();
                    tokens.push(Token::new(TokenKind::Dedent, Span::empty(self.byte_pos())));
                }
                tokens.push(Token::new(TokenKind::Eof, Span::empty(self.byte_pos())));
                break;
            }

            if self.at_line_start {
                self.handle_indentation(&mut tokens)?;
                self.at_line_start = false;
                continue;
            }

            let ch = self.chars[self.pos];

            match ch {
                ' ' | '\t' => {
                    self.pos += 1;
                }
                '\n' => {
                    let bp = self.byte_pos();
                    tokens.push(Token::new(TokenKind::Newline, Span::new(bp, bp + 1)));
                    self.pos += 1;
                    self.at_line_start = true;
                }
                '/' if self.peek_char() == Some('/') => {
                    self.consume_comment(false);
                }
                '"' => {
                    if self.peek_char() == Some('"') && self.chars.get(self.pos + 2) == Some(&'"') {
                        tokens.push(self.read_multiline_string()?);
                    } else {
                        tokens.push(self.read_string()?);
                    }
                }
                ':' => {
                    let bp = self.byte_pos();
                    tokens.push(Token::new(TokenKind::Colon, Span::new(bp, bp + 1)));
                    self.pos += 1;
                }
                '=' => {
                    let bp = self.byte_pos();
                    tokens.push(Token::new(TokenKind::Equals, Span::new(bp, bp + 1)));
                    self.pos += 1;
                }
                '-' => {
                    if self.peek_char().is_some_and(|c| c.is_ascii_digit()) {
                        let prev_meaningful = tokens
                            .iter()
                            .rev()
                            .find(|t| !matches!(t.kind, TokenKind::Newline | TokenKind::Indent));
                        if prev_meaningful.is_some_and(|t| {
                            matches!(
                                t.kind,
                                TokenKind::Equals | TokenKind::BracketOpen | TokenKind::Comma
                            )
                        }) {
                            tokens.push(self.read_number()?);
                        } else {
                            let bp = self.byte_pos();
                            tokens.push(Token::new(TokenKind::Dash, Span::new(bp, bp + 1)));
                            self.pos += 1;
                        }
                    } else {
                        let bp = self.byte_pos();
                        tokens.push(Token::new(TokenKind::Dash, Span::new(bp, bp + 1)));
                        self.pos += 1;
                    }
                }
                '|' => {
                    let bp = self.byte_pos();
                    tokens.push(Token::new(TokenKind::Pipe, Span::new(bp, bp + 1)));
                    self.pos += 1;
                }
                '(' => {
                    let bp = self.byte_pos();
                    tokens.push(Token::new(TokenKind::ParenOpen, Span::new(bp, bp + 1)));
                    self.pos += 1;
                }
                ')' => {
                    let bp = self.byte_pos();
                    tokens.push(Token::new(TokenKind::ParenClose, Span::new(bp, bp + 1)));
                    self.pos += 1;
                }
                '.' => {
                    let bp = self.byte_pos();
                    tokens.push(Token::new(TokenKind::Dot, Span::new(bp, bp + 1)));
                    self.pos += 1;
                }
                '[' => {
                    if self.peek_char() == Some(']') {
                        let span = Span::new(self.byte_pos(), self.byte_pos_at(self.pos + 2));
                        let after_bracket = if self.pos + 2 < self.chars.len() {
                            Some(self.chars[self.pos + 2])
                        } else {
                            None
                        };
                        if after_bracket.is_some_and(|c| c.is_alphabetic() || c == '_' || c == '(')
                        {
                            tokens.push(Token::new(TokenKind::ArrayPrefix, span));
                        } else {
                            let bp = self.byte_pos();
                            tokens.push(Token::new(TokenKind::BracketOpen, Span::new(bp, bp + 1)));
                            self.pos += 1;
                            let bp = self.byte_pos();
                            tokens.push(Token::new(TokenKind::BracketClose, Span::new(bp, bp + 1)));
                            self.pos += 1;
                            continue;
                        }
                        self.pos += 2;
                    } else {
                        let bp = self.byte_pos();
                        tokens.push(Token::new(TokenKind::BracketOpen, Span::new(bp, bp + 1)));
                        self.pos += 1;
                    }
                }
                ']' => {
                    let bp = self.byte_pos();
                    tokens.push(Token::new(TokenKind::BracketClose, Span::new(bp, bp + 1)));
                    self.pos += 1;
                }
                ',' => {
                    let bp = self.byte_pos();
                    tokens.push(Token::new(TokenKind::Comma, Span::new(bp, bp + 1)));
                    self.pos += 1;
                }
                '?' => {
                    let bp = self.byte_pos();
                    tokens.push(Token::new(TokenKind::Question, Span::new(bp, bp + 1)));
                    self.pos += 1;
                }
                '@' => {
                    tokens.push(self.read_role());
                }
                '$' => {
                    tokens.push(self.read_secret_ref()?);
                }
                c if c.is_ascii_digit() => {
                    tokens.push(self.read_number()?);
                }
                c if c.is_alphabetic() || c == '_' => {
                    tokens.push(self.read_identifier_or_keyword());
                }
                _ => {
                    let bp = self.byte_pos();
                    return Err(NmlError::lex(
                        format!("unexpected character: '{ch}'"),
                        Span::new(bp, bp + 1),
                    ));
                }
            }
        }

        Ok(tokens)
    }

    fn peek_char(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    fn handle_indentation(&mut self, tokens: &mut Vec<Token>) -> NmlResult<()> {
        let mut indent = 0;
        while self.pos < self.chars.len() && self.chars[self.pos] == ' ' {
            indent += 1;
            self.pos += 1;
        }

        // Per spec, tabs are not permitted in indentation. Without this
        // check a leading tab would be silently consumed as inter-token
        // whitespace and the line treated as indent level `indent`,
        // misinterpreting the document's structure.
        if self.pos < self.chars.len() && self.chars[self.pos] == '\t' {
            let bp = self.byte_pos();
            return Err(NmlError::lex(
                "tabs are not permitted in indentation; use spaces",
                Span::new(bp, bp + 1),
            ));
        }

        if self.pos >= self.chars.len()
            || self.chars[self.pos] == '\n'
            || (self.chars[self.pos] == '/' && self.peek_char() == Some('/'))
        {
            if self.pos < self.chars.len() && self.chars[self.pos] != '\n' {
                self.consume_comment(true);
            }
            return Ok(());
        }

        let current_indent = *self.indent_stack.last().unwrap();

        if indent > current_indent {
            self.indent_stack.push(indent);
            tokens.push(Token::new(
                TokenKind::Indent,
                Span::new(self.byte_pos_at(self.pos - indent), self.byte_pos()),
            ));
        } else if indent < current_indent {
            while self.indent_stack.len() > 1 && *self.indent_stack.last().unwrap() > indent {
                self.indent_stack.pop();
                tokens.push(Token::new(
                    TokenKind::Dedent,
                    Span::new(self.byte_pos_at(self.pos - indent), self.byte_pos()),
                ));
            }
            if *self.indent_stack.last().unwrap() != indent {
                return Err(NmlError::lex(
                    "inconsistent indentation",
                    Span::new(self.byte_pos_at(self.pos - indent), self.byte_pos()),
                ));
            }
        }

        Ok(())
    }

    /// Consume a `//` comment through end of line, recording it for the
    /// comment side channel. `self.pos` must be at the first `/`.
    fn consume_comment(&mut self, own_line: bool) {
        let start = self.byte_pos();
        while self.pos < self.chars.len() && self.chars[self.pos] != '\n' {
            self.pos += 1;
        }
        let end = self.byte_pos();
        let raw = &self.source[start..end];
        let text = raw.strip_prefix("//").unwrap_or(raw).trim_end();
        self.comments.push(Comment {
            text: text.to_string(),
            span: Span::new(start, end),
            own_line,
        });
    }

    fn read_string(&mut self) -> NmlResult<Token> {
        let start = self.byte_pos();
        self.pos += 1;
        let mut value = String::new();

        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            match ch {
                '"' => {
                    self.pos += 1;
                    return Ok(Token::new(
                        TokenKind::StringLiteral(value),
                        Span::new(start, self.byte_pos()),
                    ));
                }
                '\\' => {
                    self.pos += 1;
                    if self.pos >= self.chars.len() {
                        return Err(NmlError::lex(
                            "unexpected end of string",
                            Span::new(start, self.byte_pos()),
                        ));
                    }
                    match self.chars[self.pos] {
                        '"' => value.push('"'),
                        '\\' => value.push('\\'),
                        'n' => value.push('\n'),
                        't' => value.push('\t'),
                        c => {
                            return Err(NmlError::lex(
                                format!("unknown escape sequence: '\\{c}'"),
                                Span::new(
                                    self.byte_pos_at(self.pos - 1),
                                    self.byte_pos_at(self.pos + 1),
                                ),
                            ));
                        }
                    }
                    self.pos += 1;
                }
                '\n' => {
                    return Err(NmlError::lex(
                        "unterminated string literal",
                        Span::new(start, self.byte_pos()),
                    ));
                }
                _ => {
                    value.push(ch);
                    self.pos += 1;
                }
            }
        }

        Err(NmlError::lex(
            "unterminated string literal",
            Span::new(start, self.byte_pos()),
        ))
    }

    fn read_multiline_string(&mut self) -> NmlResult<Token> {
        let start = self.byte_pos();
        self.pos += 3; // skip """
        let mut raw = String::new();

        while self.pos < self.chars.len() {
            if self.chars[self.pos] == '"'
                && self.chars.get(self.pos + 1) == Some(&'"')
                && self.chars.get(self.pos + 2) == Some(&'"')
            {
                self.pos += 3; // skip closing """
                let value = Self::dedent_multiline_string(&raw);
                return Ok(Token::new(
                    TokenKind::StringLiteral(value),
                    Span::new(start, self.byte_pos()),
                ));
            }
            match self.chars[self.pos] {
                '\\' => {
                    self.pos += 1;
                    if self.pos >= self.chars.len() {
                        return Err(NmlError::lex(
                            "unexpected end of string",
                            Span::new(start, self.byte_pos()),
                        ));
                    }
                    match self.chars[self.pos] {
                        '"' => raw.push('"'),
                        '\\' => raw.push('\\'),
                        'n' => raw.push('\n'),
                        't' => raw.push('\t'),
                        c => {
                            return Err(NmlError::lex(
                                format!("unknown escape sequence: '\\{c}'"),
                                Span::new(
                                    self.byte_pos_at(self.pos - 1),
                                    self.byte_pos_at(self.pos + 1),
                                ),
                            ));
                        }
                    }
                    self.pos += 1;
                }
                ch => {
                    raw.push(ch);
                    self.pos += 1;
                }
            }
        }

        Err(NmlError::lex(
            "unterminated multiline string literal",
            Span::new(start, self.byte_pos()),
        ))
    }

    /// Dedent multiline string content: strip minimum leading whitespace from each line,
    /// trim leading newline after opening \"\"\" and trailing newline before closing \"\"\".
    fn dedent_multiline_string(raw: &str) -> String {
        let mut lines: Vec<&str> = raw.split('\n').collect();

        // Trim leading newline: if first line is empty, remove it
        if let Some(first) = lines.first() {
            if first.is_empty() || first.chars().all(|c| c.is_whitespace()) {
                lines.remove(0);
            }
        }

        // Trim trailing newline: if last line is empty, remove it
        if let Some(last) = lines.last() {
            if last.is_empty() || last.chars().all(|c| c.is_whitespace()) {
                lines.pop();
            }
        }

        if lines.is_empty() {
            return String::new();
        }

        let min_indent = lines
            .iter()
            .filter(|line| !line.chars().all(|c| c.is_whitespace()))
            .map(|line| line.chars().take_while(|c| *c == ' ').count())
            .min()
            .unwrap_or(0);

        lines
            .iter()
            .map(|line| {
                if line.len() >= min_indent && line.chars().take(min_indent).all(|c| c == ' ') {
                    &line[min_indent..]
                } else {
                    *line
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn read_number(&mut self) -> NmlResult<Token> {
        let start = self.byte_pos();
        let mut s = String::new();

        if self.chars[self.pos] == '-' {
            s.push('-');
            self.pos += 1;
        }

        while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_digit() {
            s.push(self.chars[self.pos]);
            self.pos += 1;
        }

        let mut is_float = false;
        if self.pos < self.chars.len() && self.chars[self.pos] == '.' {
            is_float = true;
            s.push('.');
            self.pos += 1;
            while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_digit() {
                s.push(self.chars[self.pos]);
                self.pos += 1;
            }
        }

        let span = Span::new(start, self.byte_pos());
        // Literals without a decimal point are exact 64-bit integers; a
        // round-trip through f64 would silently corrupt values above 2^53.
        // Integers that do not fit in i64 are an explicit error rather
        // than a silently rounded float.
        let value = if is_float {
            crate::types::Number::Float(
                s.parse()
                    .map_err(|_| NmlError::lex(format!("invalid number: \"{s}\""), span))?,
            )
        } else {
            crate::types::Number::Int(s.parse().map_err(|_| {
                NmlError::lex(
                    format!("integer \"{s}\" out of range for 64-bit integer"),
                    span,
                )
            })?)
        };

        Ok(Token::new(TokenKind::NumberLiteral(value), span))
    }

    fn read_role(&mut self) -> Token {
        let start = self.byte_pos();
        self.pos += 1; // skip @
        let mut value = String::from("@");

        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            if ch.is_alphanumeric()
                || ch == '/'
                || ch == ':'
                || ch == '-'
                || ch == '_'
                || ch == '.'
                || ch == '@'
                || ch == '{'
                || ch == '}'
                || ch == '+'
            {
                value.push(ch);
                self.pos += 1;
            } else {
                break;
            }
        }

        if value.ends_with(':') {
            let next = self.chars.get(self.pos).copied();
            if next.is_none() || next == Some('\n') || next == Some(' ') || next == Some('\t') {
                value.pop();
                self.pos -= 1;
            }
        }

        Token::new(TokenKind::Role(value), Span::new(start, self.byte_pos()))
    }

    const KNOWN_NAMESPACES: &'static [&'static str] = &["ENV"];

    fn read_secret_ref(&mut self) -> NmlResult<Token> {
        let start = self.byte_pos();
        self.pos += 1; // skip $

        let ns_start = self.pos;
        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            if ch.is_alphanumeric() || ch == '_' {
                self.pos += 1;
            } else {
                break;
            }
        }

        if self.pos == ns_start {
            return Err(NmlError::lex(
                "expected namespace after '$' (e.g. $ENV.MY_VAR)",
                Span::new(start, self.byte_pos()),
            ));
        }

        let namespace: String = self.chars[ns_start..self.pos].iter().collect();

        if !Self::KNOWN_NAMESPACES.contains(&namespace.as_str()) {
            let valid = Self::KNOWN_NAMESPACES.join(", ");
            return Err(NmlError::lex(
                format!(
                    "unknown variable source '{}'. Valid sources: {}",
                    namespace, valid
                ),
                Span::new(start, self.byte_pos()),
            ));
        }

        if self.pos >= self.chars.len() || self.chars[self.pos] != '.' {
            return Err(NmlError::lex(
                format!("expected '.' after ${}", namespace),
                Span::new(start, self.byte_pos()),
            ));
        }
        self.pos += 1; // skip '.'

        let key_start = self.pos;
        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            if ch.is_alphanumeric() || ch == '_' || ch == '.' {
                self.pos += 1;
            } else {
                break;
            }
        }

        if self.pos == key_start {
            return Err(NmlError::lex(
                format!("expected variable name after ${namespace}."),
                Span::new(start, self.byte_pos()),
            ));
        }

        let full = self.source[start..self.byte_pos()].to_string();
        Ok(Token::new(
            TokenKind::SecretRef(full),
            Span::new(start, self.byte_pos()),
        ))
    }

    fn read_identifier_or_keyword(&mut self) -> Token {
        let start = self.byte_pos();
        let mut value = String::new();

        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            if ch.is_alphanumeric() || ch == '_' || ch == '-' {
                value.push(ch);
                self.pos += 1;
            } else {
                break;
            }
        }

        let kind = match value.as_str() {
            "true" => TokenKind::BoolLiteral(true),
            "false" => TokenKind::BoolLiteral(false),
            _ => {
                if value.len() == 3 && value.chars().all(|c| c.is_ascii_uppercase()) {
                    TokenKind::CurrencyCode(value)
                } else {
                    TokenKind::Identifier(value)
                }
            }
        };

        Token::new(kind, Span::new(start, self.byte_pos()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(input: &str) -> Vec<TokenKind> {
        let mut lexer = Lexer::new(input);
        lexer
            .tokenize()
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| !matches!(k, TokenKind::Newline))
            .collect()
    }

    #[test]
    fn comments_captured_with_placement() {
        let source =
            "// header\nservice App: // trailing\n    // indented\n    port = 8080 // why\n";
        let mut lexer = Lexer::new(source);
        lexer.tokenize().unwrap();
        let comments = lexer.take_comments();

        assert_eq!(comments.len(), 4);
        assert_eq!(comments[0].text, " header");
        assert!(comments[0].own_line);
        assert_eq!(comments[1].text, " trailing");
        assert!(!comments[1].own_line);
        assert_eq!(comments[2].text, " indented");
        assert!(comments[2].own_line);
        assert_eq!(comments[3].text, " why");
        assert!(!comments[3].own_line);
        // Spans must cover the full `// ...` text.
        assert_eq!(
            &source[comments[0].span.start..comments[0].span.end],
            "// header"
        );
    }

    #[test]
    fn comment_like_text_inside_string_not_captured() {
        let mut lexer = Lexer::new("url = \"https://example.com\"\n");
        lexer.tokenize().unwrap();
        assert!(lexer.take_comments().is_empty());
    }

    #[test]
    fn divider_comment_text_verbatim() {
        let mut lexer = Lexer::new("//// section ////\nx = 1\n");
        lexer.tokenize().unwrap();
        let comments = lexer.take_comments();
        assert_eq!(comments[0].text, "// section ////");
    }

    #[test]
    fn tab_indentation_rejected() {
        let mut lexer = Lexer::new("service App:\n\tport = 8080\n");
        let err = lexer.tokenize().unwrap_err();
        assert!(
            err.message().contains("tabs are not permitted"),
            "expected tab error, got: {}",
            err.message()
        );
    }

    #[test]
    fn tab_after_spaces_in_indentation_rejected() {
        let mut lexer = Lexer::new("service App:\n  \tport = 8080\n");
        assert!(lexer.tokenize().is_err());
    }

    #[test]
    fn tab_inside_string_literal_ok() {
        let kinds = lex("name = \"a\tb\"");
        assert!(kinds
            .iter()
            .any(|k| matches!(k, TokenKind::StringLiteral(s) if s == "a\tb")));
    }

    #[test]
    fn test_simple_property() {
        let tokens = lex("name = \"hello\"");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("name".into()),
                TokenKind::Equals,
                TokenKind::StringLiteral("hello".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_block_declaration() {
        let tokens = lex("service MyService:\n    name = \"test\"");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("service".into()),
                TokenKind::Identifier("MyService".into()),
                TokenKind::Colon,
                TokenKind::Indent,
                TokenKind::Identifier("name".into()),
                TokenKind::Equals,
                TokenKind::StringLiteral("test".into()),
                TokenKind::Dedent,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_array_prefix() {
        let tokens = lex("[]resource items:");
        assert_eq!(
            tokens,
            vec![
                TokenKind::ArrayPrefix,
                TokenKind::Identifier("resource".into()),
                TokenKind::Identifier("items".into()),
                TokenKind::Colon,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_role_ref() {
        let tokens = lex("|allow = [@role/admin]");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Pipe,
                TokenKind::Identifier("allow".into()),
                TokenKind::Equals,
                TokenKind::BracketOpen,
                TokenKind::Role("@role/admin".into()),
                TokenKind::BracketClose,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_integer_literal_exact_above_2_pow_53() {
        let tokens = lex("id = 9007199254740993");
        assert!(
            tokens.contains(&TokenKind::NumberLiteral(crate::types::Number::Int(
                9_007_199_254_740_993
            )))
        );
    }

    #[test]
    fn test_integer_literal_i64_bounds() {
        let tokens = lex("max = 9223372036854775807");
        assert!(
            tokens.contains(&TokenKind::NumberLiteral(crate::types::Number::Int(
                i64::MAX
            )))
        );
        let tokens = lex("min = -9223372036854775808");
        assert!(
            tokens.contains(&TokenKind::NumberLiteral(crate::types::Number::Int(
                i64::MIN
            )))
        );
    }

    #[test]
    fn test_integer_literal_overflow_rejected() {
        let mut lexer = Lexer::new("big = 9223372036854775808");
        let err = lexer.tokenize().unwrap_err();
        assert!(
            err.to_string().contains("out of range"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_decimal_literal_lexes_as_float() {
        let tokens = lex("rate = 0.75");
        assert!(tokens.contains(&TokenKind::NumberLiteral(crate::types::Number::Float(0.75))));
    }

    #[test]
    fn test_money_literal() {
        let tokens = lex("price = 19.99 USD");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("price".into()),
                TokenKind::Equals,
                TokenKind::NumberLiteral(crate::types::Number::Float(19.99)),
                TokenKind::CurrencyCode("USD".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_secret_ref() {
        let tokens = lex("token = $ENV.MY_SECRET");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("token".into()),
                TokenKind::Equals,
                TokenKind::SecretRef("$ENV.MY_SECRET".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_empty_array() {
        let tokens = lex("|deny = []");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Pipe,
                TokenKind::Identifier("deny".into()),
                TokenKind::Equals,
                TokenKind::BracketOpen,
                TokenKind::BracketClose,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_comment_skipped() {
        let tokens = lex("// this is a comment\nname = \"test\"");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("name".into()),
                TokenKind::Equals,
                TokenKind::StringLiteral("test".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_dash_list_item() {
        let tokens = lex("- MyItem:");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Dash,
                TokenKind::Identifier("MyItem".into()),
                TokenKind::Colon,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_bool_literals() {
        let tokens = lex("enabled = true");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("enabled".into()),
                TokenKind::Equals,
                TokenKind::BoolLiteral(true),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_pipe_modifier() {
        let tokens = lex("|allow:\n    - @public");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Pipe,
                TokenKind::Identifier("allow".into()),
                TokenKind::Colon,
                TokenKind::Indent,
                TokenKind::Dash,
                TokenKind::Role("@public".into()),
                TokenKind::Dedent,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_dot_shared_property() {
        let tokens = lex(".healthCheck:\n    path = \"/health\"");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Dot,
                TokenKind::Identifier("healthCheck".into()),
                TokenKind::Colon,
                TokenKind::Indent,
                TokenKind::Identifier("path".into()),
                TokenKind::Equals,
                TokenKind::StringLiteral("/health".into()),
                TokenKind::Dedent,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_dot_shared_property_scalar() {
        let tokens = lex(".revalidationInterval = 7200");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Dot,
                TokenKind::Identifier("revalidationInterval".into()),
                TokenKind::Equals,
                TokenKind::NumberLiteral(crate::types::Number::Int(7200)),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_multiline_string() {
        let input = "x = \"\"\"\n    hello\n    world\n    \"\"\"";
        let mut lexer = Lexer::new(input);
        let tokens = lexer.tokenize().unwrap();
        let kinds: Vec<_> = tokens
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| !matches!(k, TokenKind::Newline))
            .collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Identifier("x".into()),
                TokenKind::Equals,
                TokenKind::StringLiteral("hello\nworld".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_multiline_string_inline() {
        let input = "x = \"\"\"one line\"\"\"";
        let mut lexer = Lexer::new(input);
        let tokens = lexer.tokenize().unwrap();
        let kinds: Vec<_> = tokens
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| !matches!(k, TokenKind::Newline))
            .collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Identifier("x".into()),
                TokenKind::Equals,
                TokenKind::StringLiteral("one line".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_secret_ref_with_dotted_key() {
        let tokens = lex("x = $ENV.MY.DOTTED.VAR");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("x".into()),
                TokenKind::Equals,
                TokenKind::SecretRef("$ENV.MY.DOTTED.VAR".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_secret_ref_unknown_namespace() {
        let mut lexer = Lexer::new("x = $FOOBAR.KEY");
        let result = lexer.tokenize();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message().contains("unknown variable source 'FOOBAR'"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_secret_ref_empty_key() {
        let mut lexer = Lexer::new("x = $ENV.");
        let result = lexer.tokenize();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message().contains("expected variable name after $ENV."),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_secret_ref_dollar_alone() {
        let mut lexer = Lexer::new("x = $ ");
        let result = lexer.tokenize();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message().contains("expected namespace after '$'"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_secret_ref_no_dot_after_namespace() {
        let mut lexer = Lexer::new("x = $ENV ");
        let result = lexer.tokenize();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message().contains("expected '.' after $ENV"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_secret_ref_with_fallback_tokens() {
        let tokens = lex("port = $ENV.PORT | 3000");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("port".into()),
                TokenKind::Equals,
                TokenKind::SecretRef("$ENV.PORT".into()),
                TokenKind::Pipe,
                TokenKind::NumberLiteral(crate::types::Number::Int(3000)),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_spans_use_byte_offsets_with_multibyte_chars() {
        // "─" (U+2500) is 3 bytes in UTF-8
        let source = "// ─\nname = \"test\"";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().unwrap();

        let name_tok = tokens
            .iter()
            .find(|t| matches!(&t.kind, TokenKind::Identifier(s) if s == "name"))
            .expect("should find 'name' identifier");

        // "// ─\n" = 2(//) + 1( ) + 3(─) + 1(\n) = 7 bytes, so 'name' starts at byte 7
        assert_eq!(name_tok.span.start, 7);
        assert_eq!(name_tok.span.end, 11);

        // Verify via SourceMap that it resolves to line 2, column 1
        let source_map = crate::span::SourceMap::new(source);
        let loc = source_map.location(name_tok.span.start);
        assert_eq!(loc.line, 2);
        assert_eq!(loc.column, 1);
    }

    #[test]
    fn test_lex_parentheses() {
        let source = "(hello | world)";
        let tokens = Lexer::new(source).tokenize().unwrap();
        let kinds: Vec<_> = tokens.iter().map(|t| &t.kind).collect();
        assert!(matches!(kinds[0], TokenKind::ParenOpen));
        assert!(matches!(kinds[1], TokenKind::Identifier(s) if s == "hello"));
        assert!(matches!(kinds[2], TokenKind::Pipe));
        assert!(matches!(kinds[3], TokenKind::Identifier(s) if s == "world"));
        assert!(matches!(kinds[4], TokenKind::ParenClose));
    }

    #[test]
    fn test_role_user_email() {
        let tokens = lex("@user/test@example.com");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Role("@user/test@example.com".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_role_user_email_in_context() {
        let tokens = lex("member = @user/test@example.com");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("member".into()),
                TokenKind::Equals,
                TokenKind::Role("@user/test@example.com".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_role_user_email_with_plus() {
        let tokens = lex("@user/test+tag@example.com");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Role("@user/test+tag@example.com".into()),
                TokenKind::Eof,
            ]
        );
    }
}
