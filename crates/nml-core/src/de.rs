//! Serde `Deserialize` bridge for NML values.
//!
//! Converts NML block bodies into Rust structs via serde deserialization.
//! Supports flat properties, nested blocks (recursive), named list items
//! with label injection, and shared property inheritance (`.key:` blocks and `.key = value` scalars).
//!
//! **Naming law (normative):** entry points are named for their **input
//! type** — [`from_body`]`(&Body)`, [`from_value`]`(&Value)`,
//! [`from_body_resolved`]`(&Body, …)`; likewise the `defaults` family
//! (`from_block_defaulted` takes a `BlockDecl`, `from_document_defaulted`
//! a `Document`). A new entry point that breaks this rule is misnamed.
//!
//! # Example
//!
//! NML numbers are exact — integer fields deserialize as integers (a
//! fractional or out-of-range value is a typed error, never a silent cast):
//!
//! ```rust
//! use serde::Deserialize;
//! use nml_core::de::from_body;
//! use nml_core::query::Document;
//!
//! #[derive(Deserialize)]
//! struct ServerConfig {
//!     port: u16,
//!     host: String,
//!     debug: bool,
//! }
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let source = r#"
//! service MyApp:
//!     port = 8080
//!     host = "localhost"
//!     debug = true
//! "#;
//! let file = nml_core::parse(source)?;
//! let doc = Document::new(&file);
//! let body = doc.block("service", "MyApp").body().ok_or("no MyApp block")?;
//! let config: ServerConfig = from_body(body)?;
//! assert_eq!(config.port, 8080);
//! assert_eq!(config.host, "localhost");
//! # Ok(())
//! # }
//! ```

use std::fmt;

use serde::Deserialize;
use serde::de::{
    self, DeserializeSeed, Deserializer as _, EnumAccess, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};

use crate::ast::*;
use crate::resolve::{self, ValueResolver};
use crate::template;
use crate::types::{Number, Value};

/// Generates the integer `deserialize_*` methods: coerce to [`Number`],
/// convert exactly via [`number_to_int`], and visit the target width.
macro_rules! deserialize_int {
    ($method:ident, $visit:ident, $ty:ty, $name:literal) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            visitor.$visit(number_to_int::<$ty>(
                coerce_to_number(self.value, $name)?,
                $name,
            )?)
        }
    };
}

/// Errors that can occur during NML deserialization.
#[derive(Debug)]
pub enum Error {
    /// A deserialization / shape error (serde-level message).
    De(String),
    /// A value-resolution failure (`$ENV`, fallback, reference cycle). The typed
    /// [`ResolveError`](crate::resolve::ResolveError) is **preserved** (not flattened to a
    /// string) so a caller can react to a specific kind — e.g. remap
    /// [`EnvDisabled`](crate::resolve::ResolveError::EnvDisabled) to domain guidance —
    /// without fragile message matching.
    Resolve(crate::resolve::ResolveError),
}

impl Error {
    /// Prefix a `De` message with the failing field, building a key path
    /// as nested deserializers unwind (`field \`database\`: field
    /// \`port\`: u8 value out of range`). This is the locator half of
    /// the redaction posture: messages never echo values, so the path —
    /// which errors historically lacked entirely — pinpoints the site
    /// instead. Typed `Resolve` errors pass through untouched: they
    /// self-describe (the env var name) and callers match the variant.
    fn with_field(self, key: &str) -> Self {
        match self {
            Error::De(msg) => Error::De(format!("field `{key}`: {msg}")),
            other => other,
        }
    }

    /// Element twin of [`Error::with_field`] for array positions.
    fn with_element(self, index: usize) -> Self {
        match self {
            Error::De(msg) => Error::De(format!("element {index}: {msg}")),
            other => other,
        }
    }

    /// If this error wraps a denied `$ENV` reference, the referenced variable text (e.g.
    /// `"$ENV.GROQ_API_KEY"`). Delegates to
    /// [`ResolveError::env_disabled_var`](crate::resolve::ResolveError::env_disabled_var) so
    /// the two error types answer the question identically.
    pub fn env_disabled_var(&self) -> Option<&str> {
        match self {
            Error::Resolve(e) => e.env_disabled_var(),
            Error::De(_) => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::De(msg) => write!(f, "{msg}"),
            Error::Resolve(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

/// serde's provided error constructors interpolate the offending VALUE
/// into the message (``unknown variant `s3cr3t`…``, ``invalid type:
/// string "hunter2"…``). In this band a value may be a resolved `$ENV`
/// secret (the resolver erases provenance — every resolved secret is a
/// plain `Value::String` by the time serde sees it) and may be
/// kilobytes long, so the overrides below keep serde's message SHAPE —
/// value *kind* plus the expectation — and redact content (security
/// review). Field/variant NAMES keep serde's default formatting: they
/// are identifiers from the target type or source keys, the locator
/// half of the UX, and never carry values.
impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::De(msg.to_string())
    }

    fn invalid_type(unexp: de::Unexpected, exp: &dyn de::Expected) -> Self {
        Error::De(format!(
            "invalid type: {}, expected {exp}",
            unexpected_kind(&unexp)
        ))
    }

    fn invalid_value(unexp: de::Unexpected, exp: &dyn de::Expected) -> Self {
        Error::De(format!(
            "invalid value: {}, expected {exp}",
            unexpected_kind(&unexp)
        ))
    }

    fn unknown_variant(_variant: &str, expected: &'static [&'static str]) -> Self {
        Error::De(if expected.is_empty() {
            "unknown variant: there are no variants".to_string()
        } else {
            format!("unknown variant, expected one of: {}", expected.join(", "))
        })
    }
}

/// The value's KIND for redacted serde errors — never its content.
///
/// `Other` needs care: it is nominally a self-description, but serde's
/// own DEFAULT `Visitor::visit_i128`/`visit_u128` build
/// ``Other("integer `{v}` as i128")`` with the value already
/// interpolated — so a >64-bit literal reaching a visitor that doesn't
/// implement those methods would echo through this arm (found by
/// implementation review). Those two shapes are recognized and collapsed;
/// any other `Other` is genuine downstream-author text (`stringify!`d
/// type names in serde's own impls) and passes through.
fn unexpected_kind(u: &de::Unexpected) -> String {
    use de::Unexpected::*;
    match u {
        Bool(_) => "a boolean".to_string(),
        Unsigned(_) | Signed(_) => "an integer".to_string(),
        Float(_) => "a floating-point number".to_string(),
        Char(_) => "a character".to_string(),
        Str(_) => "a string".to_string(),
        Bytes(_) => "bytes".to_string(),
        Unit => "a unit value".to_string(),
        Option => "an optional value".to_string(),
        NewtypeStruct => "a newtype struct".to_string(),
        Seq => "a sequence".to_string(),
        Map => "a map".to_string(),
        Enum => "an enum".to_string(),
        UnitVariant => "a unit variant".to_string(),
        NewtypeVariant => "a newtype variant".to_string(),
        TupleVariant => "a tuple variant".to_string(),
        StructVariant => "a struct variant".to_string(),
        Other(s) if s.starts_with("integer `") => "an integer".to_string(),
        Other(s) => (*s).to_string(),
    }
}

/// A failed `$ENV`/fallback resolution surfaces as a deserialization error, so
/// the defaulted pipeline can resolve-then-deserialize with `?` in one place.
impl From<crate::resolve::ResolveError> for Error {
    fn from(e: crate::resolve::ResolveError) -> Self {
        Error::Resolve(e)
    }
}

/// Convert a [`Number`] to any Rust integer type, rejecting fractional
/// and out-of-range values with a precise error — exact or error, never
/// truncation (RFC 0016 §1.7 tier 1). The magnitude runs through the
/// exact i128/u128 paths, so `u64`/`u128` targets above `i64::MAX` no
/// longer funnel through `i64`.
/// The messages never interpolate the value (security review): the
/// resolver erases provenance, so `n` may be a coerced `$ENV` secret —
/// and Display of extreme members is up to ~6 KB, an amplification
/// lever. The caller's field/span context locates; the reason suffices.
fn number_to_int<T: TryFrom<i128> + TryFrom<u128>>(
    n: Number,
    type_name: &'static str,
) -> Result<T, Error> {
    if let Some(i) = n.to_i128() {
        return T::try_from(i).map_err(|_| Error::De(format!("{type_name} value out of range")));
    }
    if let Some(u) = n.to_u128() {
        return T::try_from(u).map_err(|_| Error::De(format!("{type_name} value out of range")));
    }
    Err(Error::De(if n.is_integral() {
        format!("{type_name} value out of range")
    } else {
        format!("{type_name} value has a fractional part")
    }))
}

/// Deserialize a struct from an NML block body.
///
/// This operates on the *raw* AST: `$ENV.KEY` secrets deserialize as their
/// literal reference text and `a | b` fallback chains deserialize as the
/// primary value only. Configuration that uses env vars or fallbacks must
/// go through [`from_body_resolved`], which resolves both before
/// deserializing.
pub fn from_body<'de, T: Deserialize<'de>>(body: &'de Body) -> Result<T, Error> {
    let deserializer = BodyDeserializer { body };
    T::deserialize(deserializer)
}

/// Deserialize a value from an NML `Value`.
pub fn from_value<'de, T: Deserialize<'de>>(value: &'de Value) -> Result<T, Error> {
    let deserializer = ValueDeserializer { value };
    T::deserialize(deserializer)
}

/// Resolve values, apply shared property inheritance, then deserialize.
///
/// Pipeline: `resolve_body` -> `apply_shared_properties` -> `from_body`.
pub fn from_body_resolved<T: for<'de> Deserialize<'de>>(
    body: &Body,
    resolver: &ValueResolver,
) -> Result<T, Error> {
    let resolved = resolver.resolve_body(body)?;
    let merged = resolve::apply_shared_properties(&resolved);
    from_body(&merged)
}

// ---------------------------------------------------------------------------
// Body -> map deserializer
// ---------------------------------------------------------------------------

struct BodyDeserializer<'a> {
    body: &'a Body,
}

