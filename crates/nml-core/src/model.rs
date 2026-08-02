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

/// The value domain a facet list ranges over: `number` (RFC 0018) or
/// `duration` (RFC 0017 literals under the same facet grammar). Bounds
/// compare with the domain's own EXACT ordering — `Number`'s numeric
/// `Ord`, `Duration`'s `total_nanos` — never through floats.
pub trait FacetDomain: Ord + Clone + std::fmt::Display + Serialize {
    /// Exact divisibility for `multipleOf`. Implementations must be
    /// total (a zero/degenerate modulus answers `false`, it never
    /// panics) — the declaration rules reject non-positive moduli, but
    /// enforcement must stay safe on any loaded value.
    fn is_multiple_of(&self, m: &Self) -> bool;
}

impl FacetDomain for crate::types::Number {
    fn is_multiple_of(&self, m: &Self) -> bool {
        crate::types::Number::is_multiple_of(self, m)
    }
}

impl FacetDomain for crate::duration::Duration {
    /// Unit-blind by design: the semantic basis is `total_nanos`, so
    /// `1500ms` is a multiple of `250ms` and of `500ms` but not of
    /// `1s` — exactly as the same quantities compare for equality
    /// (RFC 0017 semantic equivalence).
    fn is_multiple_of(&self, m: &Self) -> bool {
        let m = m.total_nanos();
        m != 0 && self.total_nanos() % m == 0
    }
}

/// One facet bound (`min`/`max`, inclusive or exclusive) over domain `T`.
#[derive(Debug, Clone, Serialize)]
pub struct FacetBoundOf<T> {
    pub value: T,
    pub exclusive: bool,
    /// The facet's `key = value` span in the schema source. Config-side
    /// violation MESSAGES carry the facet's authored spelling (schema
    /// and config are usually different files, so a same-file related
    /// span would point nowhere); the span serves definition-side
    /// diagnostics.
    pub span: Span,
}

/// A `multipleOf` facet over domain `T`.
#[derive(Debug, Clone, Serialize)]
pub struct FacetMultipleOf<T> {
    pub value: T,
    pub span: Span,
}

/// RFC 0018 facets over one value domain `T`. Empty (`NONE`) when the
/// field declares none — the loader rejects misplaced facets (NML2058)
/// before enforcement ever consults them.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Facets<T> {
    pub min: Option<FacetBoundOf<T>>,
    pub max: Option<FacetBoundOf<T>>,
    pub multiple_of: Option<FacetMultipleOf<T>>,
}

/// The RFC 0018 numeric bound — [`FacetBoundOf`] over [`Number`](crate::types::Number).
pub type FacetBound = FacetBoundOf<crate::types::Number>;
/// The RFC 0018 numeric `multipleOf` — [`FacetMultipleOf`] over `Number`.
pub type FacetMultiple = FacetMultipleOf<crate::types::Number>;
/// Facets on a `number` field.
pub type NumberFacets = Facets<crate::types::Number>;
/// Facets on a `duration` field.
pub type DurationFacets = Facets<crate::duration::Duration>;

/// The facets a primitive field carries, tagged by domain — a `number`
/// field's bounds are `Number`s, a `duration` field's are `Duration`s,
/// and the type system keeps the two from ever being compared across
/// domains. `None` means "no facet list authored" — for EVERY primitive,
/// bare `number`/`duration` fields included: the field's domain is the
/// type name's job, never this enum's, so a bare field of any type
/// carries the same variant and the same serialized shape.
///
/// The payloads are boxed: a `Duration` bound stores its canonical
/// segments inline (~1/8 KiB per facet record), and most fields carry no
/// facets at all — the box keeps `FieldType` (and with it every
/// `FieldDef`) at pointer scale instead of paying the largest variant
/// everywhere.
#[derive(Debug, Clone, Default, Serialize)]
pub enum PrimitiveFacets {
    #[default]
    None,
    Number(Box<NumberFacets>),
    Duration(Box<DurationFacets>),
}

impl PrimitiveFacets {
    pub fn is_none(&self) -> bool {
        match self {
            PrimitiveFacets::None => true,
            PrimitiveFacets::Number(f) => f.is_none(),
            PrimitiveFacets::Duration(f) => f.is_none(),
        }
    }
}

impl<T> Facets<T> {
    pub const NONE: Facets<T> = Facets {
        min: None,
        max: None,
        multiple_of: None,
    };
    pub fn is_none(&self) -> bool {
        self.min.is_none() && self.max.is_none() && self.multiple_of.is_none()
    }
}

