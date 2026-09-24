use crate::span::{Span, ValueSpan};
use crate::types::SpannedValue;
use serde::Serialize;

/// One RFC 0018 facet: `min = 1` or (RFC 0017) `min = 5s`. The value is a
/// number or duration literal (`SpannedValue` holding `Value::Number` or
/// `Value::Duration`); an undecodable literal is DROPPED with a diagnostic
/// by both builders — never zero-recovered into a phantom bound.
#[derive(Debug, Clone, Serialize)]
pub struct FacetExpr {
    pub key: Identifier,
    pub value: SpannedValue,
    /// The whole `key = value` span.
    pub span: Span,
}

/// The type expression in a field definition (e.g. `string`, `[]route`).
#[derive(Debug, Clone, Serialize)]
pub enum FieldTypeExpr {
    /// A bare or faceted type name: `string`, `number(min = 1)`,
    /// `duration(min = 1s)`. Facets (RFC 0018) are empty for every name
    /// outside the faceted domains (`number`, `duration`) in a valid
    /// schema — the loader rejects them elsewhere with NML2058; the
    /// parse stays structured either way.
    Named {
        name: Identifier,
        facets: Vec<FacetExpr>,
    },
    Array(Box<FieldTypeExpr>),
    Union(Vec<FieldTypeExpr>),
    /// `(K -> V)` — a typed arm set (RFC 0007): the field's body is ordered,
    /// first-match [`Arm`]s whose keys conform to `K` and whose targets are
    /// `V`-typed. Reference targets are consumer-resolved, never
    /// existence-checked by nml (RFC 0007 §4.1); `else` is always a legal key.
    /// Not a map: ordering is significant and matching is the consumer's.
    Arms {
        key: Box<FieldTypeExpr>,
        target: Box<FieldTypeExpr>,
    },
    /// `set<T>` — an unordered, unique-element collection (RFC 0032). A bare
    /// union argument (`set<a | b>`) is the canonical spelling; the element
    /// here is then `Union`.
    Set(Box<FieldTypeExpr>),
}

/// Renders the type expression in NML source syntax: `string`, `[]route`,
/// `(step | []step)`, `(role -> denial)`.
impl std::fmt::Display for FieldTypeExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldTypeExpr::Named { name, facets } => {
                f.write_str(&name.name)?;
                if !facets.is_empty() {
                    f.write_str("(")?;
                    for (i, facet) in facets.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{} = ", facet.key.name)?;
                        match &facet.value.value {
                            crate::types::Value::Number(n) => write!(f, "{n}")?,
                            // A duration renders as authored (`250ms`,
                            // RFC 0017 Display) — the fmt fixed point
                            // depends on canonical value rendering.
                            crate::types::Value::Duration(d) => write!(f, "{d}")?,
                            other => write!(f, "{other:?}")?,
                        }
                    }
                    f.write_str(")")?;
                }
                Ok(())
            }
            FieldTypeExpr::Array(inner) => write!(f, "[]{inner}"),
            FieldTypeExpr::Union(variants) => {
                f.write_str("(")?;
                for (i, v) in variants.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" | ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str(")")
            }
            FieldTypeExpr::Arms { key, target } => write!(f, "({key} -> {target})"),
            FieldTypeExpr::Set(inner) => {
                // Canonical: bare union inside the angles (RFC 0032 Decision 4).
                f.write_str("set<")?;
                match inner.as_ref() {
                    FieldTypeExpr::Union(variants) => {
                        for (i, v) in variants.iter().enumerate() {
                            if i > 0 {
                                f.write_str(" | ")?;
                            }
                            write!(f, "{v}")?;
                        }
                    }
                    other => write!(f, "{other}")?,
                }
                f.write_str(">")
            }
        }
    }
}

/// A field definition within a model/trait body: `name type[?] [= default]`.
#[derive(Debug, Clone, Serialize)]
pub struct FieldDefinition {
    pub name: Identifier,
    pub field_type: FieldTypeExpr,
    pub optional: bool,
    /// The model's positional/scalar-shorthand field (`name type+`) — RFC 0005 §6, §16.
    pub shorthand: bool,
    pub default_value: Option<SpannedValue>,
    /// Trailing `#name`/`#name(value)` directives (RFC 0032), source order.
    pub directives: Vec<crate::types::Directive>,
}