impl<'de> de::Deserializer<'de> for BodyDeserializer<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        let entries = collect_body_map_entries(self.body);
        visitor.visit_map(BodyMapAccess { entries, index: 0 })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    /// RFC 0015 at the ROOT: `from_body` called directly on an annotated body
    /// deserializing into a Rust enum takes the same synthesized-external-tag
    /// path as every nested level — full symmetry, no "works one level down but
    /// not at the top" asymmetry. Un-annotated bodies are unchanged.
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match &self.body.type_annotation {
            Some(variant) => visitor.visit_enum(AnnotatedEnumAccess {
                variant: &variant.name,
                body: self.body,
                label: None,
            }),
            None => self.deserialize_any(visitor),
        }
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct identifier ignored_any
    }
}

// ---------------------------------------------------------------------------
// Map access for body entries (Property + NestedBlock + SharedProperty)
// ---------------------------------------------------------------------------

enum BodyMapEntry<'a> {
    Property(&'a Property),
    Block(&'a Body, &'a str),
    /// `.name = value` shared property (deserializes like a normal property value).
    SharedScalar {
        key: &'a str,
        value: &'a crate::types::SpannedValue,
    },
}

fn collect_body_map_entries<'a>(body: &'a Body) -> Vec<BodyMapEntry<'a>> {
    use crate::ast::SharedPropertyKind;
    body.entries
        .iter()
        .filter_map(|e| match &e.kind {
            BodyEntryKind::Property(p) => Some(BodyMapEntry::Property(p)),
            BodyEntryKind::NestedBlock(nb) => {
                Some(BodyMapEntry::Block(&nb.body, nb.name.name.as_str()))
            }
            BodyEntryKind::SharedProperty(sp) => match &sp.kind {
                SharedPropertyKind::Block(b) => Some(BodyMapEntry::Block(b, sp.name.name.as_str())),
                SharedPropertyKind::Scalar(sv) => Some(BodyMapEntry::SharedScalar {
                    key: sp.name.name.as_str(),
                    value: sv,
                }),
            },
            // `ListItem`s are handled by the list deserializer, not the map
            // path. `Modifier`, `FieldDefinition`, and `Arm` are
            // serde-invisible by design — there is no generic serde target for
            // an ACL, a schema field def, or an ordered routing table. Each is
            // a **hand-parsed** construct: the consumer marks its field
            // `#[serde(skip)]` and reads the `BodyEntryKind` directly (e.g.
            // `|allow`/`|grant` and RFC 0007 arm sets — see nudge's
            // `DenialBinding::parse_from_body`, which walks
            // `BodyEntryKind::Arm`). A shorthand-filled arm block (RFC 0007
            // §4.3 ⑤) deserializes identically to an authored one: the block
            // is visible here, its arms hand-read past serde.
            BodyEntryKind::ListItem(_)
            | BodyEntryKind::Modifier(_)
            | BodyEntryKind::FieldDefinition(_)
            | BodyEntryKind::Arm(_) => None,
        })
        .collect()
}

struct BodyMapAccess<'a> {
    entries: Vec<BodyMapEntry<'a>>,
    index: usize,
}

impl<'de> MapAccess<'de> for BodyMapAccess<'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        if self.index >= self.entries.len() {
            return Ok(None);
        }
        let key = match &self.entries[self.index] {
            BodyMapEntry::Property(p) => p.name.name.as_str(),
            BodyMapEntry::Block(_, name) => name,
            BodyMapEntry::SharedScalar { key, .. } => key,
        };
        seed.deserialize(de::value::StrDeserializer::new(key))
            .map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let entry = &self.entries[self.index];
        self.index += 1;
        let key = match entry {
            BodyMapEntry::Property(p) => p.name.name.as_str(),
            BodyMapEntry::Block(_, name) => name,
            BodyMapEntry::SharedScalar { key, .. } => key,
        };
        match entry {
            BodyMapEntry::Property(prop) => seed.deserialize(ValueDeserializer {
                value: &prop.value.value,
            }),
            BodyMapEntry::Block(body, _) => seed.deserialize(NestedBlockDeserializer { body }),
            BodyMapEntry::SharedScalar { value, .. } => seed.deserialize(ValueDeserializer {
                value: &value.value,
            }),
        }
        .map_err(|e| e.with_field(key))
    }
}

// ---------------------------------------------------------------------------
// Nested block deserializer (dispatches struct/seq/any)
// ---------------------------------------------------------------------------

struct NestedBlockDeserializer<'a> {
    body: &'a Body,
}

impl<'de> de::Deserializer<'de> for NestedBlockDeserializer<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        let has_list_items = self
            .body
            .entries
            .iter()
            .any(|e| matches!(&e.kind, BodyEntryKind::ListItem(_)));
        if has_list_items {
            self.deserialize_seq(visitor)
        } else {
            self.deserialize_map(visitor)
        }
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        let entries = collect_body_map_entries(self.body);
        visitor.visit_map(BodyMapAccess { entries, index: 0 })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        let items: Vec<&ListItem> = self
            .body
            .entries
            .iter()
            .filter_map(|e| match &e.kind {
                BodyEntryKind::ListItem(item) => Some(item),
                _ => None,
            })
            .collect();
        visitor.visit_seq(ListItemSeqAccess { items, index: 0 })
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_some(self)
    }

    /// RFC 0015 — the runtime side of nominal disambiguation. A block-valued
    /// union deserializes into a Rust enum; without help serde reaches it as an
    /// unwrapped map whose only native selector is `#[serde(untagged)]`
    /// (try-first, which cannot honor a header annotation). When the block
    /// carries `as <Variant>`, we present it as a **synthesized external tag** —
    /// the single-variant enum access `{ <Variant>: body }` — to a standard,
    /// externally-tagged enum. The built value is then the *same* variant the
    /// schema resolved and the validator checked: no try-first, no divergence.
    /// An un-annotated block keeps today's behavior (forwarded to `any`), so
    /// disjoint unions and non-enum targets are unaffected.
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match &self.body.type_annotation {
            Some(variant) => visitor.visit_enum(AnnotatedEnumAccess {
                variant: &variant.name,
                body: self.body,
                label: None,
            }),
            None => self.deserialize_any(visitor),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_unit()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string
        bytes byte_buf unit unit_struct newtype_struct tuple
        tuple_struct identifier
    }
}

/// EnumAccess for an `as <Variant>`-annotated block (RFC 0015): yields the
/// annotation as the external tag, then deserializes the block body as that
/// variant's content. Serde applies the target enum's own `#[serde(rename…)]`
/// and reports `unknown variant` for a tag no variant matches — the same
/// canonical enum handling every other serde format gets.
///
/// `label` carries a named list item's key (`- one as modelB:` → `"one"`) so the
/// variant's content deserializes with the SAME name-injection semantics an
/// un-annotated named item gets (`NamedItemDeserializer`); a field-level block
/// has no label.
struct AnnotatedEnumAccess<'a> {
    variant: &'a str,
    body: &'a Body,
    label: Option<&'a str>,
}

impl<'de> EnumAccess<'de> for AnnotatedEnumAccess<'de> {
    type Error = Error;
    type Variant = AnnotatedVariantAccess<'de>;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Self::Error> {
        let tag = seed.deserialize(de::value::StrDeserializer::<Error>::new(self.variant))?;
        Ok((
            tag,
            AnnotatedVariantAccess {
                body: self.body,
                label: self.label,
            },
        ))
    }
}

struct AnnotatedVariantAccess<'a> {
    body: &'a Body,
    label: Option<&'a str>,
}

impl<'de> VariantAccess<'de> for AnnotatedVariantAccess<'de> {
    type Error = Error;

    /// `field as <Variant>:` with an empty body targets a unit variant.
    fn unit_variant(self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, Self::Error> {
        match self.label {
            Some(label) => seed.deserialize(NamedItemDeserializer {
                label,
                body: self.body,
            }),
            None => seed.deserialize(NestedBlockDeserializer { body: self.body }),
        }
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        NestedBlockDeserializer { body: self.body }.deserialize_seq(visitor)
    }

    /// The common case: a same-class model variant is a struct — deserialize the
    /// content as its fields (a map), never re-entering the enum path. A named
    /// item's label injects as `name` exactly as it would un-annotated.
    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.label {
            Some(label) => NamedItemDeserializer {
                label,
                body: self.body,
            }
            .deserialize_map(visitor),
            None => NestedBlockDeserializer { body: self.body }.deserialize_map(visitor),
        }
    }
}

// ---------------------------------------------------------------------------
// Sequence access for list items
// ---------------------------------------------------------------------------

struct ListItemSeqAccess<'a> {
    items: Vec<&'a ListItem>,
    index: usize,
}

