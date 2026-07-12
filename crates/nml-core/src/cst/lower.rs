//! CST → **semantic AST** lowering (RFC 0004 §7): build the typed [`crate::ast`]
//! (decoded values, resolved structure, CST-derived spans) from the lossless CST.
//!
//! This is the canonical lowering: the semantic AST is the model that validation /
//! deserialization / defaulting read, and it is built here from the CST. Tooling
//! that needs losslessness/comments/resilience reads the CST directly — the two
//! trees are complementary layers (lossless syntax ↔ typed semantics), not
//! duplicates.

use crate::ast::*;
use crate::cst::ast::{self, AstNode};
use crate::cst::syntax::{content_span, token_span, SyntaxToken};
use crate::cst::value::decode_string_token;
use crate::error::NmlError;
use crate::span::Span;
use crate::types::{SpannedValue, Value};

/// Lower a parsed CST to the semantic AST (resilient: decode errors are swallowed
/// into placeholder values). Use [`to_ast_with_errors`] to also collect them.
pub fn to_ast(root: &ast::Root) -> File {
    to_ast_with_errors(root).0
}

/// Lower to the semantic AST **and** collect every value-decode (semantic) error
/// in a single pass. The CST defers value validation to decode, so this is where
/// those diagnostics surface — once, as the AST is built (no second decode pass).
/// Powers [`parse_to_ast`](crate::cst::parse_to_ast).
pub fn to_ast_with_errors(root: &ast::Root) -> (File, Vec<NmlError>) {
    let mut cx = Lower { errors: Vec::new() };
    let file = File {
        declarations: root.decls().map(|d| cx.declaration(d)).collect(),
    };
    (file, cx.errors)
}

/// Lowering state: the diagnostics accumulator (rust-analyzer's pattern — a
/// stateful walk with a diagnostic sink, rather than threading `Result`).
struct Lower {
    errors: Vec<NmlError>,
}

impl Lower {
    fn declaration(&mut self, decl: ast::Decl) -> Declaration {
        let span = content_span(decl.syntax());
        let kind = match decl {
            ast::Decl::Block(b) => DeclarationKind::Block(self.block(&b)),
            ast::Decl::Array(a) => DeclarationKind::Array(self.array(&a)),
            ast::Decl::Const(c) => DeclarationKind::Const(ConstDecl {
                name: name_of(c.name()),
                value: self.value_of(c.value()),
            }),
            ast::Decl::Template(t) => DeclarationKind::Template(TemplateDecl {
                name: name_of(t.name()),
                value: self.value_of(t.value()),
            }),
            ast::Decl::OneOf(o) => DeclarationKind::OneOf(self.oneof(&o)),
        };
        Declaration { kind, span }
    }

    fn block(&mut self, b: &ast::BlockDecl) -> BlockDecl {
        BlockDecl {
            keyword: ident_of(b.keyword()),
            name: name_of(b.name()),
            extends: b
                .extends()
                .map(|e| e.parents().map(ident).collect())
                .unwrap_or_default(),
            body: self.body_of(b.body()),
        }
    }

    fn array(&mut self, a: &ast::ArrayDecl) -> ArrayDecl {
        let mut modifiers = Vec::new();
        let mut shared_properties = Vec::new();
        let mut properties = Vec::new();
        let mut items = Vec::new();
        if let Some(body) = a.body() {
            for entry in body.entries() {
                match entry {
                    ast::Entry::Modifier(m) => modifiers.push(self.modifier(&m)),
                    ast::Entry::SharedProperty(s) => shared_properties.push(self.shared(&s)),
                    ast::Entry::Property(p) => properties.push(self.property(&p)),
                    ast::Entry::ListItem(l) => items.push(self.list_item(&l)),
                    // Nested blocks / field defs / arms aren't valid in an array
                    // body (arms belong to a plain `name:` block, e.g. `denial:`).
                    ast::Entry::NestedBlock(_) | ast::Entry::FieldDef(_) | ast::Entry::Arm(_) => {}
                }
            }
        }
        ArrayDecl {
            item_keyword: ident_of(a.item_keyword()),
            name: name_of(a.name()),
            body: ArrayBody {
                modifiers,
                shared_properties,
                properties,
                items,
            },
        }
    }