impl<T: FacetDomain> Facets<T> {
    /// The canonical `key = value` renderings, declaration order
    /// (min/max/multipleOf) — the one spelling hover, Display, and
    /// definition diagnostics share.
    pub fn canonical_parts(&self) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(b) = &self.min {
            parts.push(format!(
                "{} = {}",
                if b.exclusive { "exclusiveMin" } else { "min" },
                b.value
            ));
        }
        if let Some(b) = &self.max {
            parts.push(format!(
                "{} = {}",
                if b.exclusive { "exclusiveMax" } else { "max" },
                b.value
            ));
        }
        if let Some(m) = &self.multiple_of {
            parts.push(format!("multipleOf = {}", m.value));
        }
        parts
    }

    /// Every RFC 0018 violation `n` commits against these facets, as
    /// message tails (the caller prefixes the subject). **The single
    /// comparison home**: config-side value checks and definition-side
    /// default checks both route here, so the two can never disagree
    /// about what "violates" means. Comparisons are exact — the
    /// domain's own `Ord` and `is_multiple_of`, never f64.
    pub fn violations(&self, n: &T) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(b) = &self.min {
            let fails = if b.exclusive {
                *n <= b.value
            } else {
                *n < b.value
            };
            if fails {
                // "below" would lie on the exclusive-equality edge: a value
                // AT an exclusive bound violates exclusivity, not ordering.
                let relation = if b.exclusive && *n == b.value {
                    "at"
                } else {
                    "below"
                };
                out.push(format!(
                    "is {n}, {relation} the schema's {} = {}",
                    if b.exclusive { "exclusiveMin" } else { "min" },
                    b.value
                ));
            }
        }
        if let Some(b) = &self.max {
            let fails = if b.exclusive {
                *n >= b.value
            } else {
                *n > b.value
            };
            if fails {
                let relation = if b.exclusive && *n == b.value {
                    "at"
                } else {
                    "above"
                };
                out.push(format!(
                    "is {n}, {relation} the schema's {} = {}",
                    if b.exclusive { "exclusiveMax" } else { "max" },
                    b.value
                ));
            }
        }
        if let Some(m) = &self.multiple_of {
            if !n.is_multiple_of(&m.value) {
                out.push(format!(
                    "is {n}, not a multiple of the schema's multipleOf = {} \
                     (checked exactly -- no float rounding)",
                    m.value
                ));
            }
        }
        out
    }

    /// Non-emitting admission test — the fast path for union variant
    /// selection, where a rejected variant's diagnostics are discarded
    /// anyway. Building them speculatively cost ~193ns per discarded
    /// message (a 16x constant on faceted unions); this answers the
    /// same question with zero allocation.
    pub fn admits(&self, n: &T) -> bool {
        if let Some(b) = &self.min {
            if if b.exclusive {
                *n <= b.value
            } else {
                *n < b.value
            } {
                return false;
            }
        }
        if let Some(b) = &self.max {
            if if b.exclusive {
                *n >= b.value
            } else {
                *n > b.value
            } {
                return false;
            }
        }
        match &self.multiple_of {
            Some(m) => n.is_multiple_of(&m.value),
            None => true,
        }
    }
}

