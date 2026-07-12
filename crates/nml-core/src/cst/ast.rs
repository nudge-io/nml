//! Typed wrapper AST (RFC 0004 P3/P4): zero-cost accessor views over untyped
//! `SyntaxNode`s — the layer every consumer (validate / fmt / lsp / nudge / the
//! schema walkers) reads instead of touching `rowan` directly.
//!
//! Each wrapper is a newtype over a `SyntaxNode`; accessors return `Option`/
//! iterators because a CST may be incomplete — forcing callers to handle partial
//! trees is exactly what error tolerance requires. The wrappers are hand-written
//! (RFC 0004 §4.4): at this node count a codegen toolchain would be more weight
//! than the trivial child-by-kind lookups it would generate.

use crate::cst::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

/// A typed view over a `SyntaxNode` of a specific kind.
pub trait AstNode: Sized {
    fn cast(node: SyntaxNode) -> Option<Self>;
    fn syntax(&self) -> &SyntaxNode;
}

// ── shared accessor helpers ───────────────────────────────────────────────

/// First child node castable to `T`.
fn child<T: AstNode>(node: &SyntaxNode) -> Option<T> {
    node.children().find_map(T::cast)
}

/// All child nodes castable to `T`, in order. `T: 'static` always holds — every
/// `AstNode` wrapper owns its `SyntaxNode` and borrows nothing.
fn children<T: AstNode + 'static>(node: &SyntaxNode) -> impl Iterator<Item = T> + '_ {
    node.children().filter_map(T::cast)
}

/// First direct token of `kind` (trivia has distinct kinds, so a specific-kind
/// search skips it automatically).
fn token(node: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == kind)
}

/// The first `Ident` token *after* the contextual keyword `kw` (e.g. the
/// discriminator after `by`). Handles a discriminator that happens to share the
/// keyword's text, since `find` stops at the first match (the keyword).
fn ident_after_kw(node: &SyntaxNode, kw: &str) -> Option<SyntaxToken> {
    let mut idents = node
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == SyntaxKind::Ident);
    idents.by_ref().find(|t| t.text() == kw)?;
    idents.next()
}

/// Generates the newtype + `AstNode` impl for a single-kind node.
macro_rules! ast_node {
    ($(#[$m:meta])* $name:ident => $kind:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone)]
        pub struct $name(SyntaxNode);

        impl AstNode for $name {
            fn cast(node: SyntaxNode) -> Option<Self> {
                (node.kind() == SyntaxKind::$kind).then_some(Self(node))
            }
            fn syntax(&self) -> &SyntaxNode {
                &self.0
            }
        }
    };
}

// ── root & declarations ───────────────────────────────────────────────────

ast_node!(/// The parsed file. Root of the tree.
    Root => Root);

impl Root {
    pub fn decls(&self) -> impl Iterator<Item = Decl> + '_ {
        children(&self.0)
    }
}

/// Any top-level declaration.
#[derive(Debug, Clone)]
pub enum Decl {
    Block(BlockDecl),
    Array(ArrayDecl),
    Const(ConstDecl),
    Template(TemplateDecl),
    OneOf(OneOfDecl),
}

impl AstNode for Decl {
    fn cast(node: SyntaxNode) -> Option<Self> {
        Some(match node.kind() {
            SyntaxKind::BlockDecl => Decl::Block(BlockDecl(node)),
            SyntaxKind::ArrayDecl => Decl::Array(ArrayDecl(node)),
            SyntaxKind::ConstDecl => Decl::Const(ConstDecl(node)),
            SyntaxKind::TemplateDecl => Decl::Template(TemplateDecl(node)),
            SyntaxKind::OneOfDecl => Decl::OneOf(OneOfDecl(node)),
            _ => return None,
        })
    }
    fn syntax(&self) -> &SyntaxNode {
        match self {
            Decl::Block(d) => d.syntax(),
            Decl::Array(d) => d.syntax(),
            Decl::Const(d) => d.syntax(),
            Decl::Template(d) => d.syntax(),
            Decl::OneOf(d) => d.syntax(),
        }
    }
}

impl Decl {
    /// The declaration's [`Name`], regardless of kind.
    pub fn name(&self) -> Option<Name> {
        match self {
            Decl::Block(d) => d.name(),
            Decl::Array(d) => d.name(),
            Decl::Const(d) => d.name(),
            Decl::Template(d) => d.name(),
            Decl::OneOf(d) => d.name(),
        }
    }