/// A parsed NML file.
#[derive(Debug, Clone, Serialize)]
pub struct File {
    pub declarations: Vec<Declaration>,
}

/// A top-level declaration.
#[derive(Debug, Clone, Serialize)]
pub struct Declaration {
    pub kind: DeclarationKind,
    pub span: Span,
}

#[derive(Debug, Clone, Serialize)]
pub enum DeclarationKind {
    /// A block declaration: `keyword Name: ...`
    Block(BlockDecl),
    /// An array declaration: `[]keyword Name: ...`
    Array(ArrayDecl),
    /// A constant: `const Name = value`
    Const(ConstDecl),
    /// A template: `template Name: <string value>`
    Template(TemplateDecl),
    /// A discriminated union of models: `oneof Name by <field>: "v" -> Model ...`
    OneOf(OneOfDecl),
}

/// A discriminated-union declaration:
/// ```text
/// oneof email by provider:
///     "log"      -> emailLog
///     "postmark" -> emailPostmark
/// ```
/// Each arm binds a discriminator value to a variant model. The discriminator
/// field is owned by the union; an instance block carries it flat alongside the
/// selected variant's fields.
#[derive(Debug, Clone, Serialize)]
pub struct OneOfDecl {
    pub name: Identifier,
    /// The field whose value selects the variant (the `by` clause).
    pub discriminator: Identifier,
    /// Optional enum type for the discriminator (`by provider as providerKind`). When
    /// present, the arm keys must exactly cover the enum's variants (checked at load).
    pub discriminator_type: Option<Identifier>,
    /// Optional default discriminator value (`by provider = "log"`), injected when
    /// an instance omits the discriminator. Must name one of the `arms`.
    pub default_discriminator: Option<SpannedValue>,
    pub arms: Vec<OneOfArm>,
}

/// One `"value" -> ModelName` arm of a [`OneOfDecl`].
#[derive(Debug, Clone, Serialize)]
pub struct OneOfArm {
    /// Discriminator value that selects this variant.
    pub value: String,
    /// Span of the value literal (for diagnostics).
    pub value_span: Span,
    /// Variant model selected by `value`.
    pub model: Identifier,
}

/// A routing arm inside a plain block body: `(@selector | else) -> Target`
/// (the house arm idiom). Generic in the grammar; a schema restricts which
/// blocks accept arms — e.g. RFC 0018's `denial:` routes by which access door a
/// denied principal is missing.
#[derive(Debug, Clone, Serialize)]
pub struct Arm {
    pub selector: ArmSelector,
    /// Span of the selector token (for diagnostics).
    pub selector_span: Span,
    /// The bytes a content replacement of the selector substitutes — the
    /// window inside a quoted key's delimiters, minted at lowering from the
    /// token ([`crate::cst::string_content_window`]); `selector_span` for a
    /// role reference and for `else`. Skipped by `Serialize`: it is
    /// DERIVED from the token, and the AST's JSON shape is a wire
    /// surface. PRIVATE, like [`SpannedValue`]'s: an arm is minted by
    /// [`Arm::from_token`] (the lowering, from the selector's token) or
    /// [`Arm::new`] (a selector with no delimiters to strip) and read
    /// through [`Arm::selector_spans`], so no code can hold a quoted
    /// selector's window to its whole span.
    #[serde(skip)]
    selector_content: Span,
    /// The arm's target — a reference, string/path literal, or inline block
    /// ([`ArmTarget`]).
    pub target: ArmTarget,
}

impl Arm {
    /// An arm whose selector has no delimiters to strip — `else`, a role
    /// reference, a synthesized arm: the selector's content IS its span.
    pub fn new(selector: ArmSelector, selector_span: Span, target: ArmTarget) -> Self {
        Self {
            selector,
            selector_span,
            selector_content: selector_span,
            target,
        }
    }

    /// An arm minted from its selector token (the lowering's door): the
    /// content window is the token's ([`crate::cst::string_content_window`]).
    pub fn from_token(
        selector: ArmSelector,
        selector_span: Span,
        selector_content: Span,
        target: ArmTarget,
    ) -> Self {
        Self {
            selector,
            selector_span,
            selector_content,
            target,
        }
    }

