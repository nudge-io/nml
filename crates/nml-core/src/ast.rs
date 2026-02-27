use crate::span::Span;
use crate::types::SpannedValue;
use serde::Serialize;

/// The type expression in a field definition (e.g. `string`, `[]route`).
#[derive(Debug, Clone, Serialize)]
pub enum FieldTypeExpr {
    Named(Identifier),
    Array(Identifier),
}

/// A field definition within a model/trait body: `name type[?] [= default]`.
#[derive(Debug, Clone, Serialize)]
pub struct FieldDefinition {
    pub name: Identifier,
    pub field_type: FieldTypeExpr,
    pub optional: bool,
    pub default_value: Option<SpannedValue>,
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
}

/// A block declaration like `service NudgeService:` or `model service:`.
#[derive(Debug, Clone, Serialize)]
pub struct BlockDecl {
    pub keyword: Identifier,
    pub name: Identifier,
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
#[derive(Debug, Clone, Serialize)]
pub struct Body {
    pub entries: Vec<BodyEntry>,
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
    /// A shared property: `.key: ...`
    SharedProperty(SharedProperty),
    /// A list item within a body (when the body is used inline in a service, etc.)
    ListItem(ListItem),
    /// A field definition in a model/trait: `name type[?] [= default]`
    FieldDefinition(FieldDefinition),
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
    },
}

/// A shared/inherited property: `.key: <body>`.
#[derive(Debug, Clone, Serialize)]
pub struct SharedProperty {
    pub name: Identifier,
    pub body: Body,
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
    /// `- Name: <body>`
    Named { name: Identifier, body: Body },
    /// `- "string value"` (shorthand)
    Shorthand(SpannedValue),
    /// `- ReferenceName`
    Reference(Identifier),
    /// `- @role/ref`
    RoleRef(String),
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