    fn oneof(&mut self, o: &ast::OneOfDecl) -> OneOfDecl {
        OneOfDecl {
            name: name_of(o.name()),
            discriminator: ident_of(o.discriminator()),
            discriminator_type: o.enum_type().map(ident),
            default_discriminator: o.default_value().map(|t| {
                let s = self.string_token(&t);
                SpannedValue::new(Value::String(s), token_span(&t))
            }),
            arms: o
                .arms()
                .map(|arm| OneOfArm {
                    value: arm.value().map(|t| self.string_token(&t)).unwrap_or_default(),
                    value_span: arm.value().map(|t| token_span(&t)).unwrap_or(EMPTY_SPAN),
                    model: ident_of(arm.model()),
                })
                .collect(),
        }
    }

    /// Lower a present body's entries.
    fn lower_body(&mut self, body: ast::Body) -> Body {
        Body {
            entries: body.entries().map(|e| self.body_entry(e)).collect(),
        }
    }

    /// Lower an *optional* body — an absent body lowers to an empty one. For callers
    /// that already hold a body, use [`Self::lower_body`] directly.
    fn body_of(&mut self, body: Option<ast::Body>) -> Body {
        body.map(|b| self.lower_body(b)).unwrap_or_else(|| Body { entries: Vec::new() })
    }

    fn body_entry(&mut self, entry: ast::Entry) -> BodyEntry {
        let span = content_span(entry.syntax());
        let kind = match entry {
            ast::Entry::Property(p) => BodyEntryKind::Property(self.property(&p)),
            ast::Entry::NestedBlock(n) => BodyEntryKind::NestedBlock(NestedBlock {
                name: ident_of(n.name()),
                body: self.body_of(n.body()),
            }),
            ast::Entry::Modifier(m) => BodyEntryKind::Modifier(self.modifier(&m)),
            ast::Entry::SharedProperty(s) => BodyEntryKind::SharedProperty(self.shared(&s)),
            ast::Entry::ListItem(l) => BodyEntryKind::ListItem(self.list_item(&l)),
            ast::Entry::FieldDef(f) => BodyEntryKind::FieldDefinition(self.field_def(&f)),
            ast::Entry::Arm(a) => {
                let selector_tok = a.selector();
                // A `Role` token is a `@…` selector (stored verbatim); anything
                // else at that position is the `else` catch-all keyword.
                let selector = match &selector_tok {
                    Some(t) if t.kind() == crate::cst::syntax::SyntaxKind::Role => {
                        ArmSelector::Role(t.text().to_string())
                    }
                    _ => ArmSelector::Else,
                };
                BodyEntryKind::Arm(Arm {
                    selector,
                    selector_span: selector_tok.map(|t| token_span(&t)).unwrap_or(EMPTY_SPAN),
                    target: ident_of(a.target()),
                })
            }
        };
        BodyEntry { kind, span }
    }

    fn property(&mut self, p: &ast::Property) -> Property {
        Property {
            name: ident_of(p.name()),
            value: self.value_of(p.value()),
        }
    }

    fn modifier(&mut self, m: &ast::Modifier) -> Modifier {
        let value = if let Some(v) = m.value() {
            ModifierValue::Inline(self.decode(&v))
        } else if let Some(body) = m.body() {
            ModifierValue::Block(
                body.entries()
                    .filter_map(|e| match e {
                        ast::Entry::ListItem(l) => Some(self.list_item(&l)),
                        _ => None,
                    })
                    .collect(),
            )
        } else if let Some(te) = m.type_expr() {
            ModifierValue::TypeAnnotation {
                field_type: type_expr(&te),
                optional: m.optional(),
            }
        } else {
            ModifierValue::Block(Vec::new())
        };
        Modifier {
            name: ident_of(m.name()),
            value,
        }
    }