impl<'de> SeqAccess<'de> for ListItemSeqAccess<'de> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        if self.index >= self.items.len() {
            return Ok(None);
        }
        let item = self.items[self.index];
        let position = self.index;
        self.index += 1;

        // Named items locate by their label (`- Api:` → ``field `Api```),
        // which reads better than a positional index; anonymous items
        // locate by position.
        let locate = |e: Error| match &item.kind {
            ListItemKind::Named { name, .. } => e.with_field(&name.name),
            _ => e.with_element(position),
        };

        match &item.kind {
            ListItemKind::Named { name, body } => seed
                .deserialize(NamedItemDeserializer {
                    label: &name.name,
                    body,
                })
                .map(Some),
            // A bare scalar deserializes as its value. A scalar *with a body* is a
            // materialized shorthand item (the value already injected into the body by
            // `apply_positional`); deserialize the body as a struct, exactly like a
            // nested block — `de` need not know the shorthand field, the pass placed it.
            ListItemKind::Shorthand { value, body: None } => seed
                .deserialize(ValueDeserializer {
                    value: &value.value,
                })
                .map(Some),
            ListItemKind::Shorthand {
                body: Some(body), ..
            } => seed.deserialize(NestedBlockDeserializer { body }).map(Some),
            ListItemKind::Reference(ident) => seed
                .deserialize(de::value::StrDeserializer::<Error>::new(&ident.name))
                .map(Some),
            ListItemKind::Role(s) => seed
                .deserialize(de::value::StrDeserializer::<Error>::new(s))
                .map(Some),
        }
        .map_err(locate)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len().saturating_sub(self.index))
    }
}

// ---------------------------------------------------------------------------
// Named list item deserializer (injects label as "name")
// ---------------------------------------------------------------------------

struct NamedItemDeserializer<'a> {
    label: &'a str,
    body: &'a Body,
}

impl<'de> de::Deserializer<'de> for NamedItemDeserializer<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        let has_explicit_name = self.body.entries.iter().any(|e| {
            matches!(
                &e.kind,
                BodyEntryKind::Property(p) if p.name.name == "name"
            )
        });

        let body_entries = collect_body_map_entries(self.body);

        visitor.visit_map(NamedItemMapAccess {
            label: self.label,
            body_entries,
            body_index: 0,
            inject_name: !has_explicit_name,
            name_value_pending: false,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_some(self)
    }

    /// RFC 0015 at the LIST-ELEMENT level: `- one as modelB:` deserializing into
    /// a Rust enum takes the same synthesized-external-tag path as a field-level
    /// block (see `NestedBlockDeserializer::deserialize_enum`), carrying the item
    /// label so the variant's content keeps named-item `name` injection.
    /// Un-annotated items are unchanged (forwarded to `any`).
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match &self.body.type_annotation {
            Some(variant) => visitor.visit_enum(AnnotatedEnumAccess {
                variant: &variant.name,
                body: self.body,
                label: Some(self.label),
            }),
            None => self.deserialize_any(visitor),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_unit()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string
        bytes byte_buf unit unit_struct newtype_struct seq tuple
        tuple_struct identifier
    }
}

struct NamedItemMapAccess<'a> {
    label: &'a str,
    body_entries: Vec<BodyMapEntry<'a>>,
    body_index: usize,
    inject_name: bool,
    name_value_pending: bool,
}

impl<'de> MapAccess<'de> for NamedItemMapAccess<'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        if self.inject_name {
            self.inject_name = false;
            self.name_value_pending = true;
            return seed
                .deserialize(de::value::StrDeserializer::new("name"))
                .map(Some);
        }

        if self.body_index >= self.body_entries.len() {
            return Ok(None);
        }
        let key = match &self.body_entries[self.body_index] {
            BodyMapEntry::Property(p) => p.name.name.as_str(),
            BodyMapEntry::Block(_, name) => name,
            BodyMapEntry::SharedScalar { key, .. } => key,
        };
        seed.deserialize(de::value::StrDeserializer::new(key))
            .map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        if self.name_value_pending {
            self.name_value_pending = false;
            // The injected label is a value like any other: if the
            // target rejects it, the error names the field it landed in
            // (`name`), same as every sibling arm below.
            return seed
                .deserialize(de::value::StrDeserializer::<Error>::new(self.label))
                .map_err(|e| e.with_field("name"));
        }

        let entry = &self.body_entries[self.body_index];
        self.body_index += 1;
        let key = match entry {
            BodyMapEntry::Property(p) => p.name.name.as_str(),
            BodyMapEntry::Block(_, name) => name,
            BodyMapEntry::SharedScalar { key, .. } => key,
        };
        match entry {
            BodyMapEntry::Property(prop) => seed.deserialize(ValueDeserializer {
                value: &prop.value.value,
            }),
            BodyMapEntry::Block(body, _) => seed.deserialize(NestedBlockDeserializer { body }),
            BodyMapEntry::SharedScalar { value, .. } => seed.deserialize(ValueDeserializer {
                value: &value.value,
            }),
        }
        .map_err(|e| e.with_field(key))
    }
}

// ---------------------------------------------------------------------------
// Value deserializer
// ---------------------------------------------------------------------------

/// Coerce a Value to a number. Native numbers pass through; string-typed
/// values parse via the liberal, **exact** coercion grammar (RFC 0016
/// §1.4 — Postel for machine-emitted data): env vars resolve to
/// `Value::String`, so `$ENV.PORT = "9007199254740993"` must survive
/// exactly, and `$ENV.RATE = "1e-6"` now arrives exact instead of
/// f64-rounded. Failures embed the numeric core's *reason* but never the
/// value: the resolver erases provenance (`resolve.rs`: resolved secrets
/// become plain `Value::String`s), so ANY coerced string could be a
/// resolved secret — echoing content here would leak credentials into
/// logs. The span/key context callers attach is the locator; the reason
/// text is the diagnosis.
fn coerce_to_number(value: &Value, target: &'static str) -> Result<Number, Error> {
    let (s, kind) = match value {
        Value::Number(n) => return Ok(*n),
        Value::String(s) => (s, "string"),
        Value::Secret(s) => (s, "secret"),
        Value::Duration(s) => (s, "duration"),
        Value::Path(s) => (s, "path"),
        other => {
            return Err(Error::De(format!(
                "expected {target}, got {}",
                other.type_name()
            )));
        }
    };
    Number::parse_coercion(s).map_err(|e| Error::De(format!("expected {target}, got {kind} ({e})")))
}

