use crate::ast::*;
use crate::error::{NmlError, NmlResult};
use crate::lexer::{Token, TokenKind};
use crate::money;
use crate::span::Span;
use crate::template;
use crate::types::{SpannedValue, Value};

const MAX_NESTING_DEPTH: u32 = 64;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    depth: u32,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0, depth: 0 }
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

        if keyword.name == "const" {
            self.expect_kind(TokenKind::Equals)?;
            self.skip_newlines();
            if self.check(TokenKind::Indent) {
                self.advance();
            }
            let value = self.parse_value_or_fallback()?;
            if self.check(TokenKind::Dedent) {
                self.advance();
            }
            let end_span = self.prev_span();
            return Ok(Declaration {
                kind: DeclarationKind::Const(ConstDecl { name, value }),
                span: start_span.merge(end_span),
            });
        }

        if keyword.name == "template" {
            self.expect_kind(TokenKind::Colon)?;
            self.skip_newlines();
            if self.check(TokenKind::Indent) {
                self.advance();
            }
            let value = self.parse_value()?;
            if !matches!(&value.value, Value::String(_) | Value::TemplateString(_)) {
                return Err(NmlError::parse(
                    "template value must be a string",
                    value.span,
                ));
            }
            self.skip_newlines();
            if self.check(TokenKind::Dedent) {
                self.advance();
            }
            let end_span = self.prev_span();
            return Ok(Declaration {
                kind: DeclarationKind::Template(TemplateDecl { name, value }),
                span: start_span.merge(end_span),
            });
        }

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

        self.depth += 1;
        if self.depth > MAX_NESTING_DEPTH {
            return Err(NmlError::parse(
                format!("maximum nesting depth ({MAX_NESTING_DEPTH}) exceeded"),
                self.current_span(),
            ));
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

        self.depth -= 1;

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

        // Identifier: could be property (name = value), nested block (name:),
        // or field definition (name type[?] [= default])
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
        } else if self.peek_kind_matches(|k| matches!(k, TokenKind::Identifier(_)))
            || self.check(TokenKind::ArrayPrefix)
            || self.check(TokenKind::ParenOpen)
        {
            let field = self.parse_field_definition(ident)?;
            let end = self.prev_span();
            Ok(BodyEntry {
                kind: BodyEntryKind::FieldDefinition(field),
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
        self.skip_newlines();
        let had_indent = if self.check(TokenKind::Indent) {
            self.advance();
            true
        } else {
            false
        };
        let value = self.parse_value_or_fallback()?;
        if had_indent {
            self.skip_newlines();
            if self.check(TokenKind::Dedent) {
                self.advance();
            }
        }
        self.skip_newlines();
        Ok(Property { name, value })
    }

    fn parse_nested_block_rest(&mut self, name: Identifier) -> NmlResult<NestedBlock> {
        self.expect_kind(TokenKind::Colon)?;
        self.skip_newlines();
        let body = self.parse_body()?;
        Ok(NestedBlock { name, body })
    }

    fn parse_field_type_expr(&mut self) -> NmlResult<FieldTypeExpr> {
        if self.check(TokenKind::ArrayPrefix) {
            self.advance();
            let inner = self.parse_field_type_expr()?;
            Ok(FieldTypeExpr::Array(Box::new(inner)))
        } else if self.check(TokenKind::ParenOpen) {
            self.advance();
            let mut variants = vec![self.parse_field_type_expr()?];
            while self.check(TokenKind::Pipe) {
                self.advance();
                variants.push(self.parse_field_type_expr()?);
            }
            self.expect_kind(TokenKind::ParenClose)?;
            Ok(FieldTypeExpr::Union(variants))
        } else {
            let name = self.expect_identifier()?;
            Ok(FieldTypeExpr::Named(name))
        }
    }

    fn parse_field_definition(&mut self, name: Identifier) -> NmlResult<FieldDefinition> {
        let field_type = self.parse_field_type_expr()?;

        let optional = if self.check(TokenKind::Question) {
            self.advance();
            true
        } else {
            false
        };

        let default_value = if self.check(TokenKind::Equals) {
            self.advance();
            Some(self.parse_value_or_fallback()?)
        } else {
            None
        };

        self.skip_newlines();
        Ok(FieldDefinition {
            name,
            field_type,
            optional,
            default_value,
        })
    }

    fn parse_modifier(&mut self) -> NmlResult<Modifier> {
        self.expect_kind(TokenKind::Pipe)?;
        let name = self.expect_identifier()?;

        if self.check(TokenKind::Equals) {
            self.advance();
            let value = self.parse_value_or_fallback()?;
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
        } else if self.peek_kind_matches(|k| matches!(k, TokenKind::Identifier(_)))
            || self.check(TokenKind::ArrayPrefix)
            || self.check(TokenKind::ParenOpen)
        {
            let field_type = self.parse_field_type_expr()?;
            let optional = if self.check(TokenKind::Question) {
                self.advance();
                true
            } else {
                false
            };
            self.skip_newlines();
            Ok(Modifier {
                name,
                value: ModifierValue::TypeAnnotation { field_type, optional },
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

    fn parse_value_or_fallback(&mut self) -> NmlResult<SpannedValue> {
        let value = self.parse_value()?;
        if self.check(TokenKind::Pipe) {
            self.advance();
            let fallback = self.parse_value_or_fallback()?;
            let span = value.span.merge(fallback.span);
            Ok(SpannedValue::new(
                Value::Fallback(Box::new(value), Box::new(fallback)),
                span,
            ))
        } else {
            Ok(value)
        }
    }

    fn parse_value(&mut self) -> NmlResult<SpannedValue> {
        let span = self.current_span();

        match self.peek_kind() {
            Some(TokenKind::StringLiteral(s)) => {
                let s = s.clone();
                self.advance();
                if s.contains("{{") {
                    let segments = template::parse_template_string(&s, span.start);
                    Ok(SpannedValue::new(Value::TemplateString(segments), span))
                } else {
                    Ok(SpannedValue::new(Value::String(s), span))
                }
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
    fn test_parse_const() {
        let source = "const Port = 8000\nconst Greeting = \"hello\"\n";
        let file = parse(source).unwrap();
        assert_eq!(file.declarations.len(), 2);

        match &file.declarations[0].kind {
            DeclarationKind::Const(c) => {
                assert_eq!(c.name.name, "Port");
                assert!(matches!(&c.value.value, Value::Number(n) if *n == 8000.0));
            }
            _ => panic!("expected const declaration"),
        }
        match &file.declarations[1].kind {
            DeclarationKind::Const(c) => {
                assert_eq!(c.name.name, "Greeting");
                assert!(matches!(&c.value.value, Value::String(s) if s == "hello"));
            }
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn test_parse_template() {
        let source = "template Greeting:\n    \"hello world\"\n";
        let file = parse(source).unwrap();
        assert_eq!(file.declarations.len(), 1);

        match &file.declarations[0].kind {
            DeclarationKind::Template(t) => {
                assert_eq!(t.name.name, "Greeting");
                assert!(matches!(&t.value.value, Value::String(s) if s == "hello world"));
            }
            _ => panic!("expected template declaration"),
        }
    }

    #[test]
    fn test_parse_template_multiline() {
        let source = "template Prompt:\n    \"\"\"\n    line one\n    line two\n    \"\"\"\n";
        let file = parse(source).unwrap();
        assert_eq!(file.declarations.len(), 1);

        match &file.declarations[0].kind {
            DeclarationKind::Template(t) => {
                assert_eq!(t.name.name, "Prompt");
                assert!(matches!(&t.value.value, Value::String(s) if s == "line one\nline two"));
            }
            _ => panic!("expected template declaration"),
        }
    }

    #[test]
    fn test_parse_string_with_template_expression() {
        let source = "service Svc:\n    instructions = \"{{args.instructions}} base rules\"\n";
        let file = parse(source).unwrap();
        let block = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => b,
            _ => panic!("expected block"),
        };
        match &block.body.entries[0].kind {
            BodyEntryKind::Property(p) => {
                match &p.value.value {
                    Value::TemplateString(segs) => {
                        assert_eq!(segs.len(), 2);
                        match &segs[0] {
                            crate::types::TemplateSegment::Expression { namespace, path, .. } => {
                                assert_eq!(namespace, "args");
                                assert_eq!(path, &["instructions"]);
                            }
                            _ => panic!("expected expression segment"),
                        }
                    }
                    _ => panic!("expected TemplateString, got {:?}", p.value.value),
                }
            }
            _ => panic!("expected property"),
        }
    }

    #[test]
    fn test_parse_plain_string_stays_string() {
        let source = "service Svc:\n    name = \"hello world\"\n";
        let file = parse(source).unwrap();
        let block = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => b,
            _ => panic!("expected block"),
        };
        match &block.body.entries[0].kind {
            BodyEntryKind::Property(p) => {
                assert!(matches!(&p.value.value, Value::String(s) if s == "hello world"));
            }
            _ => panic!("expected property"),
        }
    }

    #[test]
    fn test_parse_template_declaration_with_template_expr() {
        let source = "template Greeting:\n    \"{{args.name}} welcome\"\n";
        let file = parse(source).unwrap();
        match &file.declarations[0].kind {
            DeclarationKind::Template(t) => {
                assert!(matches!(&t.value.value, Value::TemplateString(_)));
            }
            _ => panic!("expected template declaration"),
        }
    }

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
    fn test_parse_mixed_list_named_and_shorthand() {
        let source = r#"
workflow W:
    steps:
        - classify:
            provider = "openai"
        - "/fallback/path"
        - DefaultHandler
        - respond:
            provider = "anthropic"
"#;
        let file = parse(source).unwrap();
        match &file.declarations[0].kind {
            DeclarationKind::Block(b) => {
                match &b.body.entries[0].kind {
                    BodyEntryKind::NestedBlock(nb) => {
                        let items: Vec<_> = nb.body.entries.iter()
                            .filter_map(|e| match &e.kind {
                                BodyEntryKind::ListItem(item) => Some(item),
                                _ => None,
                            })
                            .collect();
                        assert_eq!(items.len(), 4);
                        assert!(matches!(&items[0].kind, ListItemKind::Named { name, .. } if name.name == "classify"));
                        assert!(matches!(&items[1].kind, ListItemKind::Shorthand(v) if v.value == Value::String("/fallback/path".into())));
                        assert!(matches!(&items[2].kind, ListItemKind::Reference(id) if id.name == "DefaultHandler"));
                        assert!(matches!(&items[3].kind, ListItemKind::Named { name, .. } if name.name == "respond"));
                    }
                    other => panic!("expected NestedBlock, got {other:?}"),
                }
            }
            other => panic!("expected Block, got {other:?}"),
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

    #[test]
    fn test_parse_model_field_simple() {
        let source = "model mount:\n    path path\n    wasm string\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                assert_eq!(block.keyword.name, "model");
                assert_eq!(block.name.name, "mount");
                assert_eq!(block.body.entries.len(), 2);

                match &block.body.entries[0].kind {
                    BodyEntryKind::FieldDefinition(f) => {
                        assert_eq!(f.name.name, "path");
                        assert!(matches!(&f.field_type, FieldTypeExpr::Named(id) if id.name == "path"));
                        assert!(!f.optional);
                        assert!(f.default_value.is_none());
                    }
                    other => panic!("expected field definition, got {other:?}"),
                }

                match &block.body.entries[1].kind {
                    BodyEntryKind::FieldDefinition(f) => {
                        assert_eq!(f.name.name, "wasm");
                        assert!(matches!(&f.field_type, FieldTypeExpr::Named(id) if id.name == "string"));
                    }
                    other => panic!("expected field definition, got {other:?}"),
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_model_field_optional() {
        let source = "model provider:\n    baseUrl string?\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                match &block.body.entries[0].kind {
                    BodyEntryKind::FieldDefinition(f) => {
                        assert_eq!(f.name.name, "baseUrl");
                        assert!(matches!(&f.field_type, FieldTypeExpr::Named(id) if id.name == "string"));
                        assert!(f.optional);
                    }
                    other => panic!("expected field definition, got {other:?}"),
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_model_field_default() {
        let source = "model prompt:\n    outputFormat string = \"text\"\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                match &block.body.entries[0].kind {
                    BodyEntryKind::FieldDefinition(f) => {
                        assert_eq!(f.name.name, "outputFormat");
                        assert!(!f.optional);
                        match &f.default_value {
                            Some(v) => assert_eq!(v.value, Value::String("text".into())),
                            None => panic!("expected default value"),
                        }
                    }
                    other => panic!("expected field definition, got {other:?}"),
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_model_field_array_type() {
        let source = "model workflow:\n    steps []step\n    extensions []extensionPoint?\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                assert_eq!(block.body.entries.len(), 2);

                match &block.body.entries[0].kind {
                    BodyEntryKind::FieldDefinition(f) => {
                        assert_eq!(f.name.name, "steps");
                        assert!(matches!(&f.field_type, FieldTypeExpr::Array(inner) if matches!(inner.as_ref(), FieldTypeExpr::Named(id) if id.name == "step")));
                        assert!(!f.optional);
                    }
                    other => panic!("expected field definition, got {other:?}"),
                }

                match &block.body.entries[1].kind {
                    BodyEntryKind::FieldDefinition(f) => {
                        assert_eq!(f.name.name, "extensions");
                        assert!(matches!(&f.field_type, FieldTypeExpr::Array(inner) if matches!(inner.as_ref(), FieldTypeExpr::Named(id) if id.name == "extensionPoint")));
                        assert!(f.optional);
                    }
                    other => panic!("expected field definition, got {other:?}"),
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_model_field_union_type() {
        let source = "model step:\n    parallel [](step | []step)?\n";
        let file = parse(source).unwrap();
        let decl = &file.declarations[0];
        if let DeclarationKind::Block(block) = &decl.kind {
            match &block.body.entries[0].kind {
                BodyEntryKind::FieldDefinition(f) => {
                    assert_eq!(f.name.name, "parallel");
                    assert!(f.optional);
                    if let FieldTypeExpr::Array(inner) = &f.field_type {
                        if let FieldTypeExpr::Union(variants) = inner.as_ref() {
                            assert_eq!(variants.len(), 2);
                            assert!(matches!(&variants[0], FieldTypeExpr::Named(id) if id.name == "step"));
                            assert!(matches!(&variants[1], FieldTypeExpr::Array(inner) if matches!(inner.as_ref(), FieldTypeExpr::Named(id) if id.name == "step")));
                        } else {
                            panic!("expected Union, got {:?}", inner);
                        }
                    } else {
                        panic!("expected Array, got {:?}", f.field_type);
                    }
                }
                other => panic!("expected field definition, got {other:?}"),
            }
        } else {
            panic!("expected block");
        }
    }

    #[test]
    fn test_parse_model_field_type_ref() {
        let source = "model provider:\n    type providerType\n    model string\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                assert_eq!(block.body.entries.len(), 2);
                match &block.body.entries[0].kind {
                    BodyEntryKind::FieldDefinition(f) => {
                        assert_eq!(f.name.name, "type");
                        assert!(matches!(&f.field_type, FieldTypeExpr::Named(id) if id.name == "providerType"));
                    }
                    other => panic!("expected field definition, got {other:?}"),
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_fallback_single() {
        let source = "server Srv:\n    port = $ENV.PORT | 3000\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                match &block.body.entries[0].kind {
                    BodyEntryKind::Property(prop) => {
                        assert_eq!(prop.name.name, "port");
                        match &prop.value.value {
                            Value::Fallback(primary, fallback) => {
                                assert!(matches!(&primary.value, Value::Secret(s) if s == "$ENV.PORT"));
                                assert!(matches!(&fallback.value, Value::Number(n) if *n == 3000.0));
                            }
                            other => panic!("expected Fallback, got {other:?}"),
                        }
                    }
                    other => panic!("expected property, got {other:?}"),
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_fallback_chained() {
        let source = "server Srv:\n    port = $ENV.PORT | $ENV.DEFAULT_PORT | 8080\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                match &block.body.entries[0].kind {
                    BodyEntryKind::Property(prop) => {
                        match &prop.value.value {
                            Value::Fallback(primary, middle) => {
                                assert!(matches!(&primary.value, Value::Secret(s) if s == "$ENV.PORT"));
                                match &middle.value {
                                    Value::Fallback(mid_primary, final_val) => {
                                        assert!(matches!(&mid_primary.value, Value::Secret(s) if s == "$ENV.DEFAULT_PORT"));
                                        assert!(matches!(&final_val.value, Value::Number(n) if *n == 8080.0));
                                    }
                                    other => panic!("expected nested Fallback, got {other:?}"),
                                }
                            }
                            other => panic!("expected Fallback, got {other:?}"),
                        }
                    }
                    other => panic!("expected property, got {other:?}"),
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_secret_no_fallback_backward_compat() {
        let source = "provider P:\n    apiKey = $ENV.API_KEY\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                match &block.body.entries[0].kind {
                    BodyEntryKind::Property(prop) => {
                        assert!(matches!(&prop.value.value, Value::Secret(s) if s == "$ENV.API_KEY"));
                    }
                    other => panic!("expected property, got {other:?}"),
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_fallback_string() {
        let source = "auth A:\n    secret = $ENV.AUTH_SECRET | \"dev-secret\"\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                match &block.body.entries[0].kind {
                    BodyEntryKind::Property(prop) => {
                        match &prop.value.value {
                            Value::Fallback(primary, fallback) => {
                                assert!(matches!(&primary.value, Value::Secret(s) if s == "$ENV.AUTH_SECRET"));
                                assert!(matches!(&fallback.value, Value::String(s) if s == "dev-secret"));
                            }
                            other => panic!("expected Fallback, got {other:?}"),
                        }
                    }
                    other => panic!("expected property, got {other:?}"),
                }
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_modifier_does_not_consume_next_line_pipe() {
        let source = "service Svc:\n    port = $ENV.PORT\n    |allow = [@public]\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Block(block) => {
                assert_eq!(block.body.entries.len(), 2);
                assert!(matches!(&block.body.entries[0].kind, BodyEntryKind::Property(p) if p.name.name == "port"));
                assert!(matches!(&block.body.entries[1].kind, BodyEntryKind::Modifier(m) if m.name.name == "allow"));
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn test_parse_const_with_fallback() {
        let source = "const Port = $ENV.PORT | 3000\n";
        let file = parse(source).unwrap();

        match &file.declarations[0].kind {
            DeclarationKind::Const(c) => {
                assert_eq!(c.name.name, "Port");
                match &c.value.value {
                    Value::Fallback(primary, fallback) => {
                        assert!(matches!(&primary.value, Value::Secret(s) if s == "$ENV.PORT"));
                        assert!(matches!(&fallback.value, Value::Number(n) if *n == 3000.0));
                    }
                    other => panic!("expected Fallback, got {other:?}"),
                }
            }
            _ => panic!("expected const"),
        }
    }

    // -----------------------------------------------------------------------
    // Phase 1a: Malformed input tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_empty_input() {
        let file = parse("").unwrap();
        assert!(file.declarations.is_empty());
    }

    #[test]
    fn test_whitespace_only_input() {
        let file = parse("   \n\n   \n").unwrap();
        assert!(file.declarations.is_empty());
    }

    #[test]
    fn test_comment_only_input() {
        // Comments may or may not be fully consumed depending on lexer -- just must not panic
        let result = parse("# just a comment\n# another\n");
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_unterminated_string() {
        let result = parse("service App:\n    name = \"hello\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_unterminated_multiline_string() {
        let result = parse("service App:\n    bio = \"\"\"hello\n    world\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_block_no_body() {
        let result = parse("service App:\n");
        // Should either parse with empty body or error -- not panic
        match result {
            Ok(file) => {
                assert_eq!(file.declarations.len(), 1);
                if let DeclarationKind::Block(b) = &file.declarations[0].kind {
                    assert!(b.body.entries.is_empty());
                }
            }
            Err(_) => {} // also acceptable
        }
    }

    #[test]
    fn test_block_eof_immediately_after_colon() {
        let result = parse("service App:");
        match result {
            Ok(file) => {
                assert_eq!(file.declarations.len(), 1);
            }
            Err(_) => {}
        }
    }

    #[test]
    fn test_unexpected_token_number_as_keyword() {
        let result = parse("123 App:\n    x = 1\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_property_without_value() {
        let result = parse("service App:\n    port =\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_property_without_equals() {
        let result = parse("service App:\n    port 8080\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_duplicate_properties_parse_ok() {
        // Parser should accept duplicates; validation is separate
        let file = parse("service App:\n    port = 8080\n    port = 9090\n").unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            assert_eq!(b.body.entries.len(), 2);
        }
    }

    #[test]
    fn test_list_item_in_top_level() {
        let result = parse("- SomeItem:\n    x = 1\n");
        // Should error or parse differently -- not panic
        assert!(result.is_err() || result.unwrap().declarations.is_empty());
    }

    #[test]
    fn test_only_dash() {
        let result = parse("-\n");
        assert!(result.is_err() || result.is_ok());
        // Must not panic
    }

    #[test]
    fn test_nested_block_no_body() {
        let result = parse("service App:\n    db:\n");
        match result {
            Ok(file) => {
                if let DeclarationKind::Block(b) = &file.declarations[0].kind {
                    if let BodyEntryKind::NestedBlock(nb) = &b.body.entries[0].kind {
                        assert!(nb.body.entries.is_empty());
                    }
                }
            }
            Err(_) => {}
        }
    }

    #[test]
    fn test_modifier_without_value() {
        let result = parse("service App:\n    |allow\n");
        // Should parse or error cleanly, not panic
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_multiple_blocks() {
        let source = "service A:\n    x = 1\n\nservice B:\n    y = 2\n\nconst C = 3\n";
        let file = parse(source).unwrap();
        assert_eq!(file.declarations.len(), 3);
    }

    #[test]
    fn test_bool_values() {
        let file = parse("service App:\n    debug = true\n    verbose = false\n").unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            if let BodyEntryKind::Property(p) = &b.body.entries[0].kind {
                assert_eq!(p.value.value, Value::Bool(true));
            }
            if let BodyEntryKind::Property(p) = &b.body.entries[1].kind {
                assert_eq!(p.value.value, Value::Bool(false));
            }
        }
    }

    #[test]
    fn test_array_property_empty() {
        let file = parse("service App:\n    tags = []\n").unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            if let BodyEntryKind::Property(p) = &b.body.entries[0].kind {
                if let Value::Array(items) = &p.value.value {
                    assert!(items.is_empty());
                } else {
                    panic!("expected empty array");
                }
            }
        }
    }

    #[test]
    fn test_array_property_single_item() {
        let file = parse("service App:\n    tags = [\"web\"]\n").unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            if let BodyEntryKind::Property(p) = &b.body.entries[0].kind {
                if let Value::Array(items) = &p.value.value {
                    assert_eq!(items.len(), 1);
                }
            }
        }
    }

    #[test]
    fn test_negative_number() {
        let file = parse("service App:\n    offset = -10\n").unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            if let BodyEntryKind::Property(p) = &b.body.entries[0].kind {
                assert_eq!(p.value.value, Value::Number(-10.0));
            }
        }
    }

    #[test]
    fn test_floating_point_number() {
        let file = parse("service App:\n    rate = 0.75\n").unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            if let BodyEntryKind::Property(p) = &b.body.entries[0].kind {
                assert_eq!(p.value.value, Value::Number(0.75));
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 1b: Recursion / depth limits
    // -----------------------------------------------------------------------

    #[test]
    fn test_deeply_nested_blocks_within_limit() {
        let mut source = String::new();
        source.push_str("root R:\n");
        for i in 0..60 {
            let indent = "    ".repeat(i + 1);
            source.push_str(&format!("{}level{}:\n", indent, i));
        }
        let result = parse(&source);
        assert!(result.is_ok(), "60 levels of nesting should be within limit");
    }

    #[test]
    fn test_nesting_at_exact_limit() {
        let mut source = String::new();
        source.push_str("root R:\n");
        // 63 nested blocks + the top-level body = 64 parse_body calls = exactly at limit
        for i in 0..63 {
            let indent = "    ".repeat(i + 1);
            source.push_str(&format!("{}level{}:\n", indent, i));
        }
        let deepest_indent = "    ".repeat(64);
        source.push_str(&format!("{}value = \"leaf\"\n", deepest_indent));
        let result = parse(&source);
        assert!(result.is_ok(), "exactly at MAX_NESTING_DEPTH should succeed");
    }

    #[test]
    fn test_nesting_one_over_limit() {
        let mut source = String::new();
        source.push_str("root R:\n");
        // 64 nested blocks: depth increments happen when parse_body finds an Indent token.
        // Blocks 0..62 each have a child block (the next level), so they get Indent → 63 increments.
        // Block 63 also needs content to trigger Indent → total 64 body increments + 1 root = 65.
        for i in 0..64 {
            let indent = "    ".repeat(i + 1);
            source.push_str(&format!("{}level{}:\n", indent, i));
        }
        let deepest_indent = "    ".repeat(65);
        source.push_str(&format!("{}value = \"deep\"\n", deepest_indent));
        let result = parse(&source);
        assert!(result.is_err(), "one over MAX_NESTING_DEPTH should fail");
        let err = result.unwrap_err();
        assert!(
            err.message().contains("maximum nesting depth"),
            "error should mention depth limit; got: {}",
            err.message()
        );
    }

    #[test]
    fn test_nesting_depth_limit_exceeded() {
        let mut source = String::new();
        source.push_str("root R:\n");
        for i in 0..70 {
            let indent = "    ".repeat(i + 1);
            source.push_str(&format!("{}level{}:\n", indent, i));
        }
        let result = parse(&source);
        assert!(result.is_err(), "70 levels of nesting should exceed limit");
        let err = result.unwrap_err();
        assert!(
            err.message().contains("maximum nesting depth"),
            "error should mention depth limit; got: {}",
            err.message()
        );
    }

    #[test]
    fn test_wide_shallow_nesting_ok() {
        let mut source = String::new();
        source.push_str("root R:\n");
        for i in 0..200 {
            source.push_str(&format!("    prop{} = {}\n", i, i));
        }
        let result = parse(&source);
        assert!(result.is_ok(), "wide shallow nesting should not hit depth limit");
    }

    #[test]
    fn test_depth_limit_in_list_items() {
        let mut source = String::new();
        source.push_str("root R:\n");
        // Build nesting through list items: - item:\n        nested:\n ...
        let mut depth = 1;
        for i in 0..40 {
            let indent = "    ".repeat(depth);
            source.push_str(&format!("{}- item{}:\n", indent, i));
            depth += 1;
            let indent = "    ".repeat(depth);
            source.push_str(&format!("{}nested:\n", indent));
            depth += 1;
        }
        let result = parse(&source);
        // 1 (top body) + 40 * 2 (list item body + nested block body) = 81 > 64
        assert!(result.is_err(), "deep nesting through list items should be caught");
    }

    #[test]
    fn test_very_long_string_value() {
        let long_string = "x".repeat(100_000);
        let source = format!("service App:\n    data = \"{}\"\n", long_string);
        let file = parse(&source).unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            if let BodyEntryKind::Property(p) = &b.body.entries[0].kind {
                if let Value::String(s) = &p.value.value {
                    assert_eq!(s.len(), 100_000);
                }
            }
        }
    }

    #[test]
    fn test_large_array() {
        let items: Vec<String> = (0..1000).map(|i| format!("\"item{}\"", i)).collect();
        let source = format!("service App:\n    list = [{}]\n", items.join(", "));
        let file = parse(&source).unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            if let BodyEntryKind::Property(p) = &b.body.entries[0].kind {
                if let Value::Array(arr) = &p.value.value {
                    assert_eq!(arr.len(), 1000);
                }
            }
        }
    }

    #[test]
    fn test_many_list_items() {
        let mut source = String::from("workflow W:\n    steps:\n");
        for i in 0..200 {
            source.push_str(&format!("        - step{}:\n            x = {}\n", i, i));
        }
        let file = parse(&source).unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            if let BodyEntryKind::NestedBlock(nb) = &b.body.entries[0].kind {
                let items: Vec<_> = nb.body.entries.iter()
                    .filter(|e| matches!(&e.kind, BodyEntryKind::ListItem(_)))
                    .collect();
                assert_eq!(items.len(), 200);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 1c: Trait and enum parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_trait_declaration() {
        let source = "trait Auditable:\n    createdAt string\n    updatedAt string\n";
        let file = parse(source).unwrap();
        assert_eq!(file.declarations.len(), 1);
        match &file.declarations[0].kind {
            DeclarationKind::Block(b) => {
                assert_eq!(b.keyword.name, "trait");
                assert_eq!(b.name.name, "Auditable");
                assert_eq!(b.body.entries.len(), 2);

                match &b.body.entries[0].kind {
                    BodyEntryKind::FieldDefinition(f) => {
                        assert_eq!(f.name.name, "createdAt");
                        assert!(matches!(&f.field_type, FieldTypeExpr::Named(id) if id.name == "string"));
                    }
                    other => panic!("expected FieldDefinition, got {other:?}"),
                }
                match &b.body.entries[1].kind {
                    BodyEntryKind::FieldDefinition(f) => {
                        assert_eq!(f.name.name, "updatedAt");
                        assert!(matches!(&f.field_type, FieldTypeExpr::Named(id) if id.name == "string"));
                    }
                    other => panic!("expected FieldDefinition, got {other:?}"),
                }
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn test_trait_uses_same_field_syntax_as_model() {
        let model_source = "model User:\n    name string\n    age number\n";
        let trait_source = "trait Timestamped:\n    name string\n    age number\n";

        let model_file = parse(model_source).unwrap();
        let trait_file = parse(trait_source).unwrap();

        let model_block = match &model_file.declarations[0].kind {
            DeclarationKind::Block(b) => b,
            _ => panic!("expected Block"),
        };
        let trait_block = match &trait_file.declarations[0].kind {
            DeclarationKind::Block(b) => b,
            _ => panic!("expected Block"),
        };

        assert_eq!(model_block.body.entries.len(), trait_block.body.entries.len());
        for (m, t) in model_block.body.entries.iter().zip(trait_block.body.entries.iter()) {
            match (&m.kind, &t.kind) {
                (BodyEntryKind::FieldDefinition(mf), BodyEntryKind::FieldDefinition(tf)) => {
                    assert_eq!(mf.name.name, tf.name.name);
                    assert_eq!(mf.optional, tf.optional);
                }
                _ => panic!("both should be FieldDefinition"),
            }
        }
    }

    #[test]
    fn test_colon_after_field_name_parses_as_nested_block() {
        let source = "model Foo:\n    bar: string\n";
        let result = parse(source);
        assert!(result.is_err(), "name: type should fail -- colon starts a nested block, not a type annotation");
    }

    #[test]
    fn test_parse_enum_declaration() {
        let source = "enum Status:\n    - active\n    - inactive\n    - pending\n";
        let file = parse(source).unwrap();
        assert_eq!(file.declarations.len(), 1);
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            assert_eq!(b.keyword.name, "enum");
            assert_eq!(b.name.name, "Status");
        }
    }

    #[test]
    fn test_parse_enum_empty() {
        let result = parse("enum Empty:\n");
        match result {
            Ok(file) => {
                if let DeclarationKind::Block(b) = &file.declarations[0].kind {
                    assert!(b.body.entries.is_empty());
                }
            }
            Err(_) => {}
        }
    }

    #[test]
    fn test_parse_shared_property() {
        let source = "workflow W:\n    .defaults:\n        retries = 3\n    - step1:\n        x = 1\n";
        let file = parse(source).unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            let has_shared = b.body.entries.iter().any(|e| {
                matches!(&e.kind, BodyEntryKind::SharedProperty(_))
            });
            assert!(has_shared, "expected a SharedProperty entry");
        }
    }

    #[test]
    fn test_parse_template_string_in_property() {
        let source = "service App:\n    greeting = \"Hello {{args.name}}\"\n";
        let file = parse(source).unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            if let BodyEntryKind::Property(p) = &b.body.entries[0].kind {
                assert!(matches!(&p.value.value, Value::TemplateString(_)));
            }
        }
    }

    #[test]
    fn test_parse_path_value() {
        // Paths are stored as strings with path-like syntax
        let file = parse("service App:\n    dir = \"./static\"\n").unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            if let BodyEntryKind::Property(p) = &b.body.entries[0].kind {
                assert!(matches!(&p.value.value, Value::String(_)));
            }
        }
    }

    #[test]
    fn test_parse_reference_value() {
        let file = parse("service App:\n    provider = GroqFast\n").unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            if let BodyEntryKind::Property(p) = &b.body.entries[0].kind {
                assert!(matches!(&p.value.value, Value::Reference(_)));
            }
        }
    }

    #[test]
    fn test_parse_role_ref_value() {
        let file = parse("service App:\n    role = @role/admin\n").unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            if let BodyEntryKind::Property(p) = &b.body.entries[0].kind {
                assert!(matches!(&p.value.value, Value::RoleRef(_)));
            }
        }
    }

    #[test]
    fn test_parse_multiline_string() {
        let source = "service App:\n    bio = \"\"\"\n    Hello\n    World\n    \"\"\"\n";
        let file = parse(source).unwrap();
        if let DeclarationKind::Block(b) = &file.declarations[0].kind {
            if let BodyEntryKind::Property(p) = &b.body.entries[0].kind {
                assert!(
                    matches!(&p.value.value, Value::String(_) | Value::TemplateString(_)),
                    "expected string from multiline"
                );
            }
        }
    }
}