    fn shared(&mut self, s: &ast::SharedProperty) -> SharedProperty {
        let kind = if let Some(body) = s.body() {
            SharedPropertyKind::Block(self.lower_body(body))
        } else if let Some(v) = s.value() {
            SharedPropertyKind::Scalar(self.decode(&v))
        } else {
            SharedPropertyKind::Scalar(empty_value())
        };
        SharedProperty {
            name: ident_of(s.name()),
            kind,
        }
    }

    fn list_item(&mut self, l: &ast::ListItem) -> ListItem {
        let span = content_span(l.syntax());
        let kind = if let Some(v) = l.value() {
            // `- "/api"` (no body) or `- "/api":` + body (scalar-key-with-body).
            ListItemKind::Shorthand {
                value: self.decode(&v),
                body: l.body().map(|b| self.lower_body(b)),
            }
        } else if let Some(role) = l.role() {
            ListItemKind::Role(role.text().to_string())
        } else if let Some(name) = l.name() {
            if let Some(body) = l.body() {
                ListItemKind::Named {
                    name: ident(name),
                    body: self.lower_body(body),
                }
            } else {
                ListItemKind::Reference(ident(name))
            }
        } else {
            ListItemKind::Role(String::new())
        };
        ListItem { kind, span }
    }

    fn field_def(&mut self, f: &ast::FieldDef) -> FieldDefinition {
        FieldDefinition {
            name: ident_of(f.name()),
            field_type: f
                .type_expr()
                .map(|t| type_expr(&t))
                .unwrap_or_else(|| FieldTypeExpr::Named(empty_ident())),
            optional: f.optional(),
            shorthand: f.shorthand(),
            default_value: f.default().map(|v| self.decode(&v)),
        }
    }

    /// Decode a value, collecting any semantic error and substituting a
    /// placeholder so lowering stays total. `decode_value` only returns *semantic*
    /// errors (structural incompleteness, which the parser already reported,
    /// decodes to a placeholder), so this never double-counts a syntactic problem.
    fn decode(&mut self, v: &ast::ValueNode) -> SpannedValue {
        match v.decode() {
            Ok(sv) => sv,
            Err(e) => {
                self.push_error(e);
                empty_value()
            }
        }
    }

    /// Record a semantic error, bounded at `MAX_ERRORS` so a pathological file
    /// cannot grow the list without limit *during* lowering (RFC 0004 §9).
    fn push_error(&mut self, e: NmlError) {
        if self.errors.len() < super::MAX_ERRORS {
            self.errors.push(e);
        }
    }

    fn value_of(&mut self, v: Option<ast::ValueNode>) -> SpannedValue {
        v.map(|v| self.decode(&v)).unwrap_or_else(empty_value)
    }

    /// Decode a bare string-literal token (oneof values), collecting errors.
    fn string_token(&mut self, tok: &SyntaxToken) -> String {
        match decode_string_token(tok) {
            Ok(s) => s,
            Err(e) => {
                self.push_error(e);
                String::new()
            }
        }
    }
}

// ── pure structural converters / defaults (no decode) ──────────────────────

fn type_expr(te: &ast::TypeExpr) -> FieldTypeExpr {
    match te.kind() {
        ast::TypeExprKind::Named => FieldTypeExpr::Named(ident_of(te.name())),
        ast::TypeExprKind::Array => FieldTypeExpr::Array(Box::new(
            te.children()
                .next()
                .map(|t| type_expr(&t))
                .unwrap_or_else(|| FieldTypeExpr::Named(empty_ident())),
        )),
        ast::TypeExprKind::Union => {
            FieldTypeExpr::Union(te.children().map(|t| type_expr(&t)).collect())
        }
        ast::TypeExprKind::Arms => {
            // `(K -> V)`: exactly two child type exprs, key then target
            // (source order). Recovery may leave one missing; the empty-ident
            // fallback matches the `Array` arm above.
            let mut children = te.children();
            let mut next = || {
                children
                    .next()
                    .map(|t| type_expr(&t))
                    .unwrap_or_else(|| FieldTypeExpr::Named(empty_ident()))
            };
            FieldTypeExpr::Arms {
                key: Box::new(next()),
                target: Box::new(next()),
            }
        }
    }
}