    /// Where the selector sits: its whole span and the content window a
    /// replacement of it substitutes.
    pub fn selector_spans(&self) -> ValueSpan {
        ValueSpan::literal(self.selector_span, self.selector_content)
    }

    /// The same arm with another target — the selector and its spans
    /// carried over exactly (the resolvers and the layer merge rewrite an
    /// inline body, never a selector).
    pub fn with_target(&self, target: ArmTarget) -> Self {
        Self {
            selector: self.selector.clone(),
            selector_span: self.selector_span,
            selector_content: self.selector_content,
            target,
        }
    }
}

/// An [`Arm`]'s right-hand side (RFC 0007 §6): a **reference** to a declared
/// item (`-> ProUpsell`, consumer-resolved), a **literal** (`-> "workflows/
/// pro.workflow.nml"`) for flat routers whose targets are paths/URLs rather
/// than declared names, or an **inline instance** (`-> adminLanding:` + body)
/// validated structurally against `V`. Which form a schema accepts follows
/// from `V`: a reference needs a referenceable `V`, a literal needs a
/// scalar-capable `V`, an inline body needs a model/`oneof` `V` (checked by
/// nml-validate).
#[derive(Debug, Clone, Serialize)]
pub enum ArmTarget {
    Reference(Identifier),
    Literal(LiteralTarget),
    /// `-> Name:` followed by an indented body — an inline instance of `V`.
    Inline {
        name: Identifier,
        body: Body,
    },
}

/// A literal arm target (`-> "workflows/pro.workflow.nml"`): its value, its
/// span, and — private, minted from the token — the content window a
/// replacement substitutes. Serializes exactly as the struct variant it
/// replaced (`{"Literal": {"value": …, "span": …}}`): the AST's JSON shape
/// is a wire surface.
#[derive(Debug, Clone, Serialize)]
pub struct LiteralTarget {
    pub value: String,
    pub span: Span,
    /// The bytes a content replacement substitutes — the window inside
    /// the literal's delimiters, minted at lowering from the token
    /// ([`crate::cst::string_content_window`]). Skipped by `Serialize`.
    #[serde(skip)]
    content: Span,
}

impl LiteralTarget {
    /// A literal with no delimiters to strip: its content IS its span.
    pub fn new(value: String, span: Span) -> Self {
        Self {
            value,
            span,
            content: span,
        }
    }

    /// A literal minted from its token (the lowering's door).
    pub fn from_token(value: String, span: Span, content: Span) -> Self {
        Self {
            value,
            span,
            content,
        }
    }

    /// Where the literal sits: its whole span and the content window.
    pub fn spans(&self) -> ValueSpan {
        ValueSpan::literal(self.span, self.content)
    }
}

impl ArmTarget {
    /// The target's source span (for diagnostics / trailing-comment anchors).
    pub fn span(&self) -> Span {
        match self {
            ArmTarget::Reference(id) => id.span,
            ArmTarget::Literal(t) => t.span,
            ArmTarget::Inline { name, .. } => name.span,
        }
    }
}

/// An [`Arm`] selector: a role reference, a string literal key, or the `else`
/// catch-all.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
pub enum ArmSelector {
    /// A selector token, e.g. `@plan/Pro` — stored verbatim (with the leading
    /// `@`), matching [`ListItemKind::Role`]; the consumer parses its shape.
    Role(String),
    /// A string-literal key, e.g. `"plan"` (RFC 0007 §6) for `(string -> V)`
    /// arm sets — decoded like a `oneof` arm value.
    Literal(String),
    /// The `else` catch-all (a contextual keyword, never a role).
    Else,
}

/// A constant declaration: `const Name = value`.
#[derive(Debug, Clone, Serialize)]
pub struct ConstDecl {
    pub name: Identifier,
    pub value: SpannedValue,
}

/// A template declaration: `template Name:` followed by a string value.
#[derive(Debug, Clone, Serialize)]
pub struct TemplateDecl {
    pub name: Identifier,
    pub value: SpannedValue,
}