    /// The leading own-line comment block attached to this declaration (RFC 0004
    /// §4.3 comment attachment), as documentation text with each `//` marker
    /// stripped. `None` when there is no leading comment. This is the payoff of
    /// correct comment attachment: a comment written above a declaration becomes
    /// its hover documentation.
    pub fn doc_comment(&self) -> Option<String> {
        leading_doc_comment(self.syntax())
    }
}

/// The leading own-line comment block attached to `node` (RFC 0004 §4.3
/// comment attachment), as documentation text with each `//` marker stripped.
/// Attachment places a leading comment *inside* the node it documents, so the
/// same walk serves declarations and array items alike.
fn leading_doc_comment(node: &SyntaxNode) -> Option<String> {
    let mut lines = Vec::new();
    for child in node.children_with_tokens() {
        match child.into_token() {
            Some(t) if t.kind() == SyntaxKind::Comment => {
                let raw = t.text();
                lines.push(raw.strip_prefix("//").unwrap_or(raw).trim().to_string());
            }
            // Whitespace/newlines between leading comments are skipped; the
            // first real token (or child node) ends the doc block.
            Some(t) if t.kind().is_trivia() => {}
            _ => break,
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

ast_node!(/// `keyword name (is …)? : body`
    BlockDecl => BlockDecl);

impl BlockDecl {
    /// The leading keyword (`service`, `model`, `enum`, …) — the first direct
    /// `Ident`; the declaration name lives in a [`Name`] node.
    pub fn keyword(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::Ident)
    }
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
    pub fn extends(&self) -> Option<Extends> {
        child(&self.0)
    }
    pub fn body(&self) -> Option<Body> {
        child(&self.0)
    }
}

ast_node!(/// `[] item_keyword name : body`
    ArrayDecl => ArrayDecl);

impl ArrayDecl {
    /// The element keyword (the first direct `Ident`, after the `[]`).
    pub fn item_keyword(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::Ident)
    }
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
    pub fn body(&self) -> Option<Body> {
        child(&self.0)
    }
}

ast_node!(/// `const name = value`
    ConstDecl => ConstDecl);

impl ConstDecl {
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
    pub fn value(&self) -> Option<ValueNode> {
        child(&self.0)
    }
}

ast_node!(/// `template name : value`
    TemplateDecl => TemplateDecl);

impl TemplateDecl {
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
    pub fn value(&self) -> Option<ValueNode> {
        child(&self.0)
    }
}

ast_node!(/// `oneof name by disc (as enum)? (= "default")? : arm+`
    OneOfDecl => OneOfDecl);

impl OneOfDecl {
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
    pub fn discriminator(&self) -> Option<SyntaxToken> {
        ident_after_kw(&self.0, "by")
    }
    pub fn enum_type(&self) -> Option<SyntaxToken> {
        ident_after_kw(&self.0, "as")
    }
    /// The default discriminator value (the only direct `String`; arm values
    /// live in [`OneOfArm`] nodes).
    pub fn default_value(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::String)
    }
    pub fn arms(&self) -> impl Iterator<Item = OneOfArm> + '_ {
        children(&self.0)
    }
}

ast_node!(/// `"value" -> Model`
    OneOfArm => OneOfArm);

impl OneOfArm {
    pub fn value(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::String)
    }
    pub fn model(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::Ident)
    }
}

ast_node!(/// `(@selector | else) -> Target`
    Arm => Arm);

impl Arm {
    /// The selector token — a `Role` (`@plan/Pro`) or the `else` keyword (an
    /// `Ident`), whichever comes first, before the arrow. The caller inspects
    /// its kind/text to tell a role selector from the `else` catch-all.
    pub fn selector(&self) -> Option<SyntaxToken> {
        self.0
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| matches!(t.kind(), SyntaxKind::Role | SyntaxKind::Ident))
    }
    /// The target identifier (the experience name) — the first `Ident` *after*
    /// the arrow, so it is never confused with an `else` selector's `Ident`.
    pub fn target(&self) -> Option<SyntaxToken> {
        let mut toks = self
            .0
            .children_with_tokens()
            .filter_map(|e| e.into_token());
        toks.by_ref()
            .find(|t| matches!(t.kind(), SyntaxKind::Arrow | SyntaxKind::FatArrow))?;
        toks.find(|t| t.kind() == SyntaxKind::Ident)
    }
}