const EMPTY_SPAN: Span = Span { start: 0, end: 0 };

fn ident(tok: SyntaxToken) -> Identifier {
    Identifier {
        name: tok.text().to_string(),
        span: token_span(&tok),
    }
}

fn ident_of(tok: Option<SyntaxToken>) -> Identifier {
    tok.map(ident).unwrap_or_else(empty_ident)
}

fn name_of(name: Option<ast::Name>) -> Identifier {
    name.and_then(|n| n.ident()).map(ident).unwrap_or_else(empty_ident)
}

fn empty_ident() -> Identifier {
    Identifier {
        name: String::new(),
        span: EMPTY_SPAN,
    }
}

fn empty_value() -> SpannedValue {
    SpannedValue::new(Value::String(String::new()), EMPTY_SPAN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cst::parse;

    fn cst_ast(src: &str) -> File {
        to_ast(&ast::Root::cast(parse(src).syntax()).unwrap())
    }

    /// RFC 0018 §4.4 arms: `(@selector | else) -> Target` under a plain block
    /// parse and lower with the right selector kind + target, and `else` stays a
    /// usable property name when it is not an arm (contextual keyword).
    #[test]
    fn arms_lower_with_role_and_else_selectors() {
        use crate::ast::{ArmSelector, BodyEntryKind, DeclarationKind};
        let file = cst_ast(
            "service App:\n    denial:\n        @plan/Pro -> ProUpsell\n        else -> Generic\n",
        );
        let DeclarationKind::Block(block) = &file.declarations[0].kind else {
            panic!("expected a block decl");
        };
        let BodyEntryKind::NestedBlock(nb) = &block.body.entries[0].kind else {
            panic!("expected a `denial:` nested block");
        };
        assert_eq!(nb.name.name, "denial");
        let arms: Vec<_> = nb
            .body
            .entries
            .iter()
            .map(|e| match &e.kind {
                BodyEntryKind::Arm(a) => a,
                other => panic!("expected an arm, got {other:?}"),
            })
            .collect();
        assert_eq!(arms.len(), 2);
        assert!(matches!(&arms[0].selector, ArmSelector::Role(r) if r == "@plan/Pro"));
        assert_eq!(arms[0].target.name, "ProUpsell");
        assert!(matches!(arms[1].selector, ArmSelector::Else));
        assert_eq!(arms[1].target.name, "Generic");

        // `else` is an arm ONLY when followed by `->`; as a property name it
        // still parses as a property (contextual keyword, not reserved).
        let prop = cst_ast("service App:\n    else = 5\n");
        let DeclarationKind::Block(b) = &prop.declarations[0].kind else {
            panic!("block");
        };
        assert!(matches!(
            &b.body.entries[0].kind,
            BodyEntryKind::Property(p) if p.name.name == "else"
        ));
    }

    /// RFC 0007 arm-set field types: `(K -> V)` lowers to `FieldTypeExpr::Arms`,
    /// composes with unions on either side, and the field-suffix `?` binds to
    /// the field (never the target type).
    #[test]
    fn arm_set_field_types_lower() {
        use crate::ast::{BodyEntryKind, DeclarationKind, FieldTypeExpr};
        let file = cst_ast(
            "model mount:\n    denial (string | (role -> denial))?\n    route (role -> (a | b))\n",
        );
        let DeclarationKind::Block(block) = &file.declarations[0].kind else {
            panic!("expected a block decl");
        };
        let fields: Vec<_> = block
            .body
            .entries
            .iter()
            .map(|e| match &e.kind {
                BodyEntryKind::FieldDefinition(f) => f,
                other => panic!("expected a field def, got {other:?}"),
            })
            .collect();

        // `denial (string | (role -> denial))?` — union of scalar and arm set,
        // optional on the FIELD.
        assert!(fields[0].optional, "the ? binds to the field");
        let FieldTypeExpr::Union(variants) = &fields[0].field_type else {
            panic!("expected a union, got {}", fields[0].field_type);
        };
        assert!(matches!(&variants[0], FieldTypeExpr::Named(n) if n.name == "string"));
        let FieldTypeExpr::Arms { key, target } = &variants[1] else {
            panic!("expected an arm set, got {}", variants[1]);
        };
        assert!(matches!(key.as_ref(), FieldTypeExpr::Named(n) if n.name == "role"));
        assert!(matches!(target.as_ref(), FieldTypeExpr::Named(n) if n.name == "denial"));
        assert_eq!(fields[0].field_type.to_string(), "(string | (role -> denial))");

        // `route (role -> (a | b))` — arm set whose target is a union.
        let FieldTypeExpr::Arms { target, .. } = &fields[1].field_type else {
            panic!("expected an arm set, got {}", fields[1].field_type);
        };
        assert!(matches!(target.as_ref(), FieldTypeExpr::Union(v) if v.len() == 2));
    }

    /// RFC 0007 §3: a BARE `K -> V` at field-type position is a parse error —
    /// the arrow, like the union pipe, is only consumed inside parens. This is
    /// what keeps the field-suffix `?` unambiguous.
    #[test]
    fn bare_arm_set_type_is_a_parse_error() {
        let parsed = parse("model mount:\n    denial role -> denial\n");
        assert!(
            !parsed.errors().is_empty(),
            "bare `K -> V` must not parse as a type: {:?}",
            parsed.errors()
        );
    }

    /// Breadth: the lowering must accept every construct — all declaration kinds,
    /// modifiers, fallback, defaults, oneof, arrays, nested blocks — in one source,
    /// cleanly (no errors) and with the expected shape.
    #[test]
    fn lowering_handles_the_full_grammar() {
        let src = "\
const MaxRetries = 3

template Greeting:
    \"Hello {{name}}\"

enum Status:
    - active
    - \"on-hold\"

trait Audited:
    auditedBy string

model Plan is Base, Audited:
    name string
    tier string?
    region string = \"us\"
    tags []string
    mode (active | inactive)
    price money = 9.99 USD
    |visibility role?

oneof email by provider as providerKind = \"log\":
    \"log\" -> emailLog
    \"postmark\" -> emailPostmark

[]mount mounts:
    |allow = [@authenticated, \"x\"]
    timeout = 30
    .region = \"us\"
    .defaults:
        retries = 3
    - Main:
        path = \"/\"
        nested:
            deep = true
    - \"shorthand\"
    - SomeRef

service App is Base:
    host = $ENV.HOST | \"localhost\"
    port = -8080
    enabled = false
";
        let file = crate::cst::parse_to_ast(src).expect("full grammar lowers cleanly");
        // const, template, enum, trait, model, oneof, []mount array, service.
        assert_eq!(file.declarations.len(), 8);
    }

    #[test]
    fn lowering_yields_decoded_values() {
        // Lowering decodes values, so a consumer reading the semantic AST sees the
        // fully-decoded value (escapes applied, money parsed), not raw text.
        let file = cst_ast("service S:\n    s = \"a\\nb\"\n    n = 100 USD\n");
        let DeclarationKind::Block(b) = &file.declarations[0].kind else {
            unreachable!()
        };
        let BodyEntryKind::Property(p0) = &b.body.entries[0].kind else {
            unreachable!()
        };
        assert_eq!(p0.value.value, Value::String("a\nb".into()));
        let BodyEntryKind::Property(p1) = &b.body.entries[1].kind else {
            unreachable!()
        };
        assert!(matches!(p1.value.value, Value::Money(_)));
    }
}