/// A block declaration like `service MyService:` or `model plan is role:`.
#[derive(Debug, Clone, Serialize)]
pub struct BlockDecl {
    pub keyword: Identifier,
    pub name: Identifier,
    pub extends: Vec<Identifier>,
    /// RFC 0019: the `uses` clause's layer refs, in authored order (empty
    /// when the declaration has no clause).
    pub uses: Vec<Identifier>,
    /// The clause's own content span (the `uses` keyword through the last
    /// ref) — the structural deletion target for NML2062's fix.
    /// Invariant: `uses_span.is_some() == !uses.is_empty()`.
    pub uses_span: Option<Span>,
    pub body: Body,
}

/// An array declaration like `[]resource registrationResources:`.
#[derive(Debug, Clone, Serialize)]
pub struct ArrayDecl {
    pub item_keyword: Identifier,
    pub name: Identifier,
    pub body: ArrayBody,
}

/// The body of a block declaration.
///
/// `type_annotation` carries an RFC 0015 nominal type annotation (`field as
/// <Variant>:`) when the body is the content of an explicitly-typed union
/// instance. It rides on the `Body` — the single input
/// [`SchemaIndex::resolve_type_in_body`](crate::SchemaIndex::resolve_type_in_body)
/// already takes — so **variant selection** stays centralized in that one
/// resolver (the validator, defaulter, identity walk, LSP, and differ all select
/// through it, with no per-consumer threading). The deserializer and formatter
/// read this field directly for their own concerns (a synthesized serde tag; the
/// re-emitted `as` text); they don't resolve variants, so they can't diverge from
/// the resolver, but the carrier — not a single call — is what keeps every reader
/// consistent. `None` for every un-annotated body (the overwhelming majority);
/// only the lowering of an `as`-annotated entry sets it.
#[derive(Debug, Clone, Serialize)]
pub struct Body {
    pub entries: Vec<BodyEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_annotation: Option<Identifier>,
}

impl Body {
    /// A **fresh** body with no nominal type annotation — for genuinely new
    /// bodies only (empty seeds, synthesized instances, array-decl inlining).
    /// `const` so it serves `const` seeds as well as runtime construction.
    ///
    /// Deliberately NOT named `new`: a body derived FROM an existing body must
    /// use [`with_entries`](Self::with_entries) instead — a fresh construction
    /// there would silently strip the RFC 0015 annotation, changing which union
    /// variant every downstream consumer resolves (a real found-in-review bug).
    /// The name makes the intent — "this body is new, not derived" — explicit
    /// at every call site, so the attractive-default footgun (`new`) does not
    /// exist.
    pub const fn fresh(entries: Vec<BodyEntry>) -> Self {
        Self {
            entries,
            type_annotation: None,
        }
    }

    /// Rebuild this body with transformed entries, **preserving** its nominal
    /// type annotation (RFC 0015). Every 1:1 body transform — value resolution,
    /// default injection, identity materialization, shared-property merging,
    /// reference inlining — must rebuild through this, so `slot as modelB:`
    /// still names `modelB` when the transformed body reaches the defaulter's
    /// variant selection and the deserializer's synthesized tag. Guarded by the
    /// full-pipeline test `annotated_union_survives_the_full_pipeline`.
    pub fn with_entries(&self, entries: Vec<BodyEntry>) -> Self {
        Self {
            entries,
            type_annotation: self.type_annotation.clone(),
        }
    }
}

/// An entry within a block body.
#[derive(Debug, Clone, Serialize)]
pub struct BodyEntry {
    pub kind: BodyEntryKind,
    pub span: Span,
}

#[derive(Debug, Clone, Serialize)]
pub enum BodyEntryKind {
    /// A property: `key = value`
    Property(Property),
    /// A nested block: `key: ...`
    NestedBlock(NestedBlock),
    /// An access control modifier: `|allow = [...]` or `|allow: ...`
    Modifier(Modifier),
    /// A shared property: `.key: ...` or `.key = value`
    SharedProperty(SharedProperty),
    /// A list item within a body (when the body is used inline in a service, etc.)
    ListItem(ListItem),
    /// A field definition in a model/trait: `name type[?] [= default]`
    FieldDefinition(FieldDefinition),
    /// A routing arm: `(@selector | else) -> Target` (house arm idiom).
    Arm(Arm),
}

