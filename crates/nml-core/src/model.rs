use crate::span::Span;
use crate::types::{PrimitiveType, SpannedValue};
use serde::Serialize;

/// What a `ModelDef` declares (RFC 0011). A trait is structurally a model —
/// same fields, defaults, directives, `extends` — distinguished only where
/// the distinction is real: a trait can compose (`is`) but never be
/// instantiated as a block, referenced as a field type, or targeted by a
/// `oneof` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    Model,
    Trait,
}

impl ModelKind {
    /// The declaration keyword, for diagnostics ("model" / "trait").
    pub fn label(self) -> &'static str {
        match self {
            ModelKind::Model => "model",
            ModelKind::Trait => "trait",
        }
    }
}

/// One `is` target: the mixin's name plus the source span of exactly that
/// token, so composition diagnostics point — and machine-applicable
/// did-you-mean fixes rewrite — the target itself, not the whole
/// declaration (RFC 0011).
#[derive(Debug, Clone, Serialize)]
pub struct MixinRef {
    pub name: String,
    pub span: Span,
}

impl MixinRef {
    /// A span-less reference for synthesized schemas and tests. Extraction
    /// always builds real spans; only construct span-less refs where no
    /// source exists to point at.
    pub fn synthetic(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            span: Span::new(0, 0),
        }
    }
}

/// A model definition parsed from `model name:` / `trait name:` (and their
/// `… is parent:` forms). `kind` separates instantiable models from
/// composition-only traits (RFC 0011).
#[derive(Debug, Clone, Serialize)]
pub struct ModelDef {
    pub name: String,
    pub kind: ModelKind,
    pub extends: Vec<MixinRef>,
    pub fields: Vec<FieldDef>,
    /// The schema source (file name) that declared this definition, stamped
    /// by the loader when composing a multi-source set. Definition-anchored
    /// findings copy it so they render `file:line:col` instead of a raw
    /// byte span; `None` for single-source extraction and synthesized roots.
    pub source: Option<String>,
    pub span: Span,
}

impl ModelDef {
    /// Whether this definition is a composition-only trait (RFC 0011).
    pub fn is_trait(&self) -> bool {
        self.kind == ModelKind::Trait
    }
}

/// An enum definition parsed from `enum name:`.
#[derive(Debug, Clone, Serialize)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<String>,
    /// Declaring schema source — see [`ModelDef::source`].
    pub source: Option<String>,
    pub span: Span,
}

/// A discriminated-union definition extracted from `oneof Name by <field>:`.
///
/// Selects one of several variant models by the value of a discriminator
/// field. Validation dispatches an instance to the variant model named by the
/// discriminator's value.
#[derive(Debug, Clone, Serialize)]
pub struct OneOfDef {
    pub name: String,
    /// Field whose value selects the variant.
    pub discriminator: String,
    /// Optional enum type for the discriminator. When present, the arm keys must
    /// exactly cover the enum's variants (enforced at schema load).
    pub discriminator_type: Option<String>,
    /// Default discriminator value, injected when an instance omits it. Always one
    /// of the `variants`' keys (enforced at schema load).
    pub default_discriminator: Option<String>,
    /// `(discriminator_value, variant_model_name)` pairs, in source order.
    pub variants: Vec<(String, String)>,
    /// Declaring schema source — see [`ModelDef::source`].
    pub source: Option<String>,
    pub span: Span,
}

/// A field definition within a model.
#[derive(Debug, Clone, Serialize)]
pub struct FieldDef {
    pub name: String,
    pub field_type: FieldType,
    pub optional: bool,
    /// The model's positional/scalar-shorthand field (`name type+`, RFC 0005
    /// §16): the one field a bare scalar list item fills. At most one per model
    /// (enforced at schema load).
    pub shorthand: bool,
    /// The declared default, retaining its parsed type and source span. `None`
    /// when the field has no `= value`. The span points at the default literal so
    /// type-check diagnostics can locate it precisely.
    pub default_value: Option<SpannedValue>,
    /// Trailing `#name`/`#name(value)` directives (RFC 0032), source order.
    /// Opaque metadata — consumers interpret (see [`crate::types::Directive`]).
    pub directives: Vec<crate::types::Directive>,
    /// The leading own-line comment block documenting the field (RFC 0004 §4.3
    /// comment attachment), `//` markers stripped, lines joined. Presentation
    /// metadata for tooling (hover, completion) ONLY: it must never influence
    /// validation or RFC 0032 reload/diff semantics — `FieldDef` deliberately
    /// derives no `PartialEq`, and semantic comparison happens on `Value`s
    /// (`Value::semantic_eq`), never on `FieldDef`s wholesale.
    pub doc: Option<String>,
    pub span: Span,
}

/// One RFC 0018 numeric bound (`min`/`max`, inclusive or exclusive).
#[derive(Debug, Clone, Serialize)]
pub struct FacetBound {
    pub value: crate::types::Number,
    pub exclusive: bool,
    /// The facet's `key = value` span in the schema source. Config-side
    /// violation MESSAGES carry the facet's authored spelling (schema
    /// and config are usually different files, so a same-file related
    /// span would point nowhere); the span serves definition-side
    /// diagnostics.
    pub span: Span,
}

