use crate::error::{NmlError, NmlResult};
use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Structure
    Indent,
    Dedent,
    Newline,
    Eof,

    // Literals
    StringLiteral(String),
    NumberLiteral(f64),
    BoolLiteral(bool),
    Identifier(String),
    RoleRef(String),       // @role/admin, @public, etc.
    SecretRef(String),     // $ENV.MY_SECRET
    CurrencyCode(String),  // USD, GBP, etc.

    // Punctuation
    Colon,          // :
    Equals,         // =
    Dash,           // -
    Pipe,           // |
    Dot,            // .
    BracketOpen,    // [
    BracketClose,   // ]
    ArrayPrefix,    // []
    Comma,          // ,
    Question,       // ?
}

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

pub struct Lexer<'a> {
    source: &'a str,
    chars: Vec<char>,
    byte_offsets: Vec<usize>,
    pos: usize,
    indent_stack: Vec<usize>,
    pending_tokens: Vec<Token>,
    at_line_start: bool,
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
            pending_tokens: Vec::new(),
            at_line_start: true,
        }
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
            if let Some(tok) = self.pending_tokens.pop() {
                tokens.push(tok);
                continue;
            }

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
                    self.skip_comment();
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
                    if self.peek_char().map_or(false, |c| c.is_ascii_digit()) {
                        let prev_meaningful = tokens.iter().rev().find(|t| {
                            !matches!(t.kind, TokenKind::Newline | TokenKind::Indent)
                        });
                        if prev_meaningful.map_or(false, |t| {
                            matches!(t.kind, TokenKind::Equals | TokenKind::BracketOpen | TokenKind::Comma)
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
                        if after_bracket.map_or(false, |c| c.is_alphabetic() || c == '_') {
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
                    tokens.push(self.read_role_ref());
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

        if self.pos >= self.chars.len()
            || self.chars[self.pos] == '\n'
            || (self.chars[self.pos] == '/' && self.peek_char() == Some('/'))
        {
            if self.pos < self.chars.len() && self.chars[self.pos] != '\n' {
                self.skip_comment();
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

    fn skip_comment(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos] != '\n' {
            self.pos += 1;
        }
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

        if self.pos < self.chars.len() && self.chars[self.pos] == '.' {
            s.push('.');
            self.pos += 1;
            while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_digit() {
                s.push(self.chars[self.pos]);
                self.pos += 1;
            }
        }

        let value: f64 = s.parse().map_err(|_| {
            NmlError::lex(
                format!("invalid number: \"{s}\""),
                Span::new(start, self.byte_pos()),
            )
        })?;

        Ok(Token::new(
            TokenKind::NumberLiteral(value),
            Span::new(start, self.byte_pos()),
        ))
    }

    fn read_role_ref(&mut self) -> Token {
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
            let next = self.chars.get(self.pos).map(|&c| c);
            if next.is_none() || next == Some('\n') || next == Some(' ') || next == Some('\t') {
                value.pop();
                self.pos -= 1;
            }
        }

        Token::new(TokenKind::RoleRef(value), Span::new(start, self.byte_pos()))
    }

    fn read_secret_ref(&mut self) -> NmlResult<Token> {
        let start = self.byte_pos();
        self.pos += 1; // skip $

        let remaining: String = self.chars[self.pos..].iter().take(4).collect();
        if remaining != "ENV." {
            return Err(NmlError::lex(
                "expected $ENV. for secret reference",
                Span::new(start, self.byte_pos_at(self.pos + 4)),
            ));
        }
        self.pos += 4;

        let name_start = self.pos;
        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            if ch.is_alphanumeric() || ch == '_' {
                self.pos += 1;
            } else {
                break;
            }
        }

        if self.pos == name_start {
            return Err(NmlError::lex(
                "expected variable name after $ENV.",
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
                TokenKind::RoleRef("@role/admin".into()),
                TokenKind::BracketClose,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn test_money_literal() {
        let tokens = lex("price = 19.99 USD");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("price".into()),
                TokenKind::Equals,
                TokenKind::NumberLiteral(19.99),
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
                TokenKind::RoleRef("@public".into()),
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
}