/// A key-value property: `key = value`.
#[derive(Debug, Clone, Serialize)]
pub struct Property {
    pub name: Identifier,
    pub value: SpannedValue,
}

/// A nested block: `key: <indented body>`.
#[derive(Debug, Clone, Serialize)]
pub struct NestedBlock {
    pub name: Identifier,
    pub body: Body,
}

/// An access control modifier: `|key = value` or `|key: <list>`.
#[derive(Debug, Clone, Serialize)]
pub struct Modifier {
    pub name: Identifier,
    pub value: ModifierValue,
}

#[derive(Debug, Clone, Serialize)]
pub enum ModifierValue {
    Inline(SpannedValue),
    Block(Vec<ListItem>),
    TypeAnnotation {
        field_type: FieldTypeExpr,
        optional: bool,
        /// Trailing `#name`/`#name(value)` directives (RFC 0032), source order.
        directives: Vec<crate::types::Directive>,
    },
}

/// Payload for a shared property after `.name`.
#[derive(Debug, Clone, Serialize)]
pub enum SharedPropertyKind {
    /// `.name: <indented body>` — merged into each named list item as a nested block.
    Block(Body),
    /// `.name = <value>` — merged into each named list item as a property (item wins on name clash).
    Scalar(SpannedValue),
}

/// A shared/inherited default for named list items: `.key: ...` or `.key = value`.
#[derive(Debug, Clone, Serialize)]
pub struct SharedProperty {
    pub name: Identifier,
    pub kind: SharedPropertyKind,
}

/// The body of an array declaration.
#[derive(Debug, Clone, Serialize)]
pub struct ArrayBody {
    pub modifiers: Vec<Modifier>,
    pub shared_properties: Vec<SharedProperty>,
    pub properties: Vec<Property>,
    pub items: Vec<ListItem>,
}

/// A list item: `- Name: ...` or `- "value"` or `- RefName`.
#[derive(Debug, Clone, Serialize)]
pub struct ListItem {
    pub kind: ListItemKind,
    pub span: Span,
}

#[derive(Debug, Clone, Serialize)]
pub enum ListItemKind {
    /// `- Name: <body>` — an ident-keyed inline definition (always has a body).
    Named { name: Identifier, body: Body },
    /// A scalar-keyed inline definition: `- "/api"` (no body) or, with a body,
    /// `- "/api":` followed by an indented block. The scalar fills the element
    /// model's shorthand (`!`) field; the optional body fills the rest. The body is
    /// genuinely optional (unlike `Named`), so it is modeled as `Option<Body>`.
    Shorthand {
        value: SpannedValue,
        body: Option<Body>,
    },
    /// `- ReferenceName`
    Reference(Identifier),
    /// `- @role/ref`
    Role(String),
}

/// An identifier with its source span.
#[derive(Debug, Clone, Serialize)]
pub struct Identifier {
    pub name: String,
    pub span: Span,
}

impl Identifier {
    pub fn new(name: impl Into<String>, span: Span) -> Self {
        Self {
            name: name.into(),
            span,
        }
    }
}

