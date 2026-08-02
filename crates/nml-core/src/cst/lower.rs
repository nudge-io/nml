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
use crate::cst::syntax::{SyntaxToken, content_span, token_span};
use crate::cst::value::{ValueErrors, decode_string_token_all};
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
pub fn to_ast_with_errors(root: &ast::Root) -> (File, Vec<NmlError>, usize) {
    let mut cx = Lower {
        errors: Vec::new(),
        suppressed: 0,
    };
    let file = File {
        declarations: root.decls().map(|d| cx.declaration(d)).collect(),
    };
    (file, cx.errors, cx.suppressed)
}

/// Lowering state: the diagnostics accumulator (rust-analyzer's pattern — a
/// stateful walk with a diagnostic sink, rather than threading `Result`).
struct Lower {
    errors: Vec<NmlError>,
    /// Errors dropped at the `MAX_ERRORS` cap — counted, never silent.
    suppressed: usize,
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
                    value: arm
                        .value()
                        .map(|t| self.string_token(&t))
                        .unwrap_or_default(),
                    value_span: arm.value().map(|t| token_span(&t)).unwrap_or(EMPTY_SPAN),
                    model: ident_of(arm.model()),
                })
                .collect(),
        }
    }

    /// Lower a present body's entries.
    fn lower_body(&mut self, body: ast::Body) -> Body {
        Body::fresh(body.entries().map(|e| self.body_entry(e)).collect())
    }

    /// Lower an *optional* body — an absent body lowers to an empty one. For callers
    /// that already hold a body, use [`Self::lower_body`] directly.
    fn body_of(&mut self, body: Option<ast::Body>) -> Body {
        body.map(|b| self.lower_body(b))
            .unwrap_or_else(|| Body::fresh(Vec::new()))
    }

    fn body_entry(&mut self, entry: ast::Entry) -> BodyEntry {
        let span = content_span(entry.syntax());
        let kind = match entry {
            ast::Entry::Property(p) => BodyEntryKind::Property(self.property(&p)),
            ast::Entry::NestedBlock(n) => {
                // RFC 0015: an `as <Variant>` annotation rides on the block's
                // `Body`, so the one canonical resolver honors it with no
                // per-consumer threading.
                let mut body = self.body_of(n.body());
                body.type_annotation = n.type_annotation().map(ident);
                BodyEntryKind::NestedBlock(NestedBlock {
                    name: ident_of(n.name()),
                    body,
                })
            }
            ast::Entry::Modifier(m) => BodyEntryKind::Modifier(self.modifier(&m)),
            ast::Entry::SharedProperty(s) => BodyEntryKind::SharedProperty(self.shared(&s)),
            ast::Entry::ListItem(l) => BodyEntryKind::ListItem(self.list_item(&l)),
            ast::Entry::FieldDef(f) => BodyEntryKind::FieldDefinition(self.field_def(&f)),
            ast::Entry::Arm(a) => {
                let selector_tok = a.selector();
                let selector = match &selector_tok {
                    Some(t) if t.kind() == crate::cst::syntax::SyntaxKind::Role => {
                        ArmSelector::Role(t.text().to_string())
                    }
                    Some(t) if t.kind() == crate::cst::syntax::SyntaxKind::String => {
                        ArmSelector::Literal(self.string_token(t))
                    }
                    _ => ArmSelector::Else,
                };
                let target = match a.target() {
                    // String after `->` is ALWAYS a literal — even when error
                    // recovery attached a body child under the Arm node for the
                    // `-> "name":` mistake (RFC 0007 §6.2).
                    Some(t) if t.kind() == crate::cst::syntax::SyntaxKind::String => {
                        ArmTarget::Literal {
                            value: self.string_token(&t),
                            span: token_span(&t),
                        }
                    }
                    Some(_) => {
                        if let Some(body) = a.inline_body() {
                            ArmTarget::Inline {
                                name: ident_of(a.target()),
                                body: self.lower_body(body),
                            }
                        } else if a.has_colon() {
                            let name = ident_of(a.target());
                            ArmTarget::Inline {
                                name,
                                body: Body::fresh(Vec::new()),
                            }
                        } else {
                            ArmTarget::Reference(ident_of(a.target()))
                        }
                    }
                    other => ArmTarget::Reference(ident_of(other)),
                };
                BodyEntryKind::Arm(Arm {
                    selector,
                    selector_span: selector_tok.map(|t| token_span(&t)).unwrap_or(EMPTY_SPAN),
                    target,
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
                field_type: type_expr(&te, &mut self.errors),
                optional: m.optional(),
                directives: m
                    .directives()
                    .map(|d| crate::types::Directive {
                        name: d.name().map(|t| t.text().to_string()).unwrap_or_default(),
                        arg: d.value().map(|v| self.decode(&v)),
                        span: super::syntax::node_span(d.syntax()),
                    })
                    .collect(),
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
        } else if l.role().is_some() {
            // RFC 0011: join a role-conjunction item (`- @role/a & @role/b`)
            // into the same canonical `" & "` form the value layer produces.
            let joined = l
                .syntax()
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == crate::cst::syntax::SyntaxKind::Role)
                .map(|t| t.text().to_string())
                .collect::<Vec<_>>()
                .join(" & ");
            ListItemKind::Role(joined)
        } else if let Some(name) = l.name() {
            // RFC 0015: an `as <Variant>` annotation on the item rides on its
            // `Body`. An annotation is itself a mark of an *instance* (not a bare
            // reference), so its presence forces the Named form even without an
            // explicit body — the annotation is never dropped.
            let annotation = l.type_annotation().map(ident);
            if let Some(body) = l.body() {
                let mut body = self.lower_body(body);
                body.type_annotation = annotation;
                ListItemKind::Named {
                    name: ident(name),
                    body,
                }
            } else if l.has_colon() || annotation.is_some() {
                // `- Name:` (or `- Name as V`) with no entries — the colon or the
                // annotation marks an inline instance with an (empty) body, not a
                // reference. Lowering it as Named keeps it visible to instance
                // validation (its missing required fields must fail loud), where
                // collapsing it into `Reference` made it structurally invisible.
                let mut body = Body::fresh(Vec::new());
                body.type_annotation = annotation;
                ListItemKind::Named {
                    name: ident(name),
                    body,
                }
            } else if name.text() == "true" || name.text() == "false" {
                // Parity with value position: `true`/`false` are the boolean
                // literals (decode_scalar's Ident arm), never references —
                // `- true` must mean what `[true]` means.
                ListItemKind::Shorthand {
                    value: SpannedValue::new(Value::Bool(name.text() == "true"), token_span(&name)),
                    body: None,
                }
            } else {
                ListItemKind::Reference(ident(name))
            }
        } else {
            // No recognizable item shape: an error, never a silent empty
            // item (an unrendered construct would be data loss downstream).
            self.push_error(NmlError::syntax(
                crate::error::ParseErrorKind::Expected {
                    expected: vec![crate::error::ExpectedItem::Desc("a list item")],
                    found: None,
                    context: Some("after '-'"),
                },
                span,
            ));
            ListItemKind::Role(String::new())
        };
        ListItem { kind, span }
    }

    fn field_def(&mut self, f: &ast::FieldDef) -> FieldDefinition {
        FieldDefinition {
            name: ident_of(f.name()),
            field_type: f
                .type_expr()
                .map(|t| type_expr(&t, &mut self.errors))
                .unwrap_or_else(|| bare_named(empty_ident())),
            optional: f.optional(),
            shorthand: f.shorthand(),
            default_value: f.default().map(|v| self.decode(&v)),
            directives: f
                .directives()
                .map(|d| crate::types::Directive {
                    name: d.name().map(|t| t.text().to_string()).unwrap_or_default(),
                    arg: d.value().map(|v| self.decode(&v)),
                    span: super::syntax::node_span(d.syntax()),
                })
                .collect(),
        }
    }

    /// Decode a value TOTALLY: every semantic error the value carries is
    /// collected (all bad escapes at once, rustc-style) and the decoder's
    /// best-effort recovery value is kept — lenient surfaces get value AND
    /// findings. The decoder only reports *semantic* errors (structural
    /// incompleteness, which the parser already reported, decodes to a
    /// placeholder), so this never double-counts a syntactic problem.
    fn decode(&mut self, v: &ast::ValueNode) -> SpannedValue {
        let mut sink = ValueErrors::default();
        let sv = v.decode_all(&mut sink);
        for e in sink.errors {
            self.push_error(e);
        }
        self.suppressed += sink.suppressed;
        sv
    }

    /// Record a semantic error, bounded at `MAX_ERRORS` so a pathological file
    /// cannot grow the list without limit *during* lowering (RFC 0004 §9).
    fn push_error(&mut self, e: NmlError) {
        if self.errors.len() < super::MAX_ERRORS {
            self.errors.push(e);
        } else {
            self.suppressed += 1;
        }
    }

    fn value_of(&mut self, v: Option<ast::ValueNode>) -> SpannedValue {
        v.map(|v| self.decode(&v)).unwrap_or_else(empty_value)
    }

    /// Decode a bare string-literal token (oneof values) **totally**: every
    /// bad escape is reported (not just the first) and the U+FFFD-recovered
    /// text is kept, so lenient surfaces keep the arm's identity — the same
    /// contract property values get from `decode_value_all`.
    fn string_token(&mut self, tok: &SyntaxToken) -> String {
        let mut sink = ValueErrors::default();
        let s = decode_string_token_all(tok, &mut sink);
        for e in sink.errors {
            self.push_error(e);
        }
        self.suppressed += sink.suppressed;
        s
    }
}

// ── pure structural converters / defaults (no decode) ──────────────────────

/// A facet-free `Named` — the recovery placeholder and the common case.
fn bare_named(id: Identifier) -> FieldTypeExpr {
    FieldTypeExpr::Named {
        name: id,
        facets: Vec::new(),
    }
}

/// RFC 0018 facet lowering. Facet values are number literals and route
/// through [`super::value::parse_number`] — the one place a
/// `NumberError` gains a span and an NML code — so a domain-rejected
/// facet (`min = 1e9999`) reports the SAME NML0013/NML0014 surface as
/// any config literal, then recovers to `Number::ZERO` (the error, not
/// the placeholder, is the contract).
fn facets_of(te: &ast::TypeExpr, errors: &mut Vec<NmlError>) -> Vec<FacetExpr> {
    let Some(list) = te.facet_list() else {
        return Vec::new();
    };
    list.facets()
        .filter_map(|f| {
            let span = super::syntax::node_span(f.syntax());
            let key = f.name().map(ident).unwrap_or_else(|| Identifier {
                name: String::new(),
                span,
            });
            let (text, vspan) = if let Some(dl) = f.duration_literal() {
                let components = dl.components();
                let first = components.first()?.0.text().to_string();
                let span = super::syntax::node_span(dl.syntax());
                let text = if f.dash().is_some() {
                    format!("-{}", first)
                } else {
                    first
                };
                (text, span)
            } else {
                match (f.dash(), f.number()) {
                    (Some(d), Some(n)) => (
                        format!("-{}", n.text()),
                        Span::new(
                            usize::from(d.text_range().start()),
                            usize::from(n.text_range().end()),
                        ),
                    ),
                    (None, Some(n)) => (
                        n.text().to_string(),
                        Span::new(
                            usize::from(n.text_range().start()),
                            usize::from(n.text_range().end()),
                        ),
                    ),
                    _ => return None,
                }
            };
            // A facet whose literal does not decode is DROPPED, not
            // recovered to zero — matching extraction (cst/extract.rs),
            // so the two facet builders can never disagree. A recovered
            // zero invented a bound nobody wrote: `min = <45 digits>`
            // became `min = 0`, and every later rule then measured
            // against a phantom (a violating-default report naming a
            // bound absent from the source). The NML0014/NML0013 error
            // is the finding; a fabricated facet on top is noise. Safe
            // for fmt, which refuses to format an errored document.
            //
            // A unit token makes the literal a DURATION (RFC 0017) —
            // same decode-or-drop contract, RFC 0017's own errors
            // (unknown unit, out of range, negative).
            let raw_components = f.duration_literal().map(|dl| dl.components());
            let value = if let Some(raw_components) = raw_components {
                if let Some((dangling, _)) = raw_components.iter().find(|(_, u)| u.is_none()) {
                    let break_span = Span::new(
                        usize::from(dangling.text_range().start()),
                        usize::from(dangling.text_range().end()),
                    );
                    errors.push(crate::error::NmlError::Duration {
                        kind: crate::duration::DurationErrorKind::MalformedCompound { break_span },
                        span: Span::new(vspan.start, break_span.end),
                    });
                    return None;
                }
                let pairs: Vec<_> = raw_components
                    .into_iter()
                    .map(|(n, u)| (n, u.expect("checked above")))
                    .collect();
                let components = super::syntax::duration_component_tokens(&pairs, &text);
                let literal_span = pairs
                    .last()
                    .map(|(_, u)| Span::new(vspan.start, usize::from(u.text_range().end())))
                    .unwrap_or(vspan);
                match crate::duration::parse_components_cst(&components, literal_span) {
                    Ok(d) => SpannedValue::new(crate::types::Value::Duration(d), literal_span),
                    Err(e) => {
                        errors.push(e);
                        return None;
                    }
                }
            } else {
                match super::value::parse_number(&text, vspan) {
                    Ok(n) => SpannedValue::new(crate::types::Value::Number(n), vspan),
                    Err(e) => {
                        errors.push(e);
                        return None;
                    }
                }
            };
            Some(FacetExpr { key, value, span })
        })
        .collect()
}

fn type_expr(te: &ast::TypeExpr, errors: &mut Vec<NmlError>) -> FieldTypeExpr {
    match te.kind() {
        ast::TypeExprKind::Named => FieldTypeExpr::Named {
            name: ident_of(te.name()),
            facets: facets_of(te, errors),
        },
        ast::TypeExprKind::Array => FieldTypeExpr::Array(Box::new(
            te.children()
                .next()
                .map(|t| type_expr(&t, errors))
                .unwrap_or_else(|| bare_named(empty_ident())),
        )),
        ast::TypeExprKind::Union => {
            FieldTypeExpr::Union(te.children().map(|t| type_expr(&t, errors)).collect())
        }
        ast::TypeExprKind::Arms => {
            // `(K -> V)`: exactly two child type exprs, key then target
            // (source order). Recovery may leave one missing; the empty-ident
            // fallback matches the `Array` arm above.
            let mut children = te.children();
            let mut next = || {
                children
                    .next()
                    .map(|t| type_expr(&t, errors))
                    .unwrap_or_else(|| bare_named(empty_ident()))
            };
            FieldTypeExpr::Arms {
                key: Box::new(next()),
                target: Box::new(next()),
            }
        }
        ast::TypeExprKind::Set => {
            // `set<T>` (RFC 0032): several children = a bare union's variants
            // (canonical `set<a | b>`); one = the element type. Recovery may
            // leave none; the empty-ident fallback matches `Array` above.
            let mut children: Vec<FieldTypeExpr> =
                te.children().map(|t| type_expr(&t, errors)).collect();
            let element = match children.len() {
                0 => bare_named(empty_ident()),
                1 => children.pop().expect("len checked"),
                _ => FieldTypeExpr::Union(children),
            };
            FieldTypeExpr::Set(Box::new(element))
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
    name.and_then(|n| n.ident())
        .map(ident)
        .unwrap_or_else(empty_ident)
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
    /// RFC 0018: facet lists parse, lower with exact values, render
    /// canonically, and survive on any Named type (number-only is the
    /// schema-load rule, so the parse keeps the evidence structured).
    #[test]
    fn facet_lists_lower_and_render() {
        let file = cst_ast(
            "model server:\n    port number(min = 1, max = 65535)\n    weight number(min = -1.5, exclusiveMax = 1)\n    step set<number(multipleOf = 0.01)>\n    ref denial(min = 2)\n",
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
        let FieldTypeExpr::Named { name, facets } = &fields[0].field_type else {
            panic!("expected named, got {}", fields[0].field_type);
        };
        assert_eq!(name.name, "number");
        assert_eq!(facets.len(), 2);
        assert_eq!(facets[0].key.name, "min");
        assert_eq!(
            fields[0].field_type.to_string(),
            "number(min = 1, max = 65535)"
        );
        assert_eq!(
            fields[1].field_type.to_string(),
            "number(min = -1.5, exclusiveMax = 1)"
        );
        assert_eq!(
            fields[2].field_type.to_string(),
            "set<number(multipleOf = 0.01)>"
        );
        // Model refs keep the facet evidence for the NML2058 pass.
        assert_eq!(fields[3].field_type.to_string(), "denial(min = 2)");
    }

    /// A domain-rejected facet literal reports the SAME NML0014 surface
    /// as a config literal (one numeric error vocabulary) and recovers
    /// to the zero placeholder.
    /// Round 22: facets compose with every other field suffix —
    /// optional `?`, a `= default`, trailing `#directives`, and the
    /// positional `+` — since the facet list sits between the type
    /// name and all of them. Space before the paren does NOT attach
    /// (an attached list is part of the type name, like a call).
    #[test]
    fn facets_compose_with_field_suffixes() {
        let file = cst_ast(
            "model m:\n    a number(min = 1)?\n    b number(min = 0) = 5\n    c number(max = 9) #live\n    d number(min = 1)+\n",
        );
        let DeclarationKind::Block(block) = &file.declarations[0].kind else {
            panic!("expected block");
        };
        let fields: Vec<_> = block
            .body
            .entries
            .iter()
            .filter_map(|e| match &e.kind {
                BodyEntryKind::FieldDefinition(f) => Some(f),
                _ => None,
            })
            .collect();
        assert!(fields[0].optional, "`?` binds to the field, not the facets");
        assert_eq!(fields[0].field_type.to_string(), "number(min = 1)");
        assert!(fields[1].default_value.is_some(), "`= default` survives");
        assert_eq!(fields[1].field_type.to_string(), "number(min = 0)");
        assert_eq!(fields[2].directives.len(), 1, "directives survive");
        assert_eq!(fields[2].field_type.to_string(), "number(max = 9)");
        assert!(fields[3].shorthand, "`+` binds to the field");

        // Whitespace before the paren attaches, like whitespace
        // anywhere else within a line; fmt canonicalizes the spelling.
        // No ambiguity with union types: a `(` at type-START is the
        // union branch, a different path — this one runs only after a
        // type NAME was consumed, where a paren was previously always
        // a parse error.
        let (file, errors) = crate::cst::parse_to_ast_all("model m:\n    a number (min = 1)\n");
        assert!(
            errors.is_empty(),
            "space before the paren attaches: {errors:?}"
        );
        let DeclarationKind::Block(block) = &file.declarations[0].kind else {
            panic!("expected block");
        };
        let BodyEntryKind::FieldDefinition(f) = &block.body.entries[0].kind else {
            panic!("expected field");
        };
        assert_eq!(f.field_type.to_string(), "number(min = 1)");
    }

    /// F3 (round-21): facet lists never span lines, and a newline can
    /// never let a facet list swallow the NEXT field. Singular
    /// findings: the first failed expectation ends the list.
    #[test]
    fn facet_lists_do_not_cross_newlines() {
        // Value on the next line: error, and the next line survives.
        let (_f, errors) = crate::cst::parse_to_ast_all("model m:\n    a number(min =\n    1)\n");
        assert!(!errors.is_empty(), "cross-line facet value must error");
        // List opened on the next line: NOT a facet list at all.
        let (_f, errors) = crate::cst::parse_to_ast_all("model m:\n    a number\n    (min = 1)\n");
        assert!(!errors.is_empty(), "next-line paren is not a facet list");
        // A dangling comma must not absorb the next FIELD.
        let (file, errors) =
            crate::cst::parse_to_ast_all("model m:\n    a number(min = 1,\n    b string\n");
        assert!(!errors.is_empty());
        let DeclarationKind::Block(block) = &file.declarations[0].kind else {
            panic!("expected block");
        };
        let fields: Vec<_> = block
            .body
            .entries
            .iter()
            .filter_map(|e| match &e.kind {
                BodyEntryKind::FieldDefinition(f) => Some(f.name.name.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            fields.contains(&"b"),
            "the next field must survive: {fields:?}"
        );
        // Singular findings on malformed lists.
        for (src, max) in [
            ("model m:\n    a number()\n", 2usize),
            ("model m:\n    a number(min = 1,)\n", 2),
            ("model m:\n    a number(min)\n", 2),
        ] {
            let (_f, errors) = crate::cst::parse_to_ast_all(src);
            assert!(
                !errors.is_empty() && errors.len() <= max,
                "{src:?}: want 1..={max} findings, got {}: {errors:?}",
                errors.len()
            );
        }
    }

    /// A facet whose literal is out of the exact domain is DROPPED,
    /// never recovered to zero — otherwise the AST carries a bound the
    /// author never wrote and later rules measure against a phantom
    /// (the lowerer and extraction would also disagree about the same
    /// source). The NML0014 stands alone.
    #[test]
    fn undecodable_facet_is_dropped_not_zeroed() {
        let src = format!("model m:\n    x number(min = {}) = -5\n", "9".repeat(45));
        let (file, errors) = crate::cst::parse_to_ast_all(&src);
        assert!(
            errors
                .iter()
                .any(|e| e.to_string().contains("significant digits")),
            "the literal error still reports: {errors:?}"
        );
        let DeclarationKind::Block(block) = &file.declarations[0].kind else {
            panic!("expected block");
        };
        let BodyEntryKind::FieldDefinition(fd) = &block.body.entries[0].kind else {
            panic!("expected field");
        };
        let FieldTypeExpr::Named { facets, .. } = &fd.field_type else {
            panic!("expected named");
        };
        assert!(
            facets.is_empty(),
            "the bad facet must be dropped: {facets:?}"
        );
        // And no phantom violation is invented for the default.
        let (_s, diags) = crate::cst::extract_schema(&src);
        assert!(
            diags
                .iter()
                .all(|d| d.code != Some(crate::diagnostic::codes::FACET_VIOLATION)),
            "no phantom min = 0: {diags:?}"
        );
    }

    #[test]
    fn facet_literal_out_of_domain_is_nml0014() {
        let src = format!("model m:\n    x number(min = {})\n", "9".repeat(35));
        let (_file, errors) = crate::cst::parse_to_ast_all(&src);
        assert!(
            errors
                .iter()
                .any(|e| e.to_string().contains("significant digits")),
            "{errors:?}"
        );
    }

    use super::*;
    use crate::cst::parse;

    fn cst_ast(src: &str) -> File {
        to_ast(&ast::Root::cast(parse(src).syntax()).unwrap())
    }

    /// RFC 0030: `- Name:` (trailing colon, no entries) lowers as a Named item
    /// with an empty body — an inline instance visible to validation — while
    /// `- Name` (no colon) stays a Reference link. Collapsing the colon form
    /// into Reference made missing-required errors structurally impossible.
    #[test]
    fn empty_colon_item_lowers_named_bare_stays_reference() {
        use crate::ast::{BodyEntryKind, DeclarationKind, ListItemKind};
        let items_of = |src: &str| {
            let file = cst_ast(src);
            let DeclarationKind::Array(arr) = &file.declarations[0].kind else {
                panic!("expected array");
            };
            arr.body.items.clone()
        };
        let with_colon = items_of("[]role roles:\n    - admin:\n");
        assert!(
            matches!(&with_colon[0].kind, ListItemKind::Named { name, body }
                if name.name == "admin" && body.entries.is_empty()),
            "{with_colon:?}"
        );
        let bare = items_of("[]role roles:\n    - admin\n");
        assert!(
            matches!(&bare[0].kind, ListItemKind::Reference(id) if id.name == "admin"),
            "{bare:?}"
        );
        // Same distinction holds for nested list items inside a block body.
        let file = cst_ast("plan Pro:\n    includes:\n        - Free\n");
        let DeclarationKind::Block(block) = &file.declarations[0].kind else {
            panic!("expected block");
        };
        let BodyEntryKind::NestedBlock(nb) = &block.body.entries[0].kind else {
            panic!("expected nested block");
        };
        assert!(matches!(
            &nb.body.entries[0].kind,
            BodyEntryKind::ListItem(item) if matches!(&item.kind, ListItemKind::Reference(_))
        ));
    }

    /// RFC 0018 §4.4 arms: `(@selector | else) -> Target` under a plain block
    /// parse and lower with the right selector kind + target, and `else` stays a
    /// usable property name when it is not an arm (contextual keyword).
    #[test]
    fn arms_lower_with_role_and_else_selectors() {
        use crate::ast::{ArmSelector, ArmTarget, BodyEntryKind, DeclarationKind};
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
        assert!(matches!(&arms[0].target, ArmTarget::Reference(id) if id.name == "ProUpsell"));
        assert!(matches!(arms[1].selector, ArmSelector::Else));
        assert!(matches!(&arms[1].target, ArmTarget::Reference(id) if id.name == "Generic"));

        // RFC 0007 §6: a STRING literal after the arrow is a path/url target
        // (for flat routers), decoded like a oneof arm value.
        let file = cst_ast(
            "service App:\n    dispatch:\n        @role/admin -> \"admin.workflow.nml\"\n        else -> \"default.workflow.nml\"\n",
        );
        let DeclarationKind::Block(block) = &file.declarations[0].kind else {
            panic!("block");
        };
        let BodyEntryKind::NestedBlock(nb) = &block.body.entries[0].kind else {
            panic!("nested block");
        };
        let lit: Vec<_> = nb
            .body
            .entries
            .iter()
            .filter_map(|e| match &e.kind {
                BodyEntryKind::Arm(a) => Some(&a.target),
                _ => None,
            })
            .collect();
        assert!(
            matches!(lit[0], ArmTarget::Literal { value, .. } if value == "admin.workflow.nml")
        );
        assert!(
            matches!(lit[1], ArmTarget::Literal { value, .. } if value == "default.workflow.nml")
        );

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

    /// RFC 0007 §6.2: inline arm targets (`-> Name:` + body) and string-keyed
    /// selectors (`"key" -> Target`).
    #[test]
    fn arms_lower_with_inline_target_and_string_selector() {
        use crate::ast::{ArmSelector, ArmTarget, BodyEntryKind, DeclarationKind};
        let file = cst_ast(
            "service App:\n    routing:\n        @role/admin -> adminLanding:\n            label = 4\n        \"plan\" -> \"upsell\"\n",
        );
        let DeclarationKind::Block(block) = &file.declarations[0].kind else {
            panic!("block");
        };
        let BodyEntryKind::NestedBlock(nb) = &block.body.entries[0].kind else {
            panic!("nested block");
        };
        let arms: Vec<_> = nb
            .body
            .entries
            .iter()
            .map(|e| match &e.kind {
                BodyEntryKind::Arm(a) => a,
                other => panic!("expected arm, got {other:?}"),
            })
            .collect();
        assert_eq!(arms.len(), 2);
        assert!(matches!(&arms[0].selector, ArmSelector::Role(r) if r == "@role/admin"));
        assert!(matches!(
            &arms[0].target,
            ArmTarget::Inline { name, body } if name.name == "adminLanding"
                && matches!(&body.entries[0].kind, BodyEntryKind::Property(p) if p.name.name == "label")
        ));
        assert!(matches!(&arms[1].selector, ArmSelector::Literal(k) if k == "plan"));
        assert!(matches!(
            &arms[1].target,
            ArmTarget::Literal { value, .. } if value == "upsell"
        ));
    }

    /// Error-recovery CST for `-> "name":` must still lower the target as
    /// `Literal`, never `Inline` (RFC 0007 §6.2).
    #[test]
    fn arms_lower_quoted_target_with_colon_is_literal_not_inline() {
        use crate::ast::{ArmTarget, BodyEntryKind, DeclarationKind};
        let parse = parse(
            "service Api:\n    routing:\n        @role/admin -> \"adminLanding\":\n            label = 4\n",
        );
        assert!(
            parse
                .errors()
                .iter()
                .any(|e| e.message().contains("inline block uses an unquoted name")),
            "parse should teach the quoted-colon mistake: {:?}",
            parse.errors()
        );
        let file = to_ast(&ast::Root::cast(parse.syntax()).unwrap());
        let DeclarationKind::Block(block) = &file.declarations[0].kind else {
            panic!("block");
        };
        let BodyEntryKind::NestedBlock(nb) = &block.body.entries[0].kind else {
            panic!("nested block");
        };
        let arm = nb
            .body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::Arm(a) => Some(a),
                _ => None,
            })
            .expect("arm");
        assert!(
            matches!(
                &arm.target,
                ArmTarget::Literal { value, .. } if value == "adminLanding"
            ),
            "quoted target must lower as Literal, not Inline: {:?}",
            arm.target
        );
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
        assert!(matches!(&variants[0], FieldTypeExpr::Named { name: n, .. } if n.name == "string"));
        let FieldTypeExpr::Arms { key, target } = &variants[1] else {
            panic!("expected an arm set, got {}", variants[1]);
        };
        assert!(matches!(key.as_ref(), FieldTypeExpr::Named { name: n, .. } if n.name == "role"));
        assert!(
            matches!(target.as_ref(), FieldTypeExpr::Named { name: n, .. } if n.name == "denial")
        );
        assert_eq!(
            fields[0].field_type.to_string(),
            "(string | (role -> denial))"
        );

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
    fn as_annotation_lowers_onto_the_body() {
        // RFC 0015: `as <Variant>` at a field entry AND a list element both lower
        // onto the target `Body`, so the one canonical resolver sees them.
        let file = cst_ast(
            "host H:\n    slot as modelB:\n        b = \"x\"\n    items:\n        - one as modelC:\n            c = \"y\"\n",
        );
        let DeclarationKind::Block(b) = &file.declarations[0].kind else {
            panic!("expected block")
        };
        let nested = |name: &str| {
            b.body.entries.iter().find_map(move |e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == name => Some(&nb.body),
                _ => None,
            })
        };
        assert_eq!(
            nested("slot")
                .unwrap()
                .type_annotation
                .as_ref()
                .map(|i| i.name.as_str()),
            Some("modelB"),
            "field-level annotation must lower onto the body"
        );
        let item_ann = nested("items")
            .unwrap()
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::ListItem(li) => match &li.kind {
                    ListItemKind::Named { body, .. } => {
                        body.type_annotation.as_ref().map(|i| i.name.clone())
                    }
                    _ => None,
                },
                _ => None,
            });
        assert_eq!(
            item_ann.as_deref(),
            Some("modelC"),
            "list-element annotation must lower onto the item body"
        );
    }

    /// F1 (interaction audit): `as` on a ROOT declaration header must be a loud
    /// parse error that keeps the body attached to the REAL declaration — never
    /// the silent split into a bodyless block plus a bogus `as`-keyword block
    /// that swallows the body.
    #[test]
    fn as_on_declaration_header_errors_without_swallowing_the_body() {
        // All three header positions: plain, after an `is` clause, array decl.
        for src in [
            "host H as modelB:\n    b = \"x\"\n",
            "host H is Base as modelB:\n    b = \"x\"\n",
        ] {
            let parsed = parse(src);
            assert!(
                !parsed.errors().is_empty(),
                "`as` on a declaration header must be a parse error: {src:?}"
            );
            let file = cst_ast(src);
            assert_eq!(
                file.declarations.len(),
                1,
                "no bogus extra declaration: {src:?}"
            );
            let DeclarationKind::Block(b) = &file.declarations[0].kind else {
                panic!("expected block")
            };
            assert_eq!(b.keyword.name, "host");
            assert!(
                b.body.entries.iter().any(|e| matches!(
                    &e.kind,
                    BodyEntryKind::Property(p) if p.name.name == "b"
                )),
                "the body must attach to the real declaration: {src:?}"
            );
        }
        // Round-10 F6/F7: repeated `as` clauses all consumed (body on the real
        // decl), and `is as` leaves `as` for the annotation rejection.
        for src in [
            "host H as x as y as z:\n    b = \"x\"\n",
            "host H is as modelB:\n    b = \"x\"\n",
        ] {
            let parsed = parse(src);
            assert!(!parsed.errors().is_empty(), "must be loud: {src:?}");
            let file = cst_ast(src);
            assert_eq!(
                file.declarations.len(),
                1,
                "no bogus extra declaration: {src:?}"
            );
            let DeclarationKind::Block(b) = &file.declarations[0].kind else {
                panic!("expected block")
            };
            assert_eq!(b.keyword.name, "host", "{src:?}");
            assert!(
                b.body.entries.iter().any(|e| matches!(
                    &e.kind,
                    BodyEntryKind::Property(p) if p.name.name == "b"
                )),
                "the body must attach to the real declaration: {src:?}"
            );
        }

        // Array decl: same guarantee, item attaches to the real declaration.
        let src = "[]mount m as modelB:\n    - one:\n        b = \"x\"\n";
        let parsed = parse(src);
        assert!(
            !parsed.errors().is_empty(),
            "`as` on an array-decl header must be a parse error"
        );
        let file = cst_ast(src);
        assert_eq!(file.declarations.len(), 1, "no bogus extra declaration");
        let DeclarationKind::Array(a) = &file.declarations[0].kind else {
            panic!("expected array decl")
        };
        assert_eq!(a.item_keyword.name, "mount");
        assert_eq!(
            a.body.items.len(),
            1,
            "the item must attach to the real declaration"
        );
    }

    #[test]
    fn malformed_as_recovers_without_panic_or_sibling_capture() {
        // `field as` (no variant) and `field as X` (no colon) must error, not
        // panic, and must NOT swallow the following (same-indent) sibling.
        for src in [
            "host H:\n    slot as\n    sibling = 1\n",
            "host H:\n    slot as modelB\n    sibling = 1\n",
        ] {
            let parsed = parse(src);
            assert!(
                !parsed.errors().is_empty(),
                "a malformed `as` must be a parse error: {src:?}"
            );
            let file = cst_ast(src);
            let DeclarationKind::Block(b) = &file.declarations[0].kind else {
                panic!("expected block")
            };
            assert!(
                b.body.entries.iter().any(|e| matches!(
                    &e.kind,
                    BodyEntryKind::Property(p) if p.name.name == "sibling"
                )),
                "the sibling entry must survive recovery, not be captured: {src:?}"
            );
        }
    }

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
