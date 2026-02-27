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
    _source: &'a str,
    chars: Vec<char>,
    pos: usize,
    indent_stack: Vec<usize>,
    pending_tokens: Vec<Token>,
    at_line_start: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            _source: source,
            chars: source.chars().collect(),
            pos: 0,
            indent_stack: vec![0],
            pending_tokens: Vec::new(),
            at_line_start: true,
        }
    }

    pub fn tokenize(&mut self) -> NmlResult<Vec<Token>> {
        let mut tokens = Vec::new();

        loop {
            if let Some(tok) = self.pending_tokens.pop() {
                tokens.push(tok);
                continue;
            }

            if self.pos >= self.chars.len() {
                // Emit remaining DEDENTs
                while self.indent_stack.len() > 1 {
                    self.indent_stack.pop();
                    tokens.push(Token::new(TokenKind::Dedent, Span::empty(self.pos)));
                }
                tokens.push(Token::new(TokenKind::Eof, Span::empty(self.pos)));
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
                    tokens.push(Token::new(TokenKind::Newline, Span::new(self.pos, self.pos + 1)));
                    self.pos += 1;
                    self.at_line_start = true;
                }
                '/' if self.peek_char() == Some('/') => {
                    self.skip_comment();
                }
                '"' => {
                    tokens.push(self.read_string()?);
                }
                ':' => {
                    tokens.push(Token::new(TokenKind::Colon, Span::new(self.pos, self.pos + 1)));
                    self.pos += 1;
                }
                '=' => {
                    tokens.push(Token::new(TokenKind::Equals, Span::new(self.pos, self.pos + 1)));
                    self.pos += 1;
                }
                '-' => {
                    // Could be a negative number or a dash (list item)
                    if self.peek_char().map_or(false, |c| c.is_ascii_digit()) {
                        // Check if preceded by '=' or '[' or ',' to determine if negative number
                        let prev_meaningful = tokens.iter().rev().find(|t| {
                            !matches!(t.kind, TokenKind::Newline | TokenKind::Indent)
                        });
                        if prev_meaningful.map_or(false, |t| {
                            matches!(t.kind, TokenKind::Equals | TokenKind::BracketOpen | TokenKind::Comma)
                        }) {
                            tokens.push(self.read_number()?);
                        } else {
                            tokens.push(Token::new(TokenKind::Dash, Span::new(self.pos, self.pos + 1)));
                            self.pos += 1;
                        }
                    } else {
                        tokens.push(Token::new(TokenKind::Dash, Span::new(self.pos, self.pos + 1)));
                        self.pos += 1;
                    }
                }
                '|' => {
                    tokens.push(Token::new(TokenKind::Pipe, Span::new(self.pos, self.pos + 1)));
                    self.pos += 1;
                }
                '.' => {
                    tokens.push(Token::new(TokenKind::Dot, Span::new(self.pos, self.pos + 1)));
                    self.pos += 1;
                }
                '[' => {
                    if self.peek_char() == Some(']') {
                        let span = Span::new(self.pos, self.pos + 2);
                        // Check if next char after ] is a letter (array type prefix) or not (empty array)
                        let after_bracket = if self.pos + 2 < self.chars.len() {
                            Some(self.chars[self.pos + 2])
                        } else {
                            None
                        };
                        if after_bracket.map_or(false, |c| c.is_alphabetic() || c == '_') {
                            tokens.push(Token::new(TokenKind::ArrayPrefix, span));
                        } else {
                            tokens.push(Token::new(TokenKind::BracketOpen, Span::new(self.pos, self.pos + 1)));
                            self.pos += 1;
                            tokens.push(Token::new(TokenKind::BracketClose, Span::new(self.pos, self.pos + 1)));
                            self.pos += 1;
                            continue;
                        }
                        self.pos += 2;
                    } else {
                        tokens.push(Token::new(TokenKind::BracketOpen, Span::new(self.pos, self.pos + 1)));
                        self.pos += 1;
                    }
                }
                ']' => {
                    tokens.push(Token::new(TokenKind::BracketClose, Span::new(self.pos, self.pos + 1)));
                    self.pos += 1;
                }
                ',' => {
                    tokens.push(Token::new(TokenKind::Comma, Span::new(self.pos, self.pos + 1)));
                    self.pos += 1;
                }
                '?' => {
                    tokens.push(Token::new(TokenKind::Question, Span::new(self.pos, self.pos + 1)));
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
                    return Err(NmlError::lex(
                        format!("unexpected character: '{ch}'"),
                        Span::new(self.pos, self.pos + 1),
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

        // Skip blank lines and comment-only lines
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
            tokens.push(Token::new(TokenKind::Indent, Span::new(self.pos - indent, self.pos)));
        } else if indent < current_indent {
            while self.indent_stack.len() > 1 && *self.indent_stack.last().unwrap() > indent {
                self.indent_stack.pop();
                tokens.push(Token::new(TokenKind::Dedent, Span::new(self.pos - indent, self.pos)));
            }
            if *self.indent_stack.last().unwrap() != indent {
                return Err(NmlError::lex(
                    "inconsistent indentation",
                    Span::new(self.pos - indent, self.pos),
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
        let start = self.pos;
        self.pos += 1; // skip opening quote
        let mut value = String::new();

        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            match ch {
                '"' => {
                    self.pos += 1;
                    return Ok(Token::new(
                        TokenKind::StringLiteral(value),
                        Span::new(start, self.pos),
                    ));
                }
                '\\' => {
                    self.pos += 1;
                    if self.pos >= self.chars.len() {
                        return Err(NmlError::lex(
                            "unexpected end of string",
                            Span::new(start, self.pos),
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
                                Span::new(self.pos - 1, self.pos + 1),
                            ));
                        }
                    }
                    self.pos += 1;
                }
                '\n' => {
                    return Err(NmlError::lex(
                        "unterminated string literal",
                        Span::new(start, self.pos),
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
            Span::new(start, self.pos),
        ))
    }

    fn read_number(&mut self) -> NmlResult<Token> {
        let start = self.pos;
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
            NmlError::lex(format!("invalid number: \"{s}\""), Span::new(start, self.pos))
        })?;

        Ok(Token::new(
            TokenKind::NumberLiteral(value),
            Span::new(start, self.pos),
        ))
    }

    fn read_role_ref(&mut self) -> Token {
        let start = self.pos;
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

        // If the role ref ends with ':', check if it's actually a declaration colon
        // (followed by newline/whitespace/EOF) rather than an internal separator like @nudge:research
        if value.ends_with(':') {
            let next = self.chars.get(self.pos);
            if next.is_none() || next == Some(&'\n') || next == Some(&' ') || next == Some(&'\t') {
                value.pop();
                self.pos -= 1;
            }
        }

        Token::new(TokenKind::RoleRef(value), Span::new(start, self.pos))
    }

    fn read_secret_ref(&mut self) -> NmlResult<Token> {
        let start = self.pos;
        self.pos += 1; // skip $

        // Expect "ENV."
        let remaining: String = self.chars[self.pos..].iter().take(4).collect();
        if remaining != "ENV." {
            return Err(NmlError::lex(
                "expected $ENV. for secret reference",
                Span::new(start, self.pos + 4),
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
                Span::new(start, self.pos),
            ));
        }

        let full: String = self.chars[start..self.pos].iter().collect();
        Ok(Token::new(
            TokenKind::SecretRef(full),
            Span::new(start, self.pos),
        ))
    }

    fn read_identifier_or_keyword(&mut self) -> Token {
        let start = self.pos;
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
                // Check if this is a 3-letter uppercase currency code after a number
                if value.len() == 3 && value.chars().all(|c| c.is_ascii_uppercase()) {
                    TokenKind::CurrencyCode(value)
                } else {
                    TokenKind::Identifier(value)
                }
            }
        };

        Token::new(kind, Span::new(start, self.pos))
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
}