/// An RFC 0018 `multipleOf` facet.
#[derive(Debug, Clone, Serialize)]
pub struct FacetMultiple {
    pub value: crate::types::Number,
    pub span: Span,
}

/// RFC 0018 numeric facets on a `number` field. Empty (`NONE`) for
/// every non-number primitive — the loader rejects misplaced facets
/// (NML2058) before enforcement ever consults them.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NumberFacets {
    pub min: Option<FacetBound>,
    pub max: Option<FacetBound>,
    pub multiple_of: Option<FacetMultiple>,
}

impl NumberFacets {
    pub const NONE: NumberFacets = NumberFacets {
        min: None,
        max: None,
        multiple_of: None,
    };
    pub fn is_none(&self) -> bool {
        self.min.is_none() && self.max.is_none() && self.multiple_of.is_none()
    }
}

/// The type of a field.
#[derive(Debug, Clone, Serialize)]
pub enum FieldType {
    /// A primitive, optionally carrying RFC 0018 facets (only ever
    /// non-empty when `ty` is `Number` in a loaded schema). A struct
    /// variant deliberately: every pre-facet `Primitive(...)` pattern
    /// breaks at compile time, so each match site is reviewed rather
    /// than silently bypassed.
    Primitive {
        ty: PrimitiveType,
        facets: NumberFacets,
    },
    List(Box<FieldType>),
    ModelRef(String),
    /// A typed modifier field (`|allow []string?`); the inner type is the
    /// declared type of the modifier's value.
    Modifier(Box<FieldType>),
    Union(Vec<FieldType>),
    /// `(K -> V)` — a typed arm set (RFC 0007): the field's body is ordered,
    /// first-match `(@selector | else) -> Target` arms. `key` types the
    /// selectors (`role`, `string`, or an enum; `else` is always legal);
    /// `target` types the arm targets — completion/intent for reference
    /// targets (consumer-resolved, never existence-checked; RFC 0007 §4.1),
    /// full validation for inline-block targets.
    Arms {
        key: Box<FieldType>,
        target: Box<FieldType>,
    },
    /// `set<T>` — an unordered, **unique**-element collection (RFC 0032).
    /// Duplicate elements are a load-time validation error (value-level
    /// identity: for a union element type, the admitting arm is irrelevant).
    /// Unlike `List`, element order never carries meaning — diffs are
    /// order-insensitive (`SetDelta`), and authored order is preserved in
    /// source but semantically inert.
    Set(Box<FieldType>),
}

impl FieldType {
    /// The union variants of this type, transparently unwrapping a modifier
    /// wrapper (`|field (A | B)`); `None` when the type is not — even beneath a
    /// modifier — a union. The single "is this (a modifier around) a union?"
    /// test (RFC 0015): the validator's annotation/D2 gate, the defaulter's and
    /// identity walk's union branches, and LSP `as`-completion all dispatch
    /// through it, so a modifier-wrapped union is never second-class in one
    /// consumer and first-class in another.
    pub fn union_variants(&self) -> Option<&[FieldType]> {
        match self {
            FieldType::Union(v) => Some(v),
            FieldType::Modifier(inner) => inner.union_variants(),
            _ => None,
        }
    }
}

/// Renders the type in NML source syntax: `[]string`, `(step | []step)`,
/// `[](string | number)`.
///
/// A modifier's type displays as its declared inner type -- the `|` sigil
/// belongs to the field *name* (`|allow []string`), not the type.
impl std::fmt::Display for FieldType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldType::Primitive { ty: p, facets } => {
                f.write_str(p.as_str())?;
                // RFC 0018: the faceted type IS the contract — hover and
                // every model-side rendering show it in canonical form.
                if !facets.is_none() {
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(b) = &facets.min {
                        parts.push(format!(
                            "{} = {}",
                            if b.exclusive { "exclusiveMin" } else { "min" },
                            b.value
                        ));
                    }
                    if let Some(b) = &facets.max {
                        parts.push(format!(
                            "{} = {}",
                            if b.exclusive { "exclusiveMax" } else { "max" },
                            b.value
                        ));
                    }
                    if let Some(m) = &facets.multiple_of {
                        parts.push(format!("multipleOf = {}", m.value));
                    }
                    write!(f, "({})", parts.join(", "))?;
                }
                Ok(())
            }
            FieldType::List(inner) => write!(f, "[]{inner}"),
            FieldType::ModelRef(name) => f.write_str(name),
            FieldType::Modifier(inner) => write!(f, "{inner}"),
            FieldType::Union(variants) => {
                f.write_str("(")?;
                for (i, v) in variants.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" | ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str(")")
            }
            FieldType::Arms { key, target } => write!(f, "({key} -> {target})"),
            FieldType::Set(inner) => {
                // Canonical form: bare union inside the angles (`set<a | b>`)
                // — the angles already bound it, so the union's grouping
                // parens would be redundant (RFC 0032 Decision 4).
                f.write_str("set<")?;
                match inner.as_ref() {
                    FieldType::Union(variants) => {
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