/// Every span the lowered tree carries, in tree order, each named by the
/// field that holds it — the one enumeration the content-span pin and the
/// `document` fuzz target walk, so a span field added to the AST is a span
/// field added here (the fuzz target reads the same list).
pub fn for_each_span(file: &File, f: &mut impl FnMut(crate::span::SpanSite)) {
    for decl in &file.declarations {
        site(f, "Declaration", decl.span);
        match &decl.kind {
            DeclarationKind::Block(b) => {
                ident(f, "BlockDecl.keyword", &b.keyword);
                ident(f, "BlockDecl.name", &b.name);
                for e in &b.extends {
                    ident(f, "BlockDecl.extends", e);
                }
                for u in &b.uses {
                    ident(f, "BlockDecl.uses", u);
                }
                if let Some(s) = b.uses_span {
                    site(f, "BlockDecl.uses_span", s);
                }
                body_spans(f, &b.body);
            }
            DeclarationKind::Array(a) => {
                ident(f, "ArrayDecl.item_keyword", &a.item_keyword);
                ident(f, "ArrayDecl.name", &a.name);
                for m in &a.body.modifiers {
                    modifier_spans(f, m);
                }
                for s in &a.body.shared_properties {
                    shared_spans(f, s);
                }
                for p in &a.body.properties {
                    ident(f, "Property.name", &p.name);
                    value_spans(f, "Property.value", &p.value);
                }
                for i in &a.body.items {
                    item_spans(f, i);
                }
            }
            DeclarationKind::Const(c) => {
                ident(f, "ConstDecl.name", &c.name);
                value_spans(f, "ConstDecl.value", &c.value);
            }
            DeclarationKind::Template(t) => {
                ident(f, "TemplateDecl.name", &t.name);
                value_spans(f, "TemplateDecl.value", &t.value);
            }
            DeclarationKind::OneOf(o) => {
                ident(f, "OneOfDecl.name", &o.name);
                ident(f, "OneOfDecl.discriminator", &o.discriminator);
                if let Some(t) = &o.discriminator_type {
                    ident(f, "OneOfDecl.discriminator_type", t);
                }
                if let Some(v) = &o.default_discriminator {
                    value_spans(f, "OneOfDecl.default_discriminator", v);
                }
                for arm in &o.arms {
                    site(f, "OneOfArm.value_span", arm.value_span);
                    ident(f, "OneOfArm.model", &arm.model);
                }
            }
        }
    }
}

fn site(f: &mut impl FnMut(crate::span::SpanSite), kind: &'static str, span: Span) {
    f(crate::span::SpanSite::aligned(kind, span));
}

/// A carrier's whole span and the window inside it, as two sites — the ONE
/// place that decides which is which. The window is a CONTENT WINDOW (what
/// a machine-applicable replacement splices) exactly when it differs from
/// the whole span, which is what a quoted literal's stripped delimiters
/// leave behind; for everything else — an unquoted arm key, a path, a
/// number — the content IS the whole span, there are no delimiters to stay
/// inside of, and the site is a whole span like any other. Read from the
/// two spans the carrier holds, never from the site's name.
fn whole_and_window(
    f: &mut impl FnMut(crate::span::SpanSite),
    whole_kind: &'static str,
    window_kind: &'static str,
    whole: Span,
    content: Span,
) {
    site(f, whole_kind, whole);
    if content == whole {
        site(f, window_kind, content);
    } else {
        f(crate::span::SpanSite::window(window_kind, content, whole));
    }
}

fn ident(f: &mut impl FnMut(crate::span::SpanSite), kind: &'static str, id: &Identifier) {
    site(f, kind, id.span);
}

/// A value's span and every span nested in it (template expressions, array
/// items, fallback arms).
pub(crate) fn value_spans(
    f: &mut impl FnMut(crate::span::SpanSite),
    kind: &'static str,
    v: &SpannedValue,
) {
    let spans = v.spans();
    whole_and_window(f, kind, "SpannedValue.content", spans.whole, spans.content);
    match &v.value {
        crate::types::Value::TemplateString(segments) => {
            for s in segments {
                if let crate::types::TemplateSegment::Expression { span, .. } = s {
                    // Inside the string token that the value's span is.
                    f(crate::span::SpanSite::expression(
                        "TemplateSegment::Expression",
                        *span,
                        v.span,
                    ));
                }
            }
        }
        crate::types::Value::Array(items) => {
            for item in items {
                value_spans(f, "Value::Array item", item);
            }
        }
        crate::types::Value::Fallback(a, b) => {
            value_spans(f, "Value::Fallback lhs", a);
            value_spans(f, "Value::Fallback rhs", b);
        }
        _ => {}
    }
}

/// A directive's whole span and its argument's.
pub(crate) fn directive_spans(
    f: &mut impl FnMut(crate::span::SpanSite),
    d: &crate::types::Directive,
) {
    site(f, "Directive", d.span);
    if let Some(arg) = &d.arg {
        value_spans(f, "Directive.arg", arg);
    }
}

