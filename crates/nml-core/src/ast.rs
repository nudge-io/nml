use crate::span::Span;
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
    /// The arm's target — a reference, string/path literal, or inline block
    /// ([`ArmTarget`]).
    pub target: ArmTarget,
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
    Literal {
        value: String,
        span: Span,
    },
    /// `-> Name:` followed by an indented body — an inline instance of `V`.
    Inline {
        name: Identifier,
        body: Body,
    },
}

impl ArmTarget {
    /// The target's source span (for diagnostics / trailing-comment anchors).
    pub fn span(&self) -> Span {
        match self {
            ArmTarget::Reference(id) => id.span,
            ArmTarget::Literal { span, .. } => *span,
            ArmTarget::Inline { name, .. } => name.span,
        }
    }
}

/// An [`Arm`] selector: a role reference, a string literal key, or the `else`
/// catch-all.
#[derive(Debug, Clone, Serialize, PartialEq)]
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