/// Coerce a string-typed Value to bool if it matches common truthy/falsy strings.
fn coerce_to_bool(value: &Value) -> Option<bool> {
    match value {
        Value::String(s) | Value::Secret(s) => match s.as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

struct ValueDeserializer<'a> {
    value: &'a Value,
}

impl<'de> de::Deserializer<'de> for ValueDeserializer<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::String(s) => visitor.visit_str(s),
            Value::TemplateString(segs) => {
                let s = template::segments_to_string(segs);
                visitor.visit_string(s)
            }
            // The RFC 0016 §1.7 ladder, form-based: fraction form visits
            // f64 (the documented-lossy edge for untyped consumers, same
            // as the old Float arm); integer form climbs i64 → u64 → i128
            // → u128 exactly and never silently rounds — beyond u128 is a
            // hard error naming the exact alternatives.
            Value::Number(n) => {
                if n.scale() > 0 {
                    visitor.visit_f64(n.to_f64())
                } else if let Some(v) = n.to_i64() {
                    visitor.visit_i64(v)
                } else if let Some(v) = n.to_u64() {
                    visitor.visit_u64(v)
                } else if let Some(v) = n.to_i128() {
                    visitor.visit_i128(v)
                } else if let Some(v) = n.to_u128() {
                    visitor.visit_u128(v)
                } else {
                    // No value interpolation: integer-form Display here
                    // can be ~6 KB (bounded-echo posture, §1.2).
                    Err(Error::De(
                        "integer exceeds 128 bits; capture it exactly with an \
                         `nml_core::types::Number` field or a String"
                            .to_string(),
                    ))
                }
            }
            Value::Bool(b) => visitor.visit_bool(*b),
            Value::Duration(s) | Value::Path(s) | Value::Secret(s) | Value::Role(s) => {
                visitor.visit_str(s)
            }
            Value::Reference(s) => visitor.visit_str(s),
            Value::Money(m) => visitor.visit_string(m.format_display()),
            // `deserialize_any` reaches here only for self-describing
            // targets (untagged enums, `Value`-like), which consume the
            // whole sequence; fixed-size targets go through
            // `deserialize_seq`/`_tuple`, where exhaustion is enforced.
            Value::Array(items) => visitor.visit_seq(ArraySeqAccess { items, index: 0 }),
            Value::Fallback(primary, _) => ValueDeserializer {
                value: &primary.value,
            }
            .deserialize_any(visitor),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::Bool(b) => visitor.visit_bool(*b),
            _ => match coerce_to_bool(self.value) {
                Some(b) => visitor.visit_bool(b),
                None => Err(Error::De(format!(
                    "expected bool, got {}",
                    self.value.type_name()
                ))),
            },
        }
    }

    /// Tier 2 (RFC 0016 §1.7): the correctly-rounded, documented-lossy
    /// decimal→binary edge — the only place f64 enters typed targets.
    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_f64(coerce_to_number(self.value, "f64")?.to_f64())
    }

    /// Direct single-rounding f32 (never `to_f64() as f32`, which
    /// double-rounds — the `16777217.0000000001` class).
    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_f32(coerce_to_number(self.value, "f32")?.to_f32())
    }

    deserialize_int!(deserialize_i64, visit_i64, i64, "i64");
    deserialize_int!(deserialize_i32, visit_i32, i32, "i32");
    deserialize_int!(deserialize_i16, visit_i16, i16, "i16");
    deserialize_int!(deserialize_i8, visit_i8, i8, "i8");
    deserialize_int!(deserialize_u64, visit_u64, u64, "u64");
    deserialize_int!(deserialize_u32, visit_u32, u32, "u32");
    deserialize_int!(deserialize_u16, visit_u16, u16, "u16");
    deserialize_int!(deserialize_u8, visit_u8, u8, "u8");
    deserialize_int!(deserialize_i128, visit_i128, i128, "i128");
    deserialize_int!(deserialize_u128, visit_u128, u128, "u128");

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::String(s) => visitor.visit_str(s),
            Value::TemplateString(segs) => visitor.visit_string(template::segments_to_string(segs)),
            Value::Path(s) | Value::Duration(s) | Value::Secret(s) => visitor.visit_str(s),
            Value::Reference(s) | Value::Role(s) => visitor.visit_str(s),
            Value::Money(m) => visitor.visit_string(m.format_display()),
            _ => Err(Error::De(format!(
                "expected string, got {}",
                self.value.type_name()
            ))),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_str(visitor)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::Array(items) => {
                let mut seq = ArraySeqAccess { items, index: 0 };
                let out = visitor.visit_seq(&mut seq)?;
                // Fixed-size targets (tuples, `[T; N]`) stop early; the
                // leftovers are config the author wrote and the program
                // would never see. Never silent.
                if seq.remaining() > 0 {
                    return Err(Error::De(format!(
                        "array has {} element(s), but the target accepts only {}",
                        items.len(),
                        seq.index
                    )));
                }
                Ok(out)
            }
            _ => Err(Error::De(format!(
                "expected array, got {}",
                self.value.type_name()
            ))),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_some(self)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_unit()
    }

    /// Unit-variant enums deserialize from string-typed values: the
    /// string is handed to serde's variant resolver via `visit_enum`,
    /// so `#[serde(rename_all = "...")]` and `#[serde(rename = "...")]`
    /// on the target enum work exactly as they do for JSON/TOML/YAML.
    /// Unknown variants error with the expected-variant list but never
    /// echo the offending value (this band's redaction rule: the string
    /// may be a resolved `$ENV` secret — see the `de::Error` overrides).
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::String(s)
            | Value::Path(s)
            | Value::Duration(s)
            | Value::Secret(s)
            | Value::Role(s)
            | Value::Reference(s) => {
                visitor.visit_enum(de::value::StrDeserializer::<Error>::new(s))
            }
            Value::TemplateString(segs) => visitor.visit_enum(
                de::value::StringDeserializer::<Error>::new(template::segments_to_string(segs)),
            ),
            Value::Fallback(primary, _) => ValueDeserializer {
                value: &primary.value,
            }
            .deserialize_enum(name, variants, visitor),
            _ => Err(Error::De(format!(
                "expected string for enum {name}, got {}",
                self.value.type_name()
            ))),
        }
    }

    /// Tier 3 (RFC 0016 §1.7): the exact-capture handshake. [`Number`]'s
    /// `Deserialize` asks for a newtype struct with the private token
    /// name; answering with the compact member encoding keeps `Number`
    /// fields lossless through NML's own bridge (native numbers pass
    /// through; coercible string kinds follow the §1.4 liberal grammar,
    /// matching every other numeric target). All other newtype structs
    /// stay transparent, exactly as the old forward did.
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if name == crate::decimal::NUMBER_NEWTYPE_TOKEN {
            let n = coerce_to_number(self.value, "number")?;
            // Compact member encoding, not the plain form: `{coeff}e{-scale}`
            // is ≤ ~42 bytes for EVERY member (plain form of extreme-scale
            // values is ~6 KB — an amplification lever), and it round-trips
            // the stored cohort member exactly through the visitor's §1.4
            // coercion-grammar arm (the plain form only preserves the
            // value for negative-scale members).
            return visitor.visit_string(format!("{}e{}", n.coeff(), -(n.scale() as i32)));
        }
        self.deserialize_any(visitor)
    }

    /// Fixed-arity targets route through `deserialize_seq` so the
    /// leftover-element check applies (forwarding these to
    /// `deserialize_any` would silently discard extra array elements —
    /// implementation-review finding).
    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_seq(visitor)
    }

    serde::forward_to_deserialize_any! {
        char bytes byte_buf unit unit_struct
        map struct identifier
    }
}

// ---------------------------------------------------------------------------
// Array sequence access
// ---------------------------------------------------------------------------

struct ArraySeqAccess<'a> {
    items: &'a [crate::types::SpannedValue],
    index: usize,
}

impl ArraySeqAccess<'_> {
    /// Elements the target never asked for. A fixed-size target (tuple,
    /// `[T; N]`) stops seeding early, and silently dropping the rest
    /// would be exactly the kind of quiet data loss this crate refuses —
    /// so `deserialize_seq` checks exhaustion after the visitor returns
    /// (implementation-review finding; serde expects the Deserializer to
    /// enforce this, as serde_json does).
    fn remaining(&self) -> usize {
        self.items.len().saturating_sub(self.index)
    }
}

impl<'de> SeqAccess<'de> for ArraySeqAccess<'de> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        if self.index >= self.items.len() {
            return Ok(None);
        }
        let item = &self.items[self.index];
        let position = self.index;
        self.index += 1;
        seed.deserialize(ValueDeserializer { value: &item.value })
            .map(Some)
            .map_err(|e| e.with_element(position))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.remaining())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cst::parse_to_ast;
    use crate::query::Document;
    use serde::Deserialize;

    #[test]
    fn deserialize_struct_from_body() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct Config {
            port: f64,
            host: String,
            debug: bool,
        }

        let source = r#"
service MyApp:
    port = 8080
    host = "localhost"
    debug = true
"#;
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "MyApp").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.port, 8080.0);
        assert_eq!(config.host, "localhost");
        assert!(config.debug);
    }

    // Shared RFC 0015 fixture: a same-class model union as an externally-tagged
    // Rust enum, plus its host struct. Both variants are exercised below so all
    // fields are genuinely read.
    #[derive(Deserialize, Debug, PartialEq)]
    enum Slot {
        #[serde(rename = "modelA")]
        ModelA { a: String },
        #[serde(rename = "modelB")]
        ModelB { b: String },
    }
    #[derive(Deserialize, Debug, PartialEq)]
    struct Host {
        slot: Slot,
    }

    fn host_slot(source: &str) -> Result<Host, Error> {
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("host", "H").body().unwrap();
        from_body(body)
    }

    #[test]
    fn deserialize_honors_nominal_annotation_as_external_tag() {
        // RFC 0015: `as <Variant>` lowers to a synthesized external tag, so a
        // block-valued same-class union deserializes to the *annotated* variant
        // — not serde-untagged try-first. Both variants (and `#[serde(rename)]`)
        // are exercised.
        assert_eq!(
            host_slot("host H:\n    slot as modelB:\n        b = \"hi\"\n").unwrap(),
            Host {
                slot: Slot::ModelB { b: "hi".into() }
            }
        );
        assert_eq!(
            host_slot("host H:\n    slot as modelA:\n        a = \"lo\"\n").unwrap(),
            Host {
                slot: Slot::ModelA { a: "lo".into() }
            }
        );
    }

    #[test]
    fn deserialize_unknown_annotation_variant_errors_at_runtime() {
        // The annotation names no variant → serde's canonical unknown-variant
        // error at deserialization (the validator also reports it upstream).
        assert!(
            host_slot("host H:\n    slot as modelZ:\n        a = \"x\"\n").is_err(),
            "unknown annotation variant must error at deserialization"
        );
    }

    #[test]
    fn deserialize_with_array() {
        #[derive(Deserialize, Debug)]
        struct Config {
            tags: Vec<String>,
        }

        let source = "service App:\n    tags = [\"web\", \"api\"]\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.tags, vec!["web", "api"]);
    }

    #[test]
    fn deserialize_value_directly() {
        let v = Value::number(crate::num!(42.0));
        let n: f64 = from_value(&v).unwrap();
        assert_eq!(n, 42.0);
    }

    #[test]
    fn type_mismatch_error() {
        let v = Value::Bool(true);
        let result: Result<f64, _> = from_value(&v);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_nested_block_as_struct() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct Server {
            port: u16,
            db: DbConfig,
        }

        #[derive(Deserialize, Debug, PartialEq)]
        struct DbConfig {
            backend: String,
            url: String,
        }

        let source = r#"
server App:
    port = 8080
    db:
        backend = "postgres"
        url = "postgres://localhost/dev"
"#;
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let config: Server = from_body(body).unwrap();
        assert_eq!(config.port, 8080);
        assert_eq!(config.db.backend, "postgres");
        assert_eq!(config.db.url, "postgres://localhost/dev");
    }

    #[test]
    fn deserialize_optional_nested_block() {
        #[derive(Deserialize, Debug)]
        struct Server {
            port: u16,
            db: Option<Db>,
        }

        #[derive(Deserialize, Debug)]
        struct Db {
            url: String,
        }

        let source = "server App:\n    port = 3000\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let config: Server = from_body(body).unwrap();
        assert_eq!(config.port, 3000);
        assert!(config.db.map(|db| db.url).is_none());
    }

    /// RFC §2 pinned fixture: an integer-form value beyond u128 through
    /// an UNTYPED target (`deserialize_any`) is a hard error naming the
    /// typed alternatives — never a silent f64 for an integer-form
    /// value. (Typed `Number` fields capture it exactly; this pins the
    /// self-describing path's refusal.)
    #[test]
    fn beyond_u128_integer_form_is_a_hard_error_for_untyped_targets() {
        #[derive(Deserialize, Debug)]
        struct AnyCfg {
            big: serde_json::Value,
        }
        let source = format!("cfg C:\n    big = 1{}\n", "0".repeat(100));
        let file = parse_to_ast(&source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("cfg", "C").body().unwrap();
        let err = from_body::<AnyCfg>(body).expect_err("10^100 must not reach an untyped target");
        let msg = err.to_string();
        assert!(
            msg.contains("exceeds 128 bits") && msg.contains("Number"),
            "must name the typed alternatives: {msg}"
        );
        // And the value never leaks into the message (redaction posture).
        assert!(!msg.contains("100000"), "no value echo: {msg}");

        // Controls: the i64 and u64 rungs arrive as JSON numbers.
        for (literal, expected) in [
            ("8080", serde_json::json!(8080u64)),
            ("18446744073709551615", serde_json::json!(u64::MAX)),
        ] {
            let source = format!("cfg C:\n    big = {literal}\n");
            let file = parse_to_ast(&source).unwrap();
            let doc = Document::new(&file);
            let body = doc.block("cfg", "C").body().unwrap();
            let cfg: AnyCfg = from_body(body).unwrap();
            assert_eq!(cfg.big, expected, "{literal}");
        }
        // The (u64::MAX, 2^128) band errors too — but via serde_json's
        // own visitor limit (no 128-bit rungs in its Value), so the
        // message is the foreign "out of range", without our typed
        // hint. Documented §1.7 posture (serde #1717): errors, never
        // rounds — pinned here so a serde_json change is noticed.
        let source = "cfg C:\n    big = 18446744073709551616\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("cfg", "C").body().unwrap();
        assert!(from_body::<AnyCfg>(body).is_err(), "2^64 must not round");
    }

    #[test]
    fn deserialize_named_list_items() {
        #[derive(Deserialize, Debug)]
        struct Workflow {
            steps: Vec<Step>,
        }

        #[derive(Deserialize, Debug)]
        struct Step {
            name: String,
            provider: String,
        }

        let source = r#"
workflow W:
    steps:
        - classify:
            provider = "fast"
        - generate:
            provider = "slow"
"#;
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("workflow", "W").body().unwrap();
        let config: Workflow = from_body(body).unwrap();
        assert_eq!(config.steps.len(), 2);
        assert_eq!(config.steps[0].name, "classify");
        assert_eq!(config.steps[0].provider, "fast");
        assert_eq!(config.steps[1].name, "generate");
        assert_eq!(config.steps[1].provider, "slow");
    }

    #[test]
    fn named_item_explicit_name_wins() {
        #[derive(Deserialize, Debug)]
        struct Item {
            name: String,
            url: String,
        }

        let source = r#"
service S:
    items:
        - Label:
            name = "OverriddenName"
            url = "/api"
"#;
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "S").body().unwrap();

        #[derive(Deserialize, Debug)]
        struct S {
            items: Vec<Item>,
        }
        let config: S = from_body(body).unwrap();
        assert_eq!(config.items[0].name, "OverriddenName");
        assert_eq!(config.items[0].url, "/api");
    }

    #[test]
    fn deserialize_integer_types() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct Config {
            port: u16,
            retries: u32,
            offset: i32,
            big: u64,
        }

        let source = r#"