fn type_expr_spans(f: &mut impl FnMut(crate::span::SpanSite), t: &FieldTypeExpr) {
    match t {
        FieldTypeExpr::Named { name, facets } => {
            ident(f, "FieldTypeExpr::Named.name", name);
            for facet in facets {
                site(f, "FacetExpr", facet.span);
                ident(f, "FacetExpr.key", &facet.key);
                value_spans(f, "FacetExpr.value", &facet.value);
            }
        }
        FieldTypeExpr::Array(inner) | FieldTypeExpr::Set(inner) => type_expr_spans(f, inner),
        FieldTypeExpr::Union(variants) => {
            for v in variants {
                type_expr_spans(f, v);
            }
        }
        FieldTypeExpr::Arms { key, target } => {
            type_expr_spans(f, key);
            type_expr_spans(f, target);
        }
    }
}

fn body_spans(f: &mut impl FnMut(crate::span::SpanSite), b: &Body) {
    if let Some(t) = &b.type_annotation {
        ident(f, "Body.type_annotation", t);
    }
    for e in &b.entries {
        site(f, "BodyEntry", e.span);
        match &e.kind {
            BodyEntryKind::Property(p) => {
                ident(f, "Property.name", &p.name);
                value_spans(f, "Property.value", &p.value);
            }
            BodyEntryKind::NestedBlock(n) => {
                ident(f, "NestedBlock.name", &n.name);
                body_spans(f, &n.body);
            }
            BodyEntryKind::Modifier(m) => modifier_spans(f, m),
            BodyEntryKind::SharedProperty(s) => shared_spans(f, s),
            BodyEntryKind::ListItem(l) => item_spans(f, l),
            BodyEntryKind::FieldDefinition(fd) => {
                ident(f, "FieldDefinition.name", &fd.name);
                type_expr_spans(f, &fd.field_type);
                if let Some(d) = &fd.default_value {
                    value_spans(f, "FieldDefinition.default_value", d);
                }
                for d in &fd.directives {
                    directive_spans(f, d);
                }
            }
            BodyEntryKind::Arm(a) => {
                whole_and_window(
                    f,
                    "Arm.selector_span",
                    "Arm.selector_content",
                    a.selector_span,
                    a.selector_content,
                );
                match &a.target {
                    ArmTarget::Reference(id) => ident(f, "ArmTarget::Reference", id),
                    ArmTarget::Literal(t) => {
                        whole_and_window(
                            f,
                            "ArmTarget::Literal",
                            "ArmTarget::Literal.content",
                            t.span,
                            t.content,
                        );
                    }
                    ArmTarget::Inline { name, body } => {
                        ident(f, "ArmTarget::Inline.name", name);
                        body_spans(f, body);
                    }
                }
            }
        }
    }
}

fn modifier_spans(f: &mut impl FnMut(crate::span::SpanSite), m: &Modifier) {
    ident(f, "Modifier.name", &m.name);
    match &m.value {
        ModifierValue::Inline(v) => value_spans(f, "Modifier.inline", v),
        ModifierValue::Block(items) => {
            for i in items {
                item_spans(f, i);
            }
        }
        ModifierValue::TypeAnnotation {
            field_type,
            directives,
            ..
        } => {
            type_expr_spans(f, field_type);
            for d in directives {
                directive_spans(f, d);
            }
        }
    }
}

fn shared_spans(f: &mut impl FnMut(crate::span::SpanSite), s: &SharedProperty) {
    ident(f, "SharedProperty.name", &s.name);
    match &s.kind {
        SharedPropertyKind::Block(b) => body_spans(f, b),
        SharedPropertyKind::Scalar(v) => value_spans(f, "SharedProperty.scalar", v),
    }
}

fn item_spans(f: &mut impl FnMut(crate::span::SpanSite), l: &ListItem) {
    site(f, "ListItem", l.span);
    match &l.kind {
        ListItemKind::Named { name, body } => {
            ident(f, "ListItem::Named.name", name);
            body_spans(f, body);
        }
        ListItemKind::Shorthand { value, body } => {
            value_spans(f, "ListItem::Shorthand.value", value);
            if let Some(b) = body {
                body_spans(f, b);
            }
        }
        ListItemKind::Reference(id) => ident(f, "ListItem::Reference", id),
        ListItemKind::Role(_) => {}
    }
}