ast_node!(/// A declaration/property name.
    Name => Name);

impl Name {
    pub fn ident(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::Ident)
    }
    pub fn text(&self) -> Option<String> {
        self.ident().map(|t| t.text().to_string())
    }
}

ast_node!(/// `is Parent (, Parent)*`
    Extends => Extends);

impl Extends {
    pub fn parents(&self) -> impl Iterator<Item = SyntaxToken> + '_ {
        // The `is` keyword is positionally the first `Ident`; the rest are
        // parents. Skipping by position (not text) is robust even to a parent
        // literally named `is`.
        self.0
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|t| t.kind() == SyntaxKind::Ident)
            .skip(1)
    }
}

// ── bodies & entries ──────────────────────────────────────────────────────

ast_node!(/// An indented block body.
    Body => Body);

impl Body {
    pub fn entries(&self) -> impl Iterator<Item = Entry> + '_ {
        children(&self.0)
    }
}

/// Any entry inside a [`Body`].
#[derive(Debug, Clone)]
pub enum Entry {
    Property(Property),
    NestedBlock(NestedBlock),
    Modifier(Modifier),
    SharedProperty(SharedProperty),
    ListItem(ListItem),
    FieldDef(FieldDef),
    Arm(Arm),
}

impl AstNode for Entry {
    fn cast(node: SyntaxNode) -> Option<Self> {
        Some(match node.kind() {
            SyntaxKind::Property => Entry::Property(Property(node)),
            SyntaxKind::NestedBlock => Entry::NestedBlock(NestedBlock(node)),
            SyntaxKind::Modifier => Entry::Modifier(Modifier(node)),
            SyntaxKind::SharedProperty => Entry::SharedProperty(SharedProperty(node)),
            SyntaxKind::ListItem => Entry::ListItem(ListItem(node)),
            SyntaxKind::FieldDef => Entry::FieldDef(FieldDef(node)),
            SyntaxKind::Arm => Entry::Arm(Arm(node)),
            _ => return None,
        })
    }
    fn syntax(&self) -> &SyntaxNode {
        match self {
            Entry::Property(e) => e.syntax(),
            Entry::NestedBlock(e) => e.syntax(),
            Entry::Modifier(e) => e.syntax(),
            Entry::SharedProperty(e) => e.syntax(),
            Entry::ListItem(e) => e.syntax(),
            Entry::FieldDef(e) => e.syntax(),
            Entry::Arm(e) => e.syntax(),
        }
    }
}

ast_node!(/// `name = value`
    Property => Property);

impl Property {
    pub fn name(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::Ident)
    }
    pub fn value(&self) -> Option<ValueNode> {
        child(&self.0)
    }
}

ast_node!(/// `name : body`
    NestedBlock => NestedBlock);

impl NestedBlock {
    pub fn name(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::Ident)
    }
    pub fn body(&self) -> Option<Body> {
        child(&self.0)
    }
}

ast_node!(/// `| name (= value | : list | type)`
    Modifier => Modifier);

impl Modifier {
    pub fn name(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::Ident)
    }
    /// The inline value form (`= value`).
    pub fn value(&self) -> Option<ValueNode> {
        child(&self.0)
    }
    /// The block/list form (`: …`).
    pub fn body(&self) -> Option<Body> {
        child(&self.0)
    }
    /// The type-annotation form (`name type`).
    pub fn type_expr(&self) -> Option<TypeExpr> {
        child(&self.0)
    }
    /// Whether a type-annotation modifier is optional (`name type?`).
    pub fn optional(&self) -> bool {
        token(&self.0, SyntaxKind::Question).is_some()
    }
}

ast_node!(/// `. name (: body | = value)`
    SharedProperty => SharedProperty);

impl SharedProperty {
    pub fn name(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::Ident)
    }
    pub fn value(&self) -> Option<ValueNode> {
        child(&self.0)
    }
    pub fn body(&self) -> Option<Body> {
        child(&self.0)
    }
}

ast_node!(/// `- ("string" | @role | Name (: body)?)`
    ListItem => ListItem);