service App:
    port = 8080
    retries = 3
    offset = -10
    big = 1000000
"#;
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.port, 8080);
        assert_eq!(config.retries, 3);
        assert_eq!(config.offset, -10);
        assert_eq!(config.big, 1000000);
    }

    #[test]
    fn deserialize_shorthand_list_items() {
        #[derive(Deserialize, Debug)]
        struct Tools {
            items: Vec<String>,
        }

        let source = r#"
service S:
    items:
        - "tool-a"
        - "tool-b"
"#;
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "S").body().unwrap();
        let config: Tools = from_body(body).unwrap();
        assert_eq!(config.items, vec!["tool-a", "tool-b"]);
    }

    #[test]
    fn shorthand_items_do_not_get_name_injected() {
        #[derive(Deserialize, Debug)]
        struct Config {
            paths: Vec<String>,
        }

        let source = r#"
service S:
    paths:
        - "/api/v1"
        - "/api/v2"
        - "/health"
"#;
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "S").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.paths, vec!["/api/v1", "/api/v2", "/health"]);
    }

    #[test]
    fn mixed_named_and_shorthand_items_via_untagged_enum() {
        #[derive(Deserialize, Debug)]
        struct Step {
            name: String,
            provider: String,
        }

        #[derive(Deserialize, Debug)]
        #[serde(untagged)]
        enum Item {
            Named(Step),
            Shorthand(String),
        }

        #[derive(Deserialize, Debug)]
        struct Config {
            items: Vec<Item>,
        }

        let source = r#"
service S:
    items:
        - step1:
            provider = "openai"
        - "/shorthand/path"
        - step2:
            provider = "anthropic"
"#;
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "S").body().unwrap();
        let config: Config = from_body(body).unwrap();

        assert_eq!(config.items.len(), 3);

        match &config.items[0] {
            Item::Named(s) => {
                assert_eq!(s.name, "step1");
                assert_eq!(s.provider, "openai");
            }
            Item::Shorthand(s) => panic!("expected Named, got Shorthand({s})"),
        }

        match &config.items[1] {
            Item::Shorthand(s) => assert_eq!(s, "/shorthand/path"),
            Item::Named(s) => panic!("expected Shorthand, got Named({})", s.name),
        }

        match &config.items[2] {
            Item::Named(s) => {
                assert_eq!(s.name, "step2");
                assert_eq!(s.provider, "anthropic");
            }
            Item::Shorthand(s) => panic!("expected Named, got Shorthand({s})"),
        }
    }

    #[test]
    fn deserialize_nested_with_list_items_and_nested_blocks() {
        #[derive(Deserialize, Debug)]
        #[serde(rename_all = "camelCase")]
        struct Auth {
            provider: String,
            oidc_providers: Vec<OidcProvider>,
        }

        #[derive(Deserialize, Debug)]
        #[serde(rename_all = "camelCase")]
        struct OidcProvider {
            name: String,
            issuer: String,
            client_id: String,
            scopes: Vec<String>,
        }

        let source = r#"
auth MyAuth:
    provider = "oidc"
    oidcProviders:
        - Google:
            issuer = "https://accounts.google.com"
            clientId = "abc123"
            scopes = ["openid", "email"]
        - GitHub:
            issuer = "https://github.com"
            clientId = "def456"
            scopes = ["read:user"]
"#;
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("auth", "MyAuth").body().unwrap();
        let config: Auth = from_body(body).unwrap();
        assert_eq!(config.provider, "oidc");
        assert_eq!(config.oidc_providers.len(), 2);
        assert_eq!(config.oidc_providers[0].name, "Google");
        assert_eq!(
            config.oidc_providers[0].issuer,
            "https://accounts.google.com"
        );
        assert_eq!(config.oidc_providers[0].client_id, "abc123");
        assert_eq!(config.oidc_providers[0].scopes, vec!["openid", "email"]);
        assert_eq!(config.oidc_providers[1].name, "GitHub");
    }

    #[test]
    fn from_body_resolved_pipeline() {
        #[derive(Deserialize, Debug)]
        struct Config {
            host: String,
            port: u16,
        }

        let source = "server App:\n    host = $ENV.HOST | \"localhost\"\n    port = 3000\n";
        let file = parse_to_ast(source).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };

        let resolver = ValueResolver::new(|_| None);
        let config: Config = from_body_resolved(body, &resolver).unwrap();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 3000);
    }

    #[test]
    fn deserialize_named_item_after_scalar_shared_merge() {
        #[derive(Deserialize, Debug, PartialEq, Eq)]
        #[serde(rename_all = "camelCase")]
        struct Step {
            interval: u64,
            name: String,
        }

        let source = "workflow W:\n    .interval = 42\n    - S:\n        name = \"x\"\n";
        let file = parse_to_ast(source).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };
        let resolver = ValueResolver::env();
        let resolved = resolver.resolve_body(body).unwrap();
        let merged = resolve::apply_shared_properties(&resolved);
        let item = merged
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::ListItem(li) => Some(li),
                _ => None,
            })
            .expect("list item");
        if let ListItemKind::Named { body, .. } = &item.kind {
            let step: Step = from_body(body).unwrap();
            assert_eq!(
                step,
                Step {
                    interval: 42,
                    name: "x".into(),
                }
            );
        } else {
            panic!("expected named item");
        }
    }

    // -------------------------------------------------------------------
    // Phase 2: Serde bridge robustness tests
    // -------------------------------------------------------------------

    #[test]
    fn missing_required_field_error() {
        #[derive(Deserialize, Debug)]
        struct Config {
            host: String,
            port: u16,
        }

        // Control: the complete source deserializes, proving the struct
        // shape is right and the failure below is really the missing field.
        let complete = "service App:\n    host = \"localhost\"\n    port = 8080\n";
        let file = parse_to_ast(complete).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 8080);

        let source = "service App:\n    host = \"localhost\"\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let result: Result<Config, _> = from_body(body);
        let err = result.expect_err("missing required field 'port' should error");
        assert!(err.to_string().contains("port"), "got: {err}");
    }

    #[test]
    fn extra_unknown_fields_ignored() {
        #[derive(Deserialize, Debug)]
        struct Config {
            host: String,
        }

        let source = "service App:\n    host = \"localhost\"\n    port = 8080\n    debug = true\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.host, "localhost");
    }

    #[test]
    fn type_mismatch_number_as_string() {
        let v = Value::number(crate::num!(42.0));
        let result: Result<String, _> = from_value(&v);
        assert!(result.is_err());
    }

    #[test]
    fn type_mismatch_string_as_bool() {
        let v = Value::String("hello".into());
        let result: Result<bool, _> = from_value(&v);
        assert!(result.is_err());
    }

    #[test]
    fn type_mismatch_bool_as_array() {
        let v = Value::Bool(true);
        let result: Result<Vec<String>, _> = from_value(&v);
        assert!(result.is_err());
    }

    #[test]
    fn deeply_nested_four_levels() {
        #[derive(Deserialize, Debug)]
        struct L1 {
            l2: L2,
        }
        #[derive(Deserialize, Debug)]
        struct L2 {
            l3: L3,
        }
        #[derive(Deserialize, Debug)]
        struct L3 {
            value: String,
        }

        let source = "root R:\n    l2:\n        l3:\n            value = \"deep\"\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("root", "R").body().unwrap();
        let config: L1 = from_body(body).unwrap();
        assert_eq!(config.l2.l3.value, "deep");
    }

    #[test]
    fn empty_body_all_optional() {
        #[derive(Deserialize, Debug)]
        struct Config {
            #[serde(default)]
            host: Option<String>,
            #[serde(default)]
            port: Option<u16>,
        }

        let source = "service App:\n    x = 1\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert!(config.host.is_none());
        assert!(config.port.is_none());
    }

    #[test]
    fn camel_case_rename() {
        #[derive(Deserialize, Debug)]
        #[serde(rename_all = "camelCase")]
        struct Config {
            api_key: String,
            max_retries: u32,
        }

        let source = "service App:\n    apiKey = \"abc\"\n    maxRetries = 3\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.api_key, "abc");
        assert_eq!(config.max_retries, 3);
    }

    #[test]
    fn from_body_resolved_unresolvable_env_propagates_error() {
        #[derive(Deserialize, Debug)]
        struct Config {
            secret: String,
        }

        let source = "service App:\n    secret = $ENV.NONEXISTENT_NML_TEST_VAR_XYZ\n";
        let file = parse_to_ast(source).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };

        let resolver = ValueResolver::env();
        let result: Result<Config, _> = from_body_resolved(body, &resolver);
        assert!(
            result.is_err(),
            "unresolvable env var should propagate error"
        );

        // Control: with a fallback the same shape resolves cleanly.
        let with_fallback =
            "service App:\n    secret = $ENV.NONEXISTENT_NML_TEST_VAR_XYZ | \"fallback\"\n";
        let file = parse_to_ast(with_fallback).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };
        let config: Config = from_body_resolved(body, &resolver).unwrap();
        assert_eq!(config.secret, "fallback");
    }

    #[test]
    fn optional_field_absent() {
        #[derive(Deserialize, Debug)]
        struct Config {
            host: String,
            #[serde(default)]
            port: Option<u16>,
        }

        let source = "service App:\n    host = \"localhost\"\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.host, "localhost");
        assert!(config.port.is_none());
    }

    #[test]
    fn optional_field_present() {
        #[derive(Deserialize, Debug)]
        struct Config {
            host: String,
            port: Option<u16>,
        }

        let source = "service App:\n    host = \"localhost\"\n    port = 3000\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, Some(3000));
    }

    #[test]
    fn deserialize_f32() {
        // 2.5 is exactly representable in f32, so equality is exact.
        let v = Value::number(crate::num!(2.5));
        let result: f32 = from_value(&v).unwrap();
        assert_eq!(result, 2.5f32);
    }

    #[test]
    fn deserialize_negative_integer() {
        let v = Value::number(crate::num!(-42.0));
        let result: i32 = from_value(&v).unwrap();
        assert_eq!(result, -42);
    }

    #[test]
    fn deserialize_unsigned_rejects_negative() {
        let v = Value::number(crate::num!(-1.0));
        let result: Result<u16, _> = from_value(&v);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_duration_as_string() {
        let v = Value::Duration("30s".into());
        let result: String = from_value(&v).unwrap();
        assert_eq!(result, "30s");
    }

    #[test]
    fn deserialize_secret_as_string() {
        let v = Value::Secret("$ENV.KEY".into());
        let result: String = from_value(&v).unwrap();
        assert_eq!(result, "$ENV.KEY");
    }

    #[test]
    fn nested_block_with_items_and_properties_mixed() {
        #[derive(Deserialize, Debug)]
        struct Workflow {
            entrypoint: String,
            steps: Vec<Step>,
        }
        #[derive(Deserialize, Debug)]
        struct Step {
            name: String,
            provider: String,
        }

        let source = r#"
workflow W:
    entrypoint = "step1"
    steps:
        - step1:
            provider = "fast"
        - step2:
            provider = "slow"
"#;
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("workflow", "W").body().unwrap();
        let config: Workflow = from_body(body).unwrap();
        assert_eq!(config.entrypoint, "step1");
        assert_eq!(config.steps.len(), 2);
        assert_eq!(config.steps[0].name, "step1");
        assert_eq!(config.steps[0].provider, "fast");
        assert_eq!(config.steps[1].name, "step2");
        assert_eq!(config.steps[1].provider, "slow");
    }

    #[test]
    fn deserialize_money_value_as_string() {
        #[derive(Deserialize, Debug)]
        struct Plan {
            #[serde(rename = "monthlyPrice")]
            monthly_price: String,
        }
        let source = "plan ProPlan:\n    monthlyPrice = 29.99 USD\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("plan", "ProPlan").body().unwrap();
        let plan: Plan = from_body(body).unwrap();
        assert_eq!(plan.monthly_price, "29.99 USD");
    }

    #[test]
    fn string_coerces_to_u16() {
        #[derive(Deserialize, Debug)]
        struct Config {
            port: u16,
        }
        let source = "server S:\n    port = \"3000\"\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("server", "S").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.port, 3000);
    }

    #[test]
    fn resolved_env_var_coerces_to_number() {
        use crate::resolve::ValueResolver;

        #[derive(Deserialize, Debug)]
        struct Config {
            port: u16,
        }

        let source = "server S:\n    port = $ENV.PORT | 8080\n";
        let file = parse_to_ast(source).unwrap();
        let resolver = ValueResolver::new(|key| match key {
            "PORT" => Some("3000".into()),
            _ => None,
        });
        let doc = Document::new(&file);
        let body = doc.block("server", "S").body().unwrap();
        let config: Config = from_body_resolved(body, &resolver).unwrap();
        assert_eq!(config.port, 3000);
    }

    #[test]
    fn resolved_env_var_fallback_uses_number() {
        use crate::resolve::ValueResolver;

        #[derive(Deserialize, Debug)]
        struct Config {
            port: u16,
        }

        let source = "server S:\n    port = $ENV.PORT | 8080\n";
        let file = parse_to_ast(source).unwrap();
        let resolver = ValueResolver::new(|_| None);
        let doc = Document::new(&file);
        let body = doc.block("server", "S").body().unwrap();
        let config: Config = from_body_resolved(body, &resolver).unwrap();
        assert_eq!(config.port, 8080);
    }

    #[test]
    fn string_coerces_to_bool() {
        #[derive(Deserialize, Debug)]
        struct Config {
            enabled: bool,
        }
        let source = "server S:\n    enabled = \"true\"\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("server", "S").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert!(config.enabled);
    }

    #[test]
    fn non_numeric_string_rejects_as_number() {
        let result = parse_port("server App:\n    port = \"not-a-number\"\n");
        assert!(result.is_err());
    }

    // --- Numeric range and fractional validation ---

    /// Shared fixture for the u16 range tests below; the success tests
    /// read `port`, the rejection tests only need the field to exist.
    #[derive(Deserialize, Debug)]
    struct PortConfig {
        port: u16,
    }

    fn parse_port(nml: &str) -> Result<PortConfig, Error> {
        let file = parse_to_ast(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        from_body(body)
    }

    // ── RFC 0014: role-conjunction expressions (`&`) ──────────────────────

    #[derive(Deserialize, Debug)]
    struct AclConfig {
        allow: Vec<String>,
    }

    fn parse_allow(nml: &str) -> Result<AclConfig, Error> {
        let file = parse_to_ast(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        from_body(body)
    }

    /// THE cross-repo contract (RFC 0014): a role-conjunction expression
    /// lowers to ONE string in canonical `" & "`-joined form — single-spaced
    /// regardless of source spacing — because that exact text is what
    /// consumers (nudge's selector parser, its `Display`, its effective-
    /// policy output) parse and emit. If this test changes shape, the
    /// consumer contract changes with it.
    #[test]
    fn role_conjunction_lowers_to_canonical_joined_text() {
        // Inline array, canonical spacing.
        let c = parse_allow(
            "server App:
    allow = [@role/a & @role/b, @plan/Pro]
",
        )
        .unwrap();
        assert_eq!(c.allow, vec!["@role/a & @role/b", "@plan/Pro"]);

        // Tight spacing canonicalizes: `&` is never part of a role token, so
        // the lexer splits regardless and lowering re-joins single-spaced.
        let c = parse_allow(
            "server App:
    allow = [@role/a&@role/b]
",
        )
        .unwrap();
        assert_eq!(c.allow, vec!["@role/a & @role/b"]);

        // Block-list items take the same expression and the same canonical form.
        let c = parse_allow(
            "server App:
    allow:
        - @role/a & @plan/Pro
",
        )
        .unwrap();
        assert_eq!(c.allow, vec!["@role/a & @plan/Pro"]);

        // Three atoms chain.
        let c = parse_allow(
            "server App:
    allow = [@role/a & @role/b & @scope/x]
",
        )
        .unwrap();
        assert_eq!(c.allow, vec!["@role/a & @role/b & @scope/x"]);

        // A scalar (non-list) value position composes too.
        #[derive(Deserialize, Debug)]
        struct One {
            gate: String,
        }
        let file = parse_to_ast(
            "server App:
    gate = @role/a & @role/b
",
        )
        .unwrap();
        let doc = crate::query::Document::new(&file);
        let one: One = from_body(doc.block("server", "App").body().unwrap()).unwrap();
        assert_eq!(one.gate, "@role/a & @role/b");
    }

    /// Malformed conjunctions are parse errors with targeted guidance —
    /// never silently swallowed tokens (the language-level mirror of the
    /// consumer's no-silent-misparse rule).
    #[test]
    fn role_conjunction_errors_are_targeted() {
        assert!(
            parse_to_ast(
                "server App:
    allow = [@role/a &]
"
            )
            .is_err()
        );
        assert!(
            parse_to_ast(
                "server App:
    allow = [& @role/a]
"
            )
            .is_err()
        );
        let err = parse_to_ast(
            "server App:
    allow = [@role/a && @role/b]
",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("&&"), "targeted '&&' guidance, got: {err}");
    }

    #[test]
    fn test_u16_valid_port() {
        let config = parse_port("server App:\n    port = 8080\n").unwrap();
        assert_eq!(config.port, 8080);
    }

    #[test]
    fn test_u16_boundary_max() {
        let config = parse_port("server App:\n    port = 65535\n").unwrap();
        assert_eq!(config.port, 65535);
    }

    #[test]
    fn test_u16_overflow_rejected() {
        let result = parse_port("server App:\n    port = 70000\n");
        assert!(result.is_err(), "70000 should not fit in u16");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("out of range"), "got: {}", msg);
    }

    #[test]
    fn test_u16_negative_rejected() {
        let result = parse_port("server App:\n    port = -1\n");
        assert!(result.is_err(), "-1 should not fit in u16");
    }

    #[test]
    fn test_u16_fractional_rejected() {
        let result = parse_port("server App:\n    port = 3000.5\n");
        assert!(
            result.is_err(),
            "fractional values should not be valid for u16"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("fractional"), "got: {}", msg);
    }

    #[test]
    fn test_u8_boundary() {
        #[derive(Deserialize, Debug)]
        struct Config {
            level: u8,
        }
        let nml = "server App:\n    level = 255\n";
        let file = parse_to_ast(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.level, 255);

        let nml_bad = "server App:\n    level = 256\n";
        let file2 = parse_to_ast(nml_bad).unwrap();
        let doc2 = crate::query::Document::new(&file2);
        let body2 = doc2.block("server", "App").body().unwrap();
        let result: Result<Config, _> = from_body(body2);
        assert!(result.is_err(), "256 should not fit in u8");
    }

    #[test]
    fn test_i8_range() {
        #[derive(Deserialize, Debug)]
        struct Config {
            offset: i8,
        }
        let nml = "server App:\n    offset = -128\n";
        let file = parse_to_ast(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.offset, -128);

        let nml_bad = "server App:\n    offset = 128\n";
        let file2 = parse_to_ast(nml_bad).unwrap();
        let doc2 = crate::query::Document::new(&file2);
        let body2 = doc2.block("server", "App").body().unwrap();
        let result: Result<Config, _> = from_body(body2);
        assert!(result.is_err(), "128 should not fit in i8");
    }

    /// Shared fixture for the u64 range tests; mirrors [`PortConfig`].
    #[derive(Deserialize, Debug)]
    struct CountConfig {
        count: u64,
    }

    fn parse_count(nml: &str) -> Result<CountConfig, Error> {
        let file = parse_to_ast(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        from_body(body)
    }

    #[test]
    fn test_u64_zero_valid() {
        let config = parse_count("server App:\n    count = 0\n").unwrap();
        assert_eq!(config.count, 0);
    }

    #[test]
    fn test_u64_negative_rejected() {
        let result = parse_count("server App:\n    count = -5\n");
        assert!(result.is_err(), "negative should not fit in u64");
    }

    // --- Direct conversion function tests ---

    #[test]
    fn test_number_to_int_from_int() {
        assert_eq!(
            number_to_int::<u16>(Number::from(3000), "u16").unwrap(),
            3000
        );
        assert_eq!(
            number_to_int::<u16>(Number::from(65535), "u16").unwrap(),
            u16::MAX
        );
        assert!(number_to_int::<u16>(Number::from(65536), "u16").is_err());
        assert!(number_to_int::<u16>(Number::from(-1), "u16").is_err());
        assert_eq!(
            number_to_int::<i32>(Number::from(-2147483648), "i32").unwrap(),
            i32::MIN
        );
        assert!(number_to_int::<i32>(Number::from(2147483648), "i32").is_err());
    }

    #[test]
    fn test_number_to_int_from_fraction_form() {
        // Value-based integrality: fraction-form members with integral
        // values convert; real fractions error with the precise reason.
        assert_eq!(
            number_to_int::<u32>(crate::num!(4294967295.0), "u32").unwrap(),
            u32::MAX
        );
        assert!(number_to_int::<u32>(crate::num!(4294967296.0), "u32").is_err());
        let err = number_to_int::<u32>(crate::num!(1.1), "u32").unwrap_err();
        assert!(err.to_string().contains("fractional part"), "{err}");
    }

    #[test]
    fn test_number_to_int_wide_targets() {
        // u64 above i64::MAX no longer funnels through i64 (RFC 0016).
        assert_eq!(
            number_to_int::<u64>("18446744073709551615".parse::<Number>().unwrap(), "u64").unwrap(),
            u64::MAX
        );
        // i128/u128 targets are first-class.
        assert_eq!(
            number_to_int::<i128>("18446744073709551617".parse::<Number>().unwrap(), "i128")
                .unwrap(),
            18446744073709551617_i128
        );
        let two_e38 = format!("2{}", "0".repeat(38)).parse::<Number>().unwrap();
        assert_eq!(
            number_to_int::<u128>(two_e38, "u128").unwrap(),
            2 * 10u128.pow(38)
        );
        let err = number_to_int::<u64>(two_e38, "u64").unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    /// RFC 0016 §1.7 tier 3: the exact-capture handshake through NML's
    /// own bridge — `Number` fields never detour through f64.
    #[test]
    fn test_number_field_exact_capture_handshake() {
        #[derive(Deserialize)]
        struct P {
            rate: crate::types::Number,
            env_like: crate::types::Number,
            big: crate::types::Number,
            neg_scale: crate::types::Number,
            extreme: crate::types::Number,
        }
        // String properties follow the §1.4 liberal coercion grammar,
        // like every other numeric target.
        let source = "service App:\n    rate = 2.50\n    env_like = \"1e-6\"\n    big = 18446744073709551617\n    neg_scale = \"2e38\"\n    extreme = \"1e-6176\"\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let p: P = from_body(body).unwrap();
        assert_eq!(
            (p.rate.coeff(), p.rate.scale()),
            (250, 2),
            "scale preserved"
        );
        assert_eq!(
            (p.env_like.coeff(), p.env_like.scale()),
            (1, 6),
            "coercion exact"
        );
        assert_eq!(p.big.to_u128(), Some(18446744073709551617), "2^64+1 exact");
        // The compact member encoding preserves the stored cohort MEMBER,
        // not just the value — the plain form would degrade (2, −38) to
        // (2×10^33, −5) — and captures extreme-scale members without
        // materializing their ~6 KB plain form.
        assert_eq!(
            (p.neg_scale.coeff(), p.neg_scale.scale()),
            (2, -38),
            "member preserved"
        );
        assert_eq!(
            (p.extreme.coeff(), p.extreme.scale()),
            (1, 6176),
            "extreme member exact"
        );
    }

    /// RFC 0016 §1.7 tier 3, pinned regression: inside serde's buffering
    /// containers (`#[serde(untagged)]` — the serde_json#505 class, same
    /// machinery as tag/flatten) the handshake degrades to the
    /// foreign-format arms: capture stays VALUE-exact via the f64
    /// shortest-round-trip rule for in-range decimals, but the stored
    /// member (written scale) is not guaranteed. The boundary is wider
    /// than `Number` alone (pre-existing, not RFC 0016 regression):
    /// buffering bypasses this Deserializer's coercion ladders entirely,
    /// so e.g. a string `"3000"` or fraction-form `3000.0` into a `u16`
    /// field also fails inside flatten/untagged where the direct path
    /// succeeds — serde's `Content` replays only the plain visit calls.
    /// This test documents the boundary so a future serde change
    /// surfaces loudly.
    #[test]
    fn test_number_field_buffered_container_degrades_to_value_equal() {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Buffered {
            Num { rate: crate::types::Number },
        }
        let source = "service App:\n    rate = 2.50\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let Buffered::Num { rate } = from_body(body).unwrap();
        assert_eq!(
            rate,
            crate::num!(2.5),
            "value equality must survive buffering"
        );
    }

    /// Redacted messages must still LOCATE: the de layer builds a field
    /// path as nested deserializers unwind, so a failure names its site
    /// without ever echoing the value (errors previously had neither).
    #[test]
    fn test_errors_carry_field_paths() {
        #[derive(Deserialize, Debug)]
        struct Db {
            #[serde(rename = "port")]
            _port: u8,
        }
        #[derive(Deserialize, Debug)]
        struct Cfg {
            #[serde(rename = "database")]
            _database: Db,
        }
        let source = "service App:\n    database:\n        port = 70000\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let err = from_body::<Cfg>(body).unwrap_err().to_string();
        assert!(
            err.contains("field `database`") && err.contains("field `port`"),
            "nested path must locate the failure: {err}"
        );
        assert!(err.contains("out of range"), "reason survives: {err}");
        assert!(!err.contains("70000"), "value must not leak: {err}");

        // Array elements locate by position.
        #[derive(Deserialize, Debug)]
        struct Ports {
            #[serde(rename = "ports")]
            _ports: Vec<u8>,
        }
        let source = "service App:\n    ports = [1, 2, 70000]\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let err = from_body::<Ports>(body).unwrap_err().to_string();
        assert!(
            err.contains("field `ports`") && err.contains("element 2"),
            "element position must locate the failure: {err}"
        );

        // Named list items: the label AND the inner field both appear —
        // NamedItemMapAccess has its own value path, which the first cut
        // of this feature missed (caught by probe; pinned here).
        #[derive(Deserialize, Debug)]
        struct Ep {
            #[serde(rename = "port")]
            _port: u8,
        }
        #[derive(Deserialize, Debug)]
        struct Eps {
            #[serde(rename = "endpoints")]
            _e: Vec<Ep>,
        }
        let source = "service App:\n    endpoints:\n        - Api:\n            port = 70000\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let err = from_body::<Eps>(body).unwrap_err().to_string();
        assert!(
            err.contains("field `endpoints`")
                && err.contains("field `Api`")
                && err.contains("field `port`"),
            "named-item path must be complete: {err}"
        );
        assert!(!err.contains("70000"), "value must not leak: {err}");
    }

    /// Fixed-size targets must never silently swallow config: an array
    /// longer than the tuple/array target it deserializes into is a
    /// hard error, not a truncation (implementation-review finding).
    #[test]
    fn test_fixed_size_targets_reject_extra_elements() {
        #[derive(Deserialize, Debug)]
        struct T {
            #[serde(rename = "x")]
            _x: (bool, bool),
        }
        let source = "service App:\n    x = [true, false, true]\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let err = from_body::<T>(body).unwrap_err().to_string();
        assert!(
            err.contains("3 element(s)") && err.contains("only 2"),
            "extra elements must be reported, got: {err}"
        );

        // The exact-arity case still deserializes.
        let source = "service App:\n    x = [true, false]\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        assert!(from_body::<T>(body).is_ok(), "exact arity must succeed");

        // Vec targets consume everything — unaffected.
        #[derive(Deserialize, Debug)]
        struct V {
            #[serde(rename = "x")]
            _x: Vec<bool>,
        }
        let source = "service App:\n    x = [true, false, true]\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        assert!(from_body::<V>(body).is_ok(), "Vec must accept any length");
    }

    /// Security posture (implementation review): serde-generated errors
    /// must never echo values — a resolved `$ENV` secret is a plain
    /// string by the time serde sees it, and both the unknown-variant
    /// and invalid-type channels used to interpolate it verbatim.
    #[test]
    fn test_serde_errors_never_echo_values() {
        #[derive(Deserialize, Debug)]
        #[serde(rename_all = "lowercase")]
        enum Backend {
            Memory,
            Postgres,
        }
        #[derive(Deserialize, Debug)]
        struct C {
            #[serde(rename = "backend")]
            _backend: Backend,
        }
        let source = "service App:\n    backend = \"s3cr3t-prod-pw\"\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let err = from_body::<C>(body).unwrap_err().to_string();
        assert!(!err.contains("s3cr3t"), "variant value leaked: {err}");
        assert!(
            err.contains("memory") && err.contains("postgres"),
            "expected-variant list is the actionable half: {err}"
        );

        // invalid_type channel: a string handed to a char-typed field.
        #[derive(Deserialize, Debug)]
        struct D {
            #[serde(rename = "x")]
            _x: char,
        }
        let source = "service App:\n    x = \"hunter2-XYZZY\"\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let err = from_body::<D>(body).unwrap_err().to_string();
        assert!(!err.contains("hunter2"), "string value leaked: {err}");
        assert!(err.contains("a string"), "kind survives redaction: {err}");

        // The 128-bit channel: serde's DEFAULT visit_i128/visit_u128 build
        // `Unexpected::Other("integer `<value>` as i128")` — the value is
        // inside the description, so the Other arm must collapse it.
        let source = "service App:\n    x = 99228162514264337593543950336\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let err = from_body::<D>(body).unwrap_err().to_string();
        assert!(
            !err.contains("99228162514264337593543950336"),
            "128-bit value leaked via Unexpected::Other: {err}"
        );
        assert!(err.contains("an integer"), "kind survives redaction: {err}");
    }

    #[test]
    fn test_large_integer_roundtrip_end_to_end() {
        // 2^53 + 1: the old f64-based pipeline silently turned this
        // into 9007199254740992.
        #[derive(Deserialize)]
        struct Config {
            id: i64,
            max: i64,
        }
        let source = "service App:\n    id = 9007199254740993\n    max = 9223372036854775807\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.id, 9_007_199_254_740_993);
        assert_eq!(config.max, i64::MAX);
    }

    #[test]
    fn test_large_integer_string_coercion_exact() {
        // Env vars resolve to strings; the string -> integer path must
        // also avoid the f64 detour.
        let v = Value::String("9007199254740993".into());
        let n: i64 = from_value(&v).unwrap();
        assert_eq!(n, 9_007_199_254_740_993);
        let v = Value::String("9223372036854775807".into());
        let n: u64 = from_value(&v).unwrap();
        assert_eq!(n, i64::MAX as u64);
    }

    #[test]
    fn test_number_to_int_full_i64_range_exact() {
        // The entire i64 range survives exactly -- the old f64 round-trip
        // corrupted anything above 2^53.
        assert_eq!(
            number_to_int::<i64>(Number::from(i64::MAX), "i64").unwrap(),
            i64::MAX
        );
        assert_eq!(
            number_to_int::<i64>(Number::from(i64::MIN), "i64").unwrap(),
            i64::MIN
        );
        assert_eq!(
            number_to_int::<i64>(Number::from(9_007_199_254_740_993), "i64").unwrap(),
            9_007_199_254_740_993
        );
    }

    #[test]
    fn test_deserialize_value_role_to_string() {
        #[derive(Deserialize, Debug)]
        struct Config {
            access: String,
        }
        let source = "service App:\n    access = @role/admin\n";
        let file = crate::cst::parse_to_ast(source).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.access, "@role/admin");
    }

    #[test]
    fn test_deserialize_list_item_role_to_vec_string() {
        #[derive(Deserialize, Debug)]
        struct Config {
            members: Vec<String>,
        }
        let source = "role admin:\n    members:\n        - @role/editor\n        - @public\n";
        let file = crate::cst::parse_to_ast(source).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("role", "admin").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.members, vec!["@role/editor", "@public"]);
    }

    // --- Unit-variant enum deserialization ---

    /// Shared fixture for the enum tests below.
    #[derive(Deserialize, Debug, PartialEq)]
    #[serde(rename_all = "lowercase")]
    enum Backend {
        Memory,
        Postgres,
    }

    #[derive(Deserialize, Debug)]
    struct BackendConfig {
        backend: Backend,
    }

    fn parse_backend(nml: &str) -> Result<BackendConfig, Error> {
        let file = parse_to_ast(nml).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        from_body(body)
    }

    #[test]
    fn deserialize_enum_from_string_value() {
        let config = parse_backend("server App:\n    backend = \"postgres\"\n").unwrap();
        assert_eq!(config.backend, Backend::Postgres);
        let config = parse_backend("server App:\n    backend = \"memory\"\n").unwrap();
        assert_eq!(config.backend, Backend::Memory);
    }

    #[test]
    fn deserialize_enum_unknown_variant_error() {
        let err = parse_backend("server App:\n    backend = \"redis\"\n")
            .expect_err("unknown variant 'redis' should error");
        let msg = err.to_string();
        assert!(
            msg.contains("memory") && msg.contains("postgres"),
            "error should name the valid variants, got: {msg}"
        );
    }

    #[test]
    fn deserialize_enum_camel_case_rename() {
        #[derive(Deserialize, Debug, PartialEq)]
        #[serde(rename_all = "camelCase")]
        enum Mode {
            AllInOne,
            ControlPlane,
            WorkerOnly,
        }

        #[derive(Deserialize, Debug)]
        struct Config {
            mode: Mode,
        }

        let source = "server App:\n    mode = \"allInOne\"\n";
        let file = parse_to_ast(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let config: Config = from_body(body).unwrap();
        assert_eq!(config.mode, Mode::AllInOne);
    }

    #[test]
    fn deserialize_enum_number_value_rejected() {
        let err = parse_backend("server App:\n    backend = 42\n")
            .expect_err("number should not be accepted as enum");
        let msg = err.to_string();
        assert!(
            msg.contains("expected string for enum"),
            "error should explain the type mismatch, got: {msg}"
        );
    }
}
