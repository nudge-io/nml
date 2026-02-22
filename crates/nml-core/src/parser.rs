use crate::ast::*;
use crate::error::{NmlError, NmlResult};
use crate::lexer::{Token, TokenKind};
use crate::money;
use crate::span::Span;
use crate::types::{SpannedValue, Value};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    pub fn parse(&mut self) -> NmlResult<File> {
        let mut declarations = Vec::new();

        while !self.at_eof() {
            self.skip_newlines();
            if self.at_eof() {
                break;
            }
            declarations.push(self.parse_declaration()?);
        }

        Ok(File { declarations })
    }

    fn parse_declaration(&mut self) -> NmlResult<Declaration> {
        let start_span = self.current_span();

        // Check for []keyword (array declaration)
        if self.check(TokenKind::ArrayPrefix) {
            return self.parse_array_declaration(start_span);
        }

        self.parse_block_declaration(start_span)
    }

    fn parse_array_declaration(&mut self, start_span: Span) -> NmlResult<Declaration> {
        self.expect_kind(TokenKind::ArrayPrefix)?;
        let item_keyword = self.expect_identifier()?;
        let name = self.expect_identifier()?;
        self.expect_kind(TokenKind::Colon)?;
        self.skip_newlines();
        let body = self.parse_array_body()?;

        let end_span = self.prev_span();
        Ok(Declaration {
            kind: DeclarationKind::Array(ArrayDecl {
                item_keyword,
                name,
                body,
            }),
            span: start_span.merge(end_span),
        })
    }

    fn parse_block_declaration(&mut self, start_span: Span) -> NmlResult<Declaration> {
        let keyword = self.expect_identifier()?;
        let name = self.parse_declaration_name()?;
        self.expect_kind(TokenKind::Colon)?;
        self.skip_newlines();
        let body = self.parse_body()?;

        let end_span = self.prev_span();
        Ok(Declaration {
            kind: DeclarationKind::Block(BlockDecl {
                keyword,
                name,
                body,
            }),
            span: start_span.merge(end_span),
        })
    }

    /// Parse the name of a declaration, which may be a regular identifier or a role ref
    /// (for roleTemplate declarations like `roleTemplate @role/admin:`).
    fn parse_declaration_name(&mut self) -> NmlResult<Identifier> {
        if let Some(TokenKind::RoleRef(ref_str)) = self.peek_kind() {
            let span = self.current_span();
            let name = ref_str.clone();
            self.advance();
            Ok(Identifier::new(name, span))
        } else {
            self.expect_identifier()
        }
    }

    fn parse_body(&mut self) -> NmlResult<Body> {
        let mut entries = Vec::new();

        if !self.check(TokenKind::Indent) {
            return Ok(Body { entries });
        }
        self.advance(); // consume Indent

        while !self.check(TokenKind::Dedent) && !self.at_eof() {
            self.skip_newlines();
            if self.check(TokenKind::Dedent) || self.at_eof() {
                break;
            }

            let entry = self.parse_body_entry()?;
            entries.push(entry);
        }

        if self.check(TokenKind::Dedent) {
            self.advance();
        }

        Ok(Body { entries })
    }

    fn parse_body_entry(&mut self) -> NmlResult<BodyEntry> {
        let span = self.current_span();

        // Modifier: |name ...
        if self.check(TokenKind::Pipe) {
            let modifier = self.parse_modifier()?;
            let end = self.prev_span();
            return Ok(BodyEntry {
                kind: BodyEntryKind::Modifier(modifier),
                span: span.merge(end),
            });
        }

        // Shared property: .name: ...
        if self.check(TokenKind::Dot) {
            let shared = self.parse_shared_property()?;
            let end = self.prev_span();
            return Ok(BodyEntry {
                kind: BodyEntryKind::SharedProperty(shared),
                span: span.merge(end),
            });
        }

        // List item: - ...
        if self.check(TokenKind::Dash) {
            let item = self.parse_list_item()?;
            let end = self.prev_span();
            return Ok(BodyEntry {
                kind: BodyEntryKind::ListItem(item),
                span: span.merge(end),
            });
        }

        // Identifier: could be property (name = value) or nested block (name:)
        let ident = self.expect_identifier()?;

        if self.check(TokenKind::Equals) {
            let prop = self.parse_property_rest(ident)?;
            let end = self.prev_span();
            Ok(BodyEntry {
                kind: BodyEntryKind::Property(prop),
                span: span.merge(end),
            })
        } else if self.check(TokenKind::Colon) {
            let nested = self.parse_nested_block_rest(ident)?;
            let end = self.prev_span();
            Ok(BodyEntry {
                kind: BodyEntryKind::NestedBlock(nested),
                span: span.merge(end),
            })
        } else {
            Err(NmlError::parse(
                format!("expected '=' or ':' after '{}'", ident.name),
                span,
            ))
        }
    }

    fn parse_property_rest(&mut self, name: Identifier) -> NmlResult<Property> {
        self.expect_kind(TokenKind::Equals)?;
        let value = self.parse_value()?;
        self.skip_newlines();
        Ok(Property { name, value })
    }

    fn parse_nested_block_rest(&mut self, name: Identifier) -> NmlResult<NestedBlock> {
        self.expect_kind(TokenKind::Colon)?;
        self.skip_newlines();
        let body = self.parse_body()?;
        Ok(NestedBlock { name, body })
    }

    fn parse_modifier(&mut self) -> NmlResult<Modifier> {
        self.expect_kind(TokenKind::Pipe)?;
        let name = self.expect_identifier()?;

        if self.check(TokenKind::Equals) {
            self.advance();
            let value = self.parse_value()?;
            self.skip_newlines();
            Ok(Modifier {
                name,
                value: ModifierValue::Inline(value),
            })
        } else if self.check(TokenKind::Colon) {
            self.advance();
            self.skip_newlines();
            let items = self.parse_list_items()?;
            Ok(Modifier {
                name,
                value: ModifierValue::Block(items),
            })
        } else {
            Err(NmlError::parse(
                format!("expected '=' or ':' after modifier '|{}'", name.name),
                name.span,
            ))
        }
    }

    fn parse_shared_property(&mut self) -> NmlResult<SharedProperty> {
        self.expect_kind(TokenKind::Dot)?;
        let name = self.expect_identifier()?;
        self.expect_kind(TokenKind::Colon)?;
        self.skip_newlines();
        let body = self.parse_body()?;
        Ok(SharedProperty { name, body })
    }

    fn parse_array_body(&mut self) -> NmlResult<ArrayBody> {
        let mut modifiers = Vec::new();
        let mut shared_properties = Vec::new();
        let mut properties = Vec::new();
        let mut items = Vec::new();

        if !self.check(TokenKind::Indent) {
            return Ok(ArrayBody {
                modifiers,
                shared_properties,
                properties,
                items,
            });
        }
        self.advance(); // consume Indent

        while !self.check(TokenKind::Dedent) && !self.at_eof() {
            self.skip_newlines();
            if self.check(TokenKind::Dedent) || self.at_eof() {
                break;
            }

            if self.check(TokenKind::Pipe) {
                modifiers.push(self.parse_modifier()?);
            } else if self.check(TokenKind::Dot) {
                shared_properties.push(self.parse_shared_property()?);
            } else if self.check(TokenKind::Dash) {
                items.push(self.parse_list_item()?);
            } else if self.peek_kind_matches(|k| matches!(k, TokenKind::Identifier(_))) {
                let ident = self.expect_identifier()?;
                if self.check(TokenKind::Equals) {
                    let prop = self.parse_property_rest(ident)?;
                    properties.push(prop);
                } else {
                    return Err(NmlError::parse(
                        format!("unexpected '{}' in array body", ident.name),
                        ident.span,
                    ));
                }
            } else {
                return Err(NmlError::parse(
                    "unexpected token in array body",
                    self.current_span(),
                ));
            }
        }

        if self.check(TokenKind::Dedent) {
            self.advance();
        }

        Ok(ArrayBody {
            modifiers,
            shared_properties,
            properties,
            items,
        })
    }

    fn parse_list_items(&mut self) -> NmlResult<Vec<ListItem>> {
        let mut items = Vec::new();

        if !self.check(TokenKind::Indent) {
            return Ok(items);
        }
        self.advance();

        while !self.check(TokenKind::Dedent) && !self.at_eof() {
            self.skip_newlines();
            if self.check(TokenKind::Dedent) || self.at_eof() {
                break;
            }
            items.push(self.parse_list_item()?);
        }

        if self.check(TokenKind::Dedent) {
            self.advance();
        }

        Ok(items)
    }

    fn parse_list_item(&mut self) -> NmlResult<ListItem> {
        let dash_span = self.current_span();
        self.expect_kind(TokenKind::Dash)?;

        // - "string" (shorthand)
        if let Some(TokenKind::StringLiteral(_)) = self.peek_kind() {
            let val = self.parse_value()?;
            self.skip_newlines();
            let span = dash_span.merge(val.span);
            return Ok(ListItem {
                kind: ListItemKind::Shorthand(val),
                span,
            });
        }

        // - @role/ref
        if let Some(TokenKind::RoleRef(ref_str)) = self.peek_kind() {
            let role = ref_str.clone();
            let ref_span = self.current_span();
            self.advance();
            self.skip_newlines();
            let span = dash_span.merge(ref_span);
            return Ok(ListItem {
                kind: ListItemKind::RoleRef(role),
                span,
            });
        }

        // - Identifier or - Identifier: <body>
        let ident = self.expect_identifier()?;

        if self.check(TokenKind::Colon) {
            self.advance();
            self.skip_newlines();
            let body = self.parse_body()?;
            let span = dash_span.merge(self.prev_span());
            Ok(ListItem {
                kind: ListItemKind::Named {
                    name: ident,
                    body,
                },
                span,
            })
        } else {
            self.skip_newlines();
            let span = dash_span.merge(ident.span);
            Ok(ListItem {
                kind: ListItemKind::Reference(ident),
                span,
            })
        }
    }

    fn parse_value(&mut self) -> NmlResult<SpannedValue> {
        let span = self.current_span();

        match self.peek_kind() {
            Some(TokenKind::StringLiteral(s)) => {
                let s = s.clone();
                self.advance();
                Ok(SpannedValue::new(Value::String(s), span))
            }
            Some(TokenKind::NumberLiteral(n)) => {
                let n = *n;
                let num_span = span;
                self.advance();

                // Check for currency code (money literal)
                if let Some(TokenKind::CurrencyCode(code)) = self.peek_kind() {
                    let code = code.clone();
                    let end_span = self.current_span();
                    self.advance();
                    let full_span = num_span.merge(end_span);

                    // Use the raw string from source for precise decimal parsing
                    let src_slice = &self.source_slice(num_span);
                    let m = money::parse_money(src_slice, &code, full_span)?;
                    Ok(SpannedValue::new(Value::Money(m), full_span))
                } else {
                    Ok(SpannedValue::new(Value::Number(n), num_span))
                }
            }
            Some(TokenKind::BoolLiteral(b)) => {
                let b = *b;
                self.advance();
                Ok(SpannedValue::new(Value::Bool(b), span))
            }
            Some(TokenKind::SecretRef(s)) => {
                let s = s.clone();
                self.advance();
                Ok(SpannedValue::new(Value::Secret(s), span))
            }
            Some(TokenKind::RoleRef(r)) => {
                let r = r.clone();
                self.advance();
                Ok(SpannedValue::new(Value::RoleRef(r), span))
            }
            Some(TokenKind::BracketOpen) => {
                self.parse_array_literal()
            }
            Some(TokenKind::Identifier(name)) => {
                let name = name.clone();
                self.advance();
                Ok(SpannedValue::new(Value::Reference(name), span))
            }
            _ => Err(NmlError::parse("expected a value", span)),
        }
    }

    fn parse_array_literal(&mut self) -> NmlResult<SpannedValue> {
        let start = self.current_span();
        self.expect_kind(TokenKind::BracketOpen)?;

        let mut values = Vec::new();

        while !self.check(TokenKind::BracketClose) && !self.at_eof() {
            let val = self.parse_value()?;
            values.push(val);

            if self.check(TokenKind::Comma) {
                self.advance();
            }
        }

        let end = self.current_span();
        self.expect_kind(TokenKind::BracketClose)?;

        Ok(SpannedValue::new(
            Value::Array(values),
            start.merge(end),
        ))
    }

    // --- Token helpers ---

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.tokens.get(self.pos).map(|t| &t.kind)
    }

    fn peek_kind_matches(&self, f: impl FnOnce(&TokenKind) -> bool) -> bool {
        self.peek_kind().map_or(false, f)
    }

    fn check(&self, kind: TokenKind) -> bool {
        self.peek_kind()
            .map_or(false, |k| std::mem::discriminant(k) == std::mem::discriminant(&kind))
    }

    fn advance(&mut self) -> &Token {
        let tok = &self.tokens[self.pos];
        self.pos += 1;
        tok
    }

    fn at_eof(&self) -> bool {
        self.pos >= self.tokens.len()
            || matches!(self.tokens[self.pos].kind, TokenKind::Eof)
    }

    fn current_span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|t| t.span)
            .unwrap_or(Span::empty(0))
    }

    fn prev_span(&self) -> Span {
        if self.pos > 0 {
            self.tokens[self.pos - 1].span
        } else {
            Span::empty(0)
        }
    }

    fn skip_newlines(&mut self) {
        while self.peek_kind() == Some(&TokenKind::Newline) {
            self.advance();
        }
    }

    fn expect_kind(&mut self, expected: TokenKind) -> NmlResult<&Token> {
        if self.check(expected.clone()) {
            Ok(self.advance())
        } else {
            Err(NmlError::parse(
                format!("expected {:?}", expected),
                self.current_span(),
            ))
        }
    }

    fn expect_identifier(&mut self) -> NmlResult<Identifier> {
        match self.peek_kind() {
            Some(TokenKind::Identifier(name)) => {
                let name = name.clone();
                let span = self.current_span();
                self.advance();
                Ok(Identifier::new(name, span))
            }
            _ => Err(NmlError::parse(
                "expected an identifier",
                self.current_span(),
            )),
        }
    }

    fn source_slice(&self, span: Span) -> String {
        self.tokens
            .iter()
            .find(|t| t.span.start == span.start)
            .map(|t| match &t.kind {
                TokenKind::NumberLiteral(n) => {
                    // Reconstruct the original number string
                    if n.fract() == 0.0 && !format!("{n}").contains('.') {
                        format!("{}", *n as i64)
                    } else {
                        format!("{n}")
                    }
                }
                _ => String::new(),
            })
            .unwrap_or_default()
    }
}