/// The type of a field.
#[derive(Debug, Clone, Serialize)]
pub enum FieldType {
    /// A primitive, optionally carrying facets (domain-tagged: only
    /// ever non-`None` when `ty` is `Number` or `Duration` in a loaded
    /// schema, and always the variant matching `ty`). A struct variant
    /// deliberately: every pre-facet `Primitive(...)` pattern breaks at
    /// compile time, so each match site is reviewed rather than
    /// silently bypassed.
    Primitive {
        ty: PrimitiveType,
        facets: PrimitiveFacets,
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
    /// full validation for inline-block targets (RFC 0007 §6.2).
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
                // A `None` here is empty by definition (extraction never
                // wraps an empty list), so only the domain arms render.
                let parts = match facets {
                    PrimitiveFacets::None => Vec::new(),
                    PrimitiveFacets::Number(fs) => fs.canonical_parts(),
                    PrimitiveFacets::Duration(fs) => fs.canonical_parts(),
                };
                if !parts.is_empty() {
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

#[cfg(test)]
mod facet_tests {
    use super::*;
    use crate::types::Number;

    fn n(s: &str) -> Number {
        s.parse().unwrap()
    }

    /// Representation invariant: `facets` is `None` ⟺ the source authored
    /// no facet list — for EVERY primitive, faceted domains included. A
    /// bare `number` field once carried `Number(Facets::NONE)`, giving
    /// bare number and bare string fields different variants (and
    /// serialized shapes) for the same "no facets" fact — a downstream
    /// trap for anyone matching the variant as a domain proxy.
    #[test]
    fn bare_fields_carry_none_for_every_domain() {
        let (schema, diags) =
            crate::cst::extract_schema("model m:\n    a number\n    b duration\n    c string\n");
        assert!(diags.is_empty(), "{diags:?}");
        let fields = &schema.models[0].fields;
        for field in fields {
            match &field.field_type {
                FieldType::Primitive { facets, .. } => assert!(
                    matches!(facets, PrimitiveFacets::None),
                    "bare '{}' must carry PrimitiveFacets::None, got {facets:?}",
                    field.name
                ),
                other => panic!("expected primitives, got {other:?}"),
            }
        }

        // And the converse: an authored list is never flattened to None.
        let (schema, diags) = crate::cst::extract_schema(
            "model m:\n    a number(min = 1)\n    b duration(min = 1s)\n",
        );
        assert!(diags.is_empty(), "{diags:?}");
        for field in &schema.models[0].fields {
            match &field.field_type {
                FieldType::Primitive { facets, .. } => {
                    assert!(!facets.is_none(), "authored facets must survive extraction")
                }
                other => panic!("expected primitives, got {other:?}"),
            }
        }
    }

    /// Wire-shape pin: the serialized form of a bare vs faceted field's
    /// `facets`. No production consumer reads this JSON today (verified:
    /// identity hashes cover source bytes; extraction never persists) —
    /// this pin exists so the NEXT change to the shape is deliberate,
    /// not incidental.
    #[test]
    fn facets_serialized_shape_is_pinned() {
        let (schema, _) =
            crate::cst::extract_schema("model m:\n    a number\n    b number(min = 1)\n");
        let fields = &schema.models[0].fields;
        let shape = |i: usize| {
            serde_json::to_value(match &fields[i].field_type {
                FieldType::Primitive { facets, .. } => facets,
                other => panic!("expected primitive, got {other:?}"),
            })
            .unwrap()
        };
        assert_eq!(shape(0), serde_json::json!("None"), "bare field");
        assert_eq!(
            shape(1)["Number"]["min"]["value"],
            serde_json::json!(1),
            "authored facet carries its bound: {}",
            shape(1)
        );
    }

    /// `admits` is the non-emitting twin of `violations` and the union
    /// fast path trusts it. If the two ever disagree, a value is
    /// admitted by one path and reported by the other — a silent
    /// inconsistency no test elsewhere would catch. Swept over every
    /// facet combination against a value grid spanning the boundaries.
    #[test]
    fn admits_agrees_with_violations_exhaustively() {
        let bound = |v: &str, exclusive: bool| FacetBound {
            value: n(v),
            exclusive,
            span: Span::empty(0),
        };
        let mins = [
            None,
            Some(bound("0", false)),
            Some(bound("0", true)),
            Some(bound("-2.5", false)),
            Some(bound("10", true)),
        ];
        let maxs = [
            None,
            Some(bound("10", false)),
            Some(bound("10", true)),
            Some(bound("0", false)),
            Some(bound("2.5", true)),
        ];
        let muls = [
            None,
            Some(FacetMultiple {
                value: n("0.1"),
                span: Span::empty(0),
            }),
            Some(FacetMultiple {
                value: n("1"),
                span: Span::empty(0),
            }),
            Some(FacetMultiple {
                value: n("2.5"),
                span: Span::empty(0),
            }),
        ];
        let values = [
            "-10", "-2.5", "-0.1", "0", "0.0", "0.1", "0.3", "1", "2.5", "5", "9.9", "10", "10.0",
            "10.1", "1000", "0.05",
        ];
        let mut checked = 0usize;
        for min in &mins {
            for max in &maxs {
                for mul in &muls {
                    let f = NumberFacets {
                        min: min.clone(),
                        max: max.clone(),
                        multiple_of: mul.clone(),
                    };
                    for v in values {
                        let num = n(v);
                        assert_eq!(
                            f.admits(&num),
                            f.violations(&num).is_empty(),
                            "admits/violations disagree on {v} against {f:?}"
                        );
                        checked += 1;
                    }
                }
            }
        }
        // A fixed expected total — recomputing the product from the
        // same lengths would be a tautology that can never fail.
        assert_eq!(checked, 1_600, "grid size changed; update deliberately");
    }
}