impl ListItem {
    /// The item name (named or reference forms).
    pub fn name(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::Ident)
    }
    /// The leading own-line comment block documenting this item (RFC 0004
    /// §4.3) — a `- Name:` item documents exactly like a declaration, so an
    /// arm target's hover (RFC 0007 §4.1) surfaces the comment above the item.
    ///
    /// Attachment nuance: a comment above the body's FIRST item precedes the
    /// item's opening `Indent`, so it attaches to the enclosing `Body`, not
    /// the item — the fallback walks the immediately preceding siblings for
    /// that contiguous comment block (skipping whitespace and the zero-width
    /// `Indent`).
    pub fn doc_comment(&self) -> Option<String> {
        if let Some(doc) = leading_doc_comment(&self.0) {
            return Some(doc);
        }
        let mut lines = Vec::new();
        let mut cursor = self.0.prev_sibling_or_token();
        while let Some(element) = cursor {
            let next = element.prev_sibling_or_token();
            match element.into_token() {
                Some(t) if t.kind() == SyntaxKind::Comment => {
                    let raw = t.text();
                    lines.push(raw.strip_prefix("//").unwrap_or(raw).trim().to_string());
                }
                Some(t) if t.kind().is_trivia() || t.kind() == SyntaxKind::Indent => {}
                // Any other token or node ends the block (e.g. a previous
                // item — its trailing comments are its own, never this
                // item's docs).
                _ => break,
            }
            cursor = next;
        }
        lines.reverse();
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
    /// The nested body (named form `- Name: …`).
    pub fn body(&self) -> Option<Body> {
        child(&self.0)
    }
    /// The shorthand value (`- "string"`).
    pub fn value(&self) -> Option<ValueNode> {
        child(&self.0)
    }
    /// The role reference (`- @role/…`).
    pub fn role(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::Role)
    }
}

ast_node!(/// `name type[?] (= default)?`
    FieldDef => FieldDef);

impl FieldDef {
    pub fn name(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::Ident)
    }
    pub fn type_expr(&self) -> Option<TypeExpr> {
        child(&self.0)
    }
    pub fn optional(&self) -> bool {
        token(&self.0, SyntaxKind::Question).is_some()
    }
    /// Whether the field is the model's positional/scalar-shorthand field
    /// (`name type+`, RFC 0005 §16). The single point where the marker token is
    /// inspected — every other consumer reads this `bool`.
    pub fn shorthand(&self) -> bool {
        token(&self.0, SyntaxKind::Plus).is_some()
    }
    pub fn default(&self) -> Option<ValueNode> {
        child(&self.0)
    }
}

// ── type expressions ──────────────────────────────────────────────────────

ast_node!(/// `Name` | `[]TypeExpr` | `(TypeExpr (| TypeExpr)*)` | `(TypeExpr -> TypeExpr)`
    TypeExpr => TypeExpr);

/// The shape of a [`TypeExpr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeExprKind {
    Named,
    Array,
    Union,
    /// `(K -> V)` — a typed arm set (RFC 0007). `children()` yields exactly
    /// key then target.
    Arms,
}

impl TypeExpr {
    pub fn kind(&self) -> TypeExprKind {
        if token(&self.0, SyntaxKind::LBracket).is_some() {
            TypeExprKind::Array
        } else if token(&self.0, SyntaxKind::LParen).is_some() {
            // The arrow is a *direct* token of this node (a nested arm set's
            // arrow lives inside its own child node), so its presence
            // distinguishes `(K -> V)` from a union. `FatArrow` counts too:
            // the parser consumed it with RFC 0006 guidance but it remains in
            // the lossless tree.
            if token(&self.0, SyntaxKind::Arrow).is_some()
                || token(&self.0, SyntaxKind::FatArrow).is_some()
            {
                TypeExprKind::Arms
            } else {
                TypeExprKind::Union
            }
        } else {
            TypeExprKind::Named
        }
    }
    /// The type name (the `Named` form).
    pub fn name(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::Ident)
    }
    /// Nested type expressions (`[]T`'s element, or a union's variants).
    pub fn children(&self) -> impl Iterator<Item = TypeExpr> + '_ {
        children(&self.0)
    }
}

// ── values ────────────────────────────────────────────────────────────────