/// Parse NML source text into an AST.
pub fn parse(source: &str) -> NmlResult<File> {
    let mut lexer = crate::lexer::Lexer::new(source);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens);
    parser.parse()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_block() {
        let source = "service MyService:\n    localMount = \"/\"\n";
        let file = parse(source).unwrap();
        assert_eq!(file.declarations.len(), 1);

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                assert_eq!(block.keyword.name, "service");
                assert_eq!(block.name.name, "MyService");
                assert_eq!(block.body.entries.len(), 1);
            }
            _ => panic!("expected block declaration"),
        }
    }

    #[test]
    fn test_parse_array_declaration() {
        let source = "[]resource items:\n    - Home:\n        path = \"/\"\n";
        let file = parse(source).unwrap();
        assert_eq!(file.declarations.len(), 1);

        match &file.declarations[0].kind {
            DeclarationKind::Array(arr) => {
                assert_eq!(arr.item_keyword.name, "resource");
                assert_eq!(arr.name.name, "items");
                assert_eq!(arr.body.items.len(), 1);
            }
            _ => panic!("expected array declaration"),
        }
    }

    #[test]
    fn test_parse_modifier_inline() {
        let source = "service Svc:\n    |allow = [@public]\n    localMount = \"/\"\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                assert!(block.body.entries.iter().any(|e| {
                    matches!(&e.kind, BodyEntryKind::Modifier(m) if m.name.name == "allow")
                }));
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_modifier_block() {
        let source = "service Svc:\n    |allow:\n        - @role/admin\n        - @public\n    localMount = \"/\"\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                let modifier = block.body.entries.iter().find_map(|e| match &e.kind {
                    BodyEntryKind::Modifier(m) if m.name.name == "allow" => Some(m),
                    _ => None,
                });
                assert!(modifier.is_some());
                match &modifier.unwrap().value {
                    ModifierValue::Block(items) => assert_eq!(items.len(), 2),
                    _ => panic!("expected block modifier"),
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_role_template() {
        let source = "roleTemplate @role/admin:\n    label = \"Admin\"\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                assert_eq!(block.keyword.name, "roleTemplate");
                assert_eq!(block.name.name, "@role/admin");
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_shorthand_list_item() {
        let source = "[]resource items:\n    - \"/test/path\"\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Array(arr) => {
                assert_eq!(arr.body.items.len(), 1);
                match &arr.body.items[0].kind {
                    ListItemKind::Shorthand(v) => {
                        assert_eq!(v.value, Value::String("/test/path".into()));
                    }
                    _ => panic!("expected shorthand item"),
                }
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn test_parse_reference_list_item() {
        let source = "[]resource items:\n    - AuthDevByPass\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Array(arr) => {
                assert_eq!(arr.body.items.len(), 1);
                match &arr.body.items[0].kind {
                    ListItemKind::Reference(ident) => {
                        assert_eq!(ident.name, "AuthDevByPass");
                    }
                    _ => panic!("expected reference item"),
                }
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn test_parse_money_property() {
        let source = "plan ProPlan:\n    monthlyPrice = 29.99 USD\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                assert_eq!(block.name.name, "ProPlan");
                match &block.body.entries[0].kind {
                    BodyEntryKind::Property(prop) => {
                        assert_eq!(prop.name.name, "monthlyPrice");
                        match &prop.value.value {
                            Value::Money(m) => {
                                assert_eq!(m.amount, 2999);
                                assert_eq!(m.currency, "USD");
                                assert_eq!(m.exponent, 2);
                            }
                            other => panic!("expected Money, got {other:?}"),
                        }
                    }
                    other => panic!("expected property, got {other:?}"),
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_money_zero_exponent() {
        let source = "plan JapanPlan:\n    price = 1299 JPY\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                match &block.body.entries[0].kind {
                    BodyEntryKind::Property(prop) => {
                        match &prop.value.value {
                            Value::Money(m) => {
                                assert_eq!(m.amount, 1299);
                                assert_eq!(m.currency, "JPY");
                                assert_eq!(m.exponent, 0);
                            }
                            other => panic!("expected Money, got {other:?}"),
                        }
                    }
                    other => panic!("expected property, got {other:?}"),
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_money_invalid_precision() {
        let source = "plan Bad:\n    price = 19.999 USD\n";
        let result = parse(source);
        assert!(result.is_err(), "too many decimal places should fail");
    }

    #[test]
    fn test_parse_secret_property() {
        let source = "provider Postmark:\n    serverToken = $ENV.POSTMARK_TOKEN\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                assert_eq!(block.name.name, "Postmark");
                match &block.body.entries[0].kind {
                    BodyEntryKind::Property(prop) => {
                        assert_eq!(prop.name.name, "serverToken");
                        match &prop.value.value {
                            Value::Secret(s) => {
                                assert_eq!(s, "$ENV.POSTMARK_TOKEN");
                            }
                            other => panic!("expected Secret, got {other:?}"),
                        }
                    }
                    other => panic!("expected property, got {other:?}"),
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_multiple_secrets() {
        let source = "provider Email:\n    apiKey = $ENV.API_KEY\n    apiSecret = $ENV.API_SECRET\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                assert_eq!(block.body.entries.len(), 2);
                for entry in &block.body.entries {
                    match &entry.kind {
                        BodyEntryKind::Property(prop) => {
                            assert!(matches!(&prop.value.value, Value::Secret(_)));
                        }
                        other => panic!("expected property, got {other:?}"),
                    }
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_nested_block() {
        let source = "server Srv:\n    rootProfile:\n        domain = \"example.com\"\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                assert_eq!(block.body.entries.len(), 1);
                match &block.body.entries[0].kind {
                    BodyEntryKind::NestedBlock(nb) => {
                        assert_eq!(nb.name.name, "rootProfile");
                    }
                    _ => panic!("expected nested block"),
                }
            }
            _ => panic!("expected block"),
        }
    }
}