/// A value node (`Value`, `ArrayValue`, or `Fallback`). Decode into a semantic
/// [`SpannedValue`](crate::types::SpannedValue) via [`ValueNode::decode`].
#[derive(Debug, Clone)]
pub struct ValueNode(SyntaxNode);

impl AstNode for ValueNode {
    fn cast(node: SyntaxNode) -> Option<Self> {
        matches!(
            node.kind(),
            SyntaxKind::Value | SyntaxKind::ArrayValue | SyntaxKind::Fallback
        )
        .then_some(Self(node))
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

impl ValueNode {
    /// Interpret this value node into a semantic value (escapes, money, etc.).
    pub fn decode(&self) -> Result<crate::types::SpannedValue, crate::error::NmlError> {
        super::value::decode_value(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cst::parse;

    fn root(src: &str) -> Root {
        Root::cast(parse(src).syntax()).expect("root")
    }

    #[test]
    fn block_declaration_accessors() {
        let r = root("service App is Base, Mixin:\n    port = 8080\n    db:\n        x = 1\n");
        let Decl::Block(b) = r.decls().next().unwrap() else {
            panic!("expected block")
        };
        assert_eq!(b.keyword().unwrap().text(), "service");
        assert_eq!(b.name().unwrap().text().as_deref(), Some("App"));
        let parents: Vec<_> = b
            .extends()
            .unwrap()
            .parents()
            .map(|t| t.text().to_string())
            .collect();
        assert_eq!(parents, ["Base", "Mixin"]);

        let entries: Vec<_> = b.body().unwrap().entries().collect();
        assert!(matches!(entries[0], Entry::Property(_)));
        assert!(matches!(entries[1], Entry::NestedBlock(_)));
    }

    /// The wrappers surface declaration `(keyword, name)` and field-definition
    /// structure; properties (`port = 8080`) are not field definitions.
    #[test]
    fn wrappers_surface_declaration_and_field_structure() {
        let src = "model User:\n    name string\n    age number\n\ntrait Audit:\n    at string\n\nservice App is User:\n    port = 8080\n";

        let r = root(src);
        let mut blocks = Vec::new();
        let mut fields = Vec::new();
        for decl in r.decls() {
            if let Decl::Block(b) = decl {
                blocks.push((
                    b.keyword().map(|t| t.text().to_string()),
                    b.name().and_then(|n| n.text()),
                ));
                if let Some(body) = b.body() {
                    for e in body.entries() {
                        if let Entry::FieldDef(f) = e {
                            fields.push(f.name().map(|t| t.text().to_string()));
                        }
                    }
                }
            }
        }

        assert_eq!(
            blocks,
            vec![
                (Some("model".to_string()), Some("User".to_string())),
                (Some("trait".to_string()), Some("Audit".to_string())),
                (Some("service".to_string()), Some("App".to_string())),
            ]
        );
        // `port = 8080` is a property, not a field definition.
        assert_eq!(
            fields,
            vec![
                Some("name".to_string()),
                Some("age".to_string()),
                Some("at".to_string())
            ]
        );
    }

    #[test]
    fn extends_parents_robust_to_keyword_named_parent() {
        // A parent literally named `is` must still be returned (position-based,
        // not text-based).
        let r = root("service App is is, Mixin:\n    x = 1\n");
        let Decl::Block(b) = r.decls().next().unwrap() else {
            unreachable!()
        };
        let parents: Vec<_> = b
            .extends()
            .unwrap()
            .parents()
            .map(|t| t.text().to_string())
            .collect();
        assert_eq!(parents, ["is", "Mixin"]);
    }

    #[test]
    fn accessors_handle_partial_trees_without_panic() {
        // Malformed/incomplete input: every accessor must return None/empty, not
        // panic — the whole point of Option-returning wrappers.
        for src in ["service", "service App is\n", "model M:\n    f\n", "oneof x\n", "[]\n"] {
            let r = root(src);
            for decl in r.decls() {
                match decl {
                    Decl::Block(b) => {
                        let _ = (b.keyword(), b.name(), b.extends(), b.body());
                        if let Some(e) = b.extends() {
                            let _ = e.parents().count();
                        }
                        if let Some(body) = b.body() {
                            for entry in body.entries() {
                                let _ = entry.syntax().kind();
                            }
                        }
                    }
                    Decl::OneOf(o) => {
                        let _ = (o.name(), o.discriminator(), o.enum_type(), o.default_value());
                        let _ = o.arms().count();
                    }
                    Decl::Array(a) => {
                        let _ = (a.item_keyword(), a.name(), a.body());
                    }
                    Decl::Const(c) => {
                        let _ = (c.name(), c.value());
                    }
                    Decl::Template(t) => {
                        let _ = (t.name(), t.value());
                    }
                }
            }
        }
    }

    #[test]
    fn property_value_decodes_through_wrapper() {
        let r = root("service App:\n    port = 8080\n");
        let Decl::Block(b) = r.decls().next().unwrap() else {
            unreachable!()
        };
        let Entry::Property(p) = b.body().unwrap().entries().next().unwrap() else {
            unreachable!()
        };
        assert_eq!(p.name().unwrap().text(), "port");
        let v = p.value().unwrap().decode().unwrap().value;
        assert_eq!(v, crate::types::Value::Number(crate::types::Number::Int(8080)));
    }

    #[test]
    fn field_def_and_type_expr_accessors() {
        let r = root("model M:\n    name string\n    tier string?\n    tags []string\n    mode (a | b)\n");
        let Decl::Block(b) = r.decls().next().unwrap() else {
            unreachable!()
        };
        let fields: Vec<FieldDef> = b
            .body()
            .unwrap()
            .entries()
            .filter_map(|e| match e {
                Entry::FieldDef(f) => Some(f),
                _ => None,
            })
            .collect();
        assert_eq!(fields.len(), 4);

        assert_eq!(fields[0].name().unwrap().text(), "name");
        assert_eq!(fields[0].type_expr().unwrap().kind(), TypeExprKind::Named);
        assert!(!fields[0].optional());

        assert!(fields[1].optional());

        assert_eq!(fields[2].type_expr().unwrap().kind(), TypeExprKind::Array);
        // `[]string` → element type `string`.
        let elem = fields[2].type_expr().unwrap().children().next().unwrap();
        assert_eq!(elem.name().unwrap().text(), "string");

        let union = fields[3].type_expr().unwrap();
        assert_eq!(union.kind(), TypeExprKind::Union);
        assert_eq!(union.children().count(), 2);
    }

    #[test]
    fn oneof_accessors() {
        let r = root(
            "oneof email by provider as kind = \"log\":\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n",
        );
        let Decl::OneOf(o) = r.decls().next().unwrap() else {
            panic!("expected oneof")
        };
        assert_eq!(o.name().unwrap().text().as_deref(), Some("email"));
        assert_eq!(o.discriminator().unwrap().text(), "provider");
        assert_eq!(o.enum_type().unwrap().text(), "kind");
        assert_eq!(o.default_value().unwrap().text(), "\"log\"");
        let arms: Vec<_> = o.arms().collect();
        assert_eq!(arms.len(), 2);
        assert_eq!(arms[0].value().unwrap().text(), "\"log\"");
        assert_eq!(arms[0].model().unwrap().text(), "emailLog");
    }

    #[test]
    fn array_decl_modifier_shared_listitem_accessors() {
        let r = root(
            "[]mount mounts:\n    |allow = [@authenticated]\n    .region = \"us\"\n    - Main:\n        path = \"/\"\n    - \"sh\"\n",
        );
        let Decl::Array(a) = r.decls().next().unwrap() else {
            panic!("expected array decl")
        };
        assert_eq!(a.item_keyword().unwrap().text(), "mount");
        assert_eq!(a.name().unwrap().text().as_deref(), Some("mounts"));

        let entries: Vec<_> = a.body().unwrap().entries().collect();
        let Entry::Modifier(m) = &entries[0] else {
            panic!("expected modifier")
        };
        assert_eq!(m.name().unwrap().text(), "allow");
        assert!(m.value().is_some());

        let Entry::SharedProperty(sp) = &entries[1] else {
            panic!("expected shared property")
        };
        assert_eq!(sp.name().unwrap().text(), "region");

        let Entry::ListItem(named) = &entries[2] else {
            panic!("expected list item")
        };
        assert_eq!(named.name().unwrap().text(), "Main");
        assert!(named.body().is_some());

        let Entry::ListItem(shorthand) = &entries[3] else {
            panic!("expected list item")
        };
        assert!(shorthand.value().is_some());
    }
}
