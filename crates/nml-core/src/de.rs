//! Serde `Deserialize` bridge for NML values.
//!
//! Converts NML block bodies into Rust structs via serde deserialization.
//! Supports flat properties, nested blocks (recursive), named list items
//! with label injection, and shared property inheritance (`.key:` blocks and `.key = value` scalars).
//!
//! # Example
//!
//! ```rust
//! use serde::Deserialize;
//! use nml_core::de::from_block;
//! use nml_core::query::Document;
//!
//! #[derive(Deserialize)]
//! struct ServerConfig {
//!     port: f64,
//!     host: String,
//!     debug: bool,
//! }
//!
//! let source = r#"
//! service MyApp:
//!     port = 8080
//!     host = "localhost"
//!     debug = true
//! "#;
//! let file = nml_core::parse(source).unwrap();
//! let doc = Document::new(&file);
//! let body = doc.block("service", "MyApp").body().unwrap();
//! let config: ServerConfig = from_block(body).unwrap();
//! assert_eq!(config.port, 8080.0);
//! assert_eq!(config.host, "localhost");
//! ```

use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

use crate::ast::*;
use crate::resolve::{self, ValueResolver};
use crate::template;
use crate::types::Value;

/// Errors that can occur during NML deserialization.
#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Error {}

impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error(msg.to_string())
    }
}

fn f64_to_u8(n: f64) -> Result<u8, Error> {
    if n.fract() != 0.0 {
        return Err(Error(format!("u8 value {} has a fractional part", n)));
    }
    if n < 0.0 || n > u8::MAX as f64 {
        return Err(Error(format!("u8 value {} out of range (0..=255)", n)));
    }
    Ok(n as u8)
}

fn f64_to_u16(n: f64) -> Result<u16, Error> {
    if n.fract() != 0.0 {
        return Err(Error(format!("u16 value {} has a fractional part", n)));
    }
    if n < 0.0 || n > u16::MAX as f64 {
        return Err(Error(format!("u16 value {} out of range (0..=65535)", n)));
    }
    Ok(n as u16)
}

fn f64_to_u32(n: f64) -> Result<u32, Error> {
    if n.fract() != 0.0 {
        return Err(Error(format!("u32 value {} has a fractional part", n)));
    }
    if n < 0.0 || n > u32::MAX as f64 {
        return Err(Error(format!("u32 value {} out of range (0..=4294967295)", n)));
    }
    Ok(n as u32)
}

fn f64_to_u64(n: f64) -> Result<u64, Error> {
    if n.fract() != 0.0 {
        return Err(Error(format!("u64 value {} has a fractional part", n)));
    }
    if n < 0.0 || n > u64::MAX as f64 {
        return Err(Error(format!("u64 value {} out of range (0..={})", n, u64::MAX)));
    }
    Ok(n as u64)
}

fn f64_to_i8(n: f64) -> Result<i8, Error> {
    if n.fract() != 0.0 {
        return Err(Error(format!("i8 value {} has a fractional part", n)));
    }
    if n < i8::MIN as f64 || n > i8::MAX as f64 {
        return Err(Error(format!("i8 value {} out of range (-128..=127)", n)));
    }
    Ok(n as i8)
}

fn f64_to_i16(n: f64) -> Result<i16, Error> {
    if n.fract() != 0.0 {
        return Err(Error(format!("i16 value {} has a fractional part", n)));
    }
    if n < i16::MIN as f64 || n > i16::MAX as f64 {
        return Err(Error(format!("i16 value {} out of range (-32768..=32767)", n)));
    }
    Ok(n as i16)
}

fn f64_to_i32(n: f64) -> Result<i32, Error> {
    if n.fract() != 0.0 {
        return Err(Error(format!("i32 value {} has a fractional part", n)));
    }
    if n < i32::MIN as f64 || n > i32::MAX as f64 {
        return Err(Error(format!("i32 value {} out of range (-2147483648..=2147483647)", n)));
    }
    Ok(n as i32)
}

fn f64_to_i64(n: f64) -> Result<i64, Error> {
    if n.fract() != 0.0 {
        return Err(Error(format!("i64 value {} has a fractional part", n)));
    }
    if n < i64::MIN as f64 || n > i64::MAX as f64 {
        return Err(Error(format!("i64 value {} out of range ({}..={})", n, i64::MIN, i64::MAX)));
    }
    Ok(n as i64)
}

/// Deserialize a struct from an NML block body.
pub fn from_block<'de, T: Deserialize<'de>>(body: &'de Body) -> Result<T, Error> {
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
/// Pipeline: `resolve_body` -> `apply_shared_properties` -> `from_block`.
pub fn from_body_resolved<T: for<'de> Deserialize<'de>>(
    body: &Body,
    resolver: &ValueResolver,
) -> Result<T, Error> {
    let resolved = resolver
        .resolve_body(body)
        .map_err(|e| Error(e.to_string()))?;
    let merged = resolve::apply_shared_properties(&resolved);
    from_block(&merged)
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

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct enum identifier ignored_any
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
                SharedPropertyKind::Block(b) => {
                    Some(BodyMapEntry::Block(b, sp.name.name.as_str()))
                }
                SharedPropertyKind::Scalar(sv) => Some(BodyMapEntry::SharedScalar {
                    key: sp.name.name.as_str(),
                    value: sv,
                }),
            },
            _ => None,
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
        match entry {
            BodyMapEntry::Property(prop) => seed.deserialize(ValueDeserializer {
                value: &prop.value.value,
            }),
            BodyMapEntry::Block(body, _) => {
                seed.deserialize(NestedBlockDeserializer { body })
            }
            BodyMapEntry::SharedScalar { value, .. } => seed.deserialize(ValueDeserializer {
                value: &value.value,
            }),
        }
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

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_unit()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string
        bytes byte_buf unit unit_struct newtype_struct tuple
        tuple_struct enum identifier
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
        self.index += 1;

        match &item.kind {
            ListItemKind::Named { name, body } => {
                seed.deserialize(NamedItemDeserializer {
                    label: &name.name,
                    body,
                })
                .map(Some)
            }
            ListItemKind::Shorthand(val) => seed
                .deserialize(ValueDeserializer {
                    value: &val.value,
                })
                .map(Some),
            ListItemKind::Reference(ident) => seed
                .deserialize(de::value::StrDeserializer::<Error>::new(&ident.name))
                .map(Some),
            ListItemKind::Role(s) => seed
                .deserialize(de::value::StrDeserializer::<Error>::new(s))
                .map(Some),
        }
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
        let has_explicit_name = self.body.entries.iter().any(|e| matches!(
            &e.kind,
            BodyEntryKind::Property(p) if p.name.name == "name"
        ));

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

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_unit()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string
        bytes byte_buf unit unit_struct newtype_struct seq tuple
        tuple_struct enum identifier
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
            return seed.deserialize(de::value::StrDeserializer::<Error>::new(self.label));
        }

        let entry = &self.body_entries[self.body_index];
        self.body_index += 1;
        match entry {
            BodyMapEntry::Property(prop) => seed.deserialize(ValueDeserializer {
                value: &prop.value.value,
            }),
            BodyMapEntry::Block(body, _) => {
                seed.deserialize(NestedBlockDeserializer { body })
            }
            BodyMapEntry::SharedScalar { value, .. } => seed.deserialize(ValueDeserializer {
                value: &value.value,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Value deserializer
// ---------------------------------------------------------------------------

/// Coerce a string-typed Value to f64 if parseable.
/// Env vars resolve to Value::String, so `$ENV.PORT` = "3000" needs to
/// deserialize into numeric fields.
fn coerce_to_f64(value: &Value) -> Option<f64> {
    match value {
        Value::String(s) | Value::Secret(s) | Value::Duration(s) | Value::Path(s) => {
            s.parse::<f64>().ok()
        }
        _ => None,
    }
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
            Value::Number(n) => {
                if n.fract() == 0.0 && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
                    visitor.visit_i64(*n as i64)
                } else {
                    visitor.visit_f64(*n)
                }
            }
            Value::Bool(b) => visitor.visit_bool(*b),
            Value::Duration(s) | Value::Path(s) | Value::Secret(s) | Value::Role(s) => {
                visitor.visit_str(s)
            }
            Value::Reference(s) => visitor.visit_str(s),
            Value::Money(m) => visitor.visit_string(m.format_display()),
            Value::Array(items) => {
                visitor.visit_seq(ArraySeqAccess { items, index: 0 })
            }
            Value::Fallback(primary, _) => {
                ValueDeserializer {
                    value: &primary.value,
                }
                .deserialize_any(visitor)
            }
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::Bool(b) => visitor.visit_bool(*b),
            _ => match coerce_to_bool(self.value) {
                Some(b) => visitor.visit_bool(b),
                None => Err(Error(format!(
                    "expected bool, got {}",
                    self.value.type_name()
                ))),
            },
        }
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::Number(n) => visitor.visit_f64(*n),
            _ => match coerce_to_f64(self.value) {
                Some(n) => visitor.visit_f64(n),
                None => Err(Error(format!(
                    "expected number, got {}",
                    self.value.type_name()
                ))),
            },
        }
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::Number(n) => visitor.visit_f32(*n as f32),
            _ => match coerce_to_f64(self.value) {
                Some(n) => visitor.visit_f32(n as f32),
                None => Err(Error(format!(
                    "expected number, got {}",
                    self.value.type_name()
                ))),
            },
        }
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::Number(n) => visitor.visit_i64(f64_to_i64(*n)?),
            _ => match coerce_to_f64(self.value) {
                Some(n) => visitor.visit_i64(f64_to_i64(n)?),
                None => Err(Error(format!(
                    "expected number, got {}",
                    self.value.type_name()
                ))),
            },
        }
    }

    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::Number(n) => visitor.visit_i32(f64_to_i32(*n)?),
            _ => match coerce_to_f64(self.value) {
                Some(n) => visitor.visit_i32(f64_to_i32(n)?),
                None => Err(Error(format!(
                    "expected number, got {}",
                    self.value.type_name()
                ))),
            },
        }
    }

    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::Number(n) => visitor.visit_i16(f64_to_i16(*n)?),
            _ => match coerce_to_f64(self.value) {
                Some(n) => visitor.visit_i16(f64_to_i16(n)?),
                None => Err(Error(format!(
                    "expected number, got {}",
                    self.value.type_name()
                ))),
            },
        }
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::Number(n) => visitor.visit_i8(f64_to_i8(*n)?),
            _ => match coerce_to_f64(self.value) {
                Some(n) => visitor.visit_i8(f64_to_i8(n)?),
                None => Err(Error(format!(
                    "expected number, got {}",
                    self.value.type_name()
                ))),
            },
        }
    }

    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::Number(n) => visitor.visit_u64(f64_to_u64(*n)?),
            _ => match coerce_to_f64(self.value) {
                Some(n) => visitor.visit_u64(f64_to_u64(n)?),
                None => Err(Error(format!(
                    "expected unsigned number, got {}",
                    self.value.type_name()
                ))),
            },
        }
    }

    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::Number(n) => visitor.visit_u32(f64_to_u32(*n)?),
            _ => match coerce_to_f64(self.value) {
                Some(n) => visitor.visit_u32(f64_to_u32(n)?),
                None => Err(Error(format!(
                    "expected unsigned number, got {}",
                    self.value.type_name()
                ))),
            },
        }
    }

    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::Number(n) => visitor.visit_u16(f64_to_u16(*n)?),
            _ => match coerce_to_f64(self.value) {
                Some(n) => visitor.visit_u16(f64_to_u16(n)?),
                None => Err(Error(format!(
                    "expected unsigned number, got {}",
                    self.value.type_name()
                ))),
            },
        }
    }

    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::Number(n) => visitor.visit_u8(f64_to_u8(*n)?),
            _ => match coerce_to_f64(self.value) {
                Some(n) => visitor.visit_u8(f64_to_u8(n)?),
                None => Err(Error(format!(
                    "expected unsigned number, got {}",
                    self.value.type_name()
                ))),
            },
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.value {
            Value::String(s) => visitor.visit_str(s),
            Value::TemplateString(segs) => {
                visitor.visit_string(template::segments_to_string(segs))
            }
            Value::Path(s) | Value::Duration(s) | Value::Secret(s) => visitor.visit_str(s),
            Value::Reference(s) | Value::Role(s) => visitor.visit_str(s),
            Value::Money(m) => visitor.visit_string(m.format_display()),
            _ => Err(Error(format!(
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
            Value::Array(items) => visitor.visit_seq(ArraySeqAccess { items, index: 0 }),
            _ => Err(Error(format!(
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

    serde::forward_to_deserialize_any! {
        char bytes byte_buf unit unit_struct newtype_struct
        tuple tuple_struct map struct enum identifier
    }
}

// ---------------------------------------------------------------------------
// Array sequence access
// ---------------------------------------------------------------------------

struct ArraySeqAccess<'a> {
    items: &'a [crate::types::SpannedValue],
    index: usize,
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
        self.index += 1;
        seed.deserialize(ValueDeserializer {
            value: &item.value,
        })
        .map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;
    use crate::query::Document;
    use serde::Deserialize;

    #[test]
    fn deserialize_struct_from_block() {
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "MyApp").body().unwrap();
        let config: Config = from_block(body).unwrap();
        assert_eq!(config.port, 8080.0);
        assert_eq!(config.host, "localhost");
        assert!(config.debug);
    }

    #[test]
    fn deserialize_with_array() {
        #[derive(Deserialize, Debug)]
        struct Config {
            tags: Vec<String>,
        }

        let source = "service App:\n    tags = [\"web\", \"api\"]\n";
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_block(body).unwrap();
        assert_eq!(config.tags, vec!["web", "api"]);
    }

    #[test]
    fn deserialize_value_directly() {
        let v = Value::Number(42.0);
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let config: Server = from_block(body).unwrap();
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let config: Server = from_block(body).unwrap();
        assert_eq!(config.port, 3000);
        assert!(config.db.is_none());
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("workflow", "W").body().unwrap();
        let config: Workflow = from_block(body).unwrap();
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "S").body().unwrap();

        #[derive(Deserialize, Debug)]
        struct S {
            items: Vec<Item>,
        }
        let config: S = from_block(body).unwrap();
        assert_eq!(config.items[0].name, "OverriddenName");
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_block(body).unwrap();
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "S").body().unwrap();
        let config: Tools = from_block(body).unwrap();
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "S").body().unwrap();
        let config: Config = from_block(body).unwrap();
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "S").body().unwrap();
        let config: Config = from_block(body).unwrap();

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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("auth", "MyAuth").body().unwrap();
        let config: Auth = from_block(body).unwrap();
        assert_eq!(config.provider, "oidc");
        assert_eq!(config.oidc_providers.len(), 2);
        assert_eq!(config.oidc_providers[0].name, "Google");
        assert_eq!(config.oidc_providers[0].issuer, "https://accounts.google.com");
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
            let step: Step = from_block(body).unwrap();
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

        let source = "service App:\n    host = \"localhost\"\n";
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let result: Result<Config, _> = from_block(body);
        assert!(result.is_err(), "missing required field 'port' should error");
    }

    #[test]
    fn extra_unknown_fields_ignored() {
        #[derive(Deserialize, Debug)]
        struct Config {
            host: String,
        }

        let source = "service App:\n    host = \"localhost\"\n    port = 8080\n    debug = true\n";
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_block(body).unwrap();
        assert_eq!(config.host, "localhost");
    }

    #[test]
    fn type_mismatch_number_as_string() {
        let v = Value::Number(42.0);
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("root", "R").body().unwrap();
        let config: L1 = from_block(body).unwrap();
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_block(body).unwrap();
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_block(body).unwrap();
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
        let file = parser::parse(source).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };

        let resolver = ValueResolver::env();
        let result: Result<Config, _> = from_body_resolved(body, &resolver);
        assert!(result.is_err(), "unresolvable env var should propagate error");
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_block(body).unwrap();
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_block(body).unwrap();
        assert_eq!(config.port, Some(3000));
    }

    #[test]
    fn deserialize_f32() {
        let v = Value::Number(3.14);
        let result: f32 = from_value(&v).unwrap();
        assert!((result - 3.14f32).abs() < 0.001);
    }

    #[test]
    fn deserialize_negative_integer() {
        let v = Value::Number(-42.0);
        let result: i32 = from_value(&v).unwrap();
        assert_eq!(result, -42);
    }

    #[test]
    fn deserialize_unsigned_rejects_negative() {
        let v = Value::Number(-1.0);
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("workflow", "W").body().unwrap();
        let config: Workflow = from_block(body).unwrap();
        assert_eq!(config.entrypoint, "step1");
        assert_eq!(config.steps.len(), 2);
        assert_eq!(config.steps[0].name, "step1");
    }

    #[test]
    fn deserialize_money_value_as_string() {
        #[derive(Deserialize, Debug)]
        struct Plan {
            #[serde(rename = "monthlyPrice")]
            monthly_price: String,
        }
        let source = "plan ProPlan:\n    monthlyPrice = 29.99 USD\n";
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("plan", "ProPlan").body().unwrap();
        let plan: Plan = from_block(body).unwrap();
        assert_eq!(plan.monthly_price, "29.99 USD");
    }

    #[test]
    fn string_coerces_to_u16() {
        #[derive(Deserialize, Debug)]
        struct Config {
            port: u16,
        }
        let source = "server S:\n    port = \"3000\"\n";
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("server", "S").body().unwrap();
        let config: Config = from_block(body).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
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
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("server", "S").body().unwrap();
        let config: Config = from_block(body).unwrap();
        assert!(config.enabled);
    }

    #[test]
    fn non_numeric_string_rejects_as_number() {
        #[derive(Deserialize, Debug)]
        struct Config {
            port: u16,
        }
        let source = "server S:\n    port = \"not-a-number\"\n";
        let file = parser::parse(source).unwrap();
        let doc = Document::new(&file);
        let body = doc.block("server", "S").body().unwrap();
        let result: Result<Config, _> = from_block(body);
        assert!(result.is_err());
    }

    // --- Numeric range and fractional validation ---

    #[test]
    fn test_u16_valid_port() {
        #[derive(Deserialize, Debug)]
        struct Config {
            port: u16,
        }
        let nml = "server App:\n    port = 8080\n";
        let file = parser::parse(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let config: Config = from_block(body).unwrap();
        assert_eq!(config.port, 8080);
    }

    #[test]
    fn test_u16_boundary_max() {
        #[derive(Deserialize, Debug)]
        struct Config {
            port: u16,
        }
        let nml = "server App:\n    port = 65535\n";
        let file = parser::parse(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let config: Config = from_block(body).unwrap();
        assert_eq!(config.port, 65535);
    }

    #[test]
    fn test_u16_overflow_rejected() {
        #[derive(Deserialize, Debug)]
        struct Config {
            port: u16,
        }
        let nml = "server App:\n    port = 70000\n";
        let file = parser::parse(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let result: Result<Config, _> = from_block(body);
        assert!(result.is_err(), "70000 should not fit in u16");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("out of range"), "got: {}", msg);
    }

    #[test]
    fn test_u16_negative_rejected() {
        #[derive(Deserialize, Debug)]
        struct Config {
            port: u16,
        }
        let nml = "server App:\n    port = -1\n";
        let file = parser::parse(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let result: Result<Config, _> = from_block(body);
        assert!(result.is_err(), "-1 should not fit in u16");
    }

    #[test]
    fn test_u16_fractional_rejected() {
        #[derive(Deserialize, Debug)]
        struct Config {
            port: u16,
        }
        let nml = "server App:\n    port = 3000.5\n";
        let file = parser::parse(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let result: Result<Config, _> = from_block(body);
        assert!(result.is_err(), "fractional values should not be valid for u16");
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
        let file = parser::parse(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let config: Config = from_block(body).unwrap();
        assert_eq!(config.level, 255);

        let nml_bad = "server App:\n    level = 256\n";
        let file2 = parser::parse(nml_bad).unwrap();
        let doc2 = crate::query::Document::new(&file2);
        let body2 = doc2.block("server", "App").body().unwrap();
        let result: Result<Config, _> = from_block(body2);
        assert!(result.is_err(), "256 should not fit in u8");
    }

    #[test]
    fn test_i8_range() {
        #[derive(Deserialize, Debug)]
        struct Config {
            offset: i8,
        }
        let nml = "server App:\n    offset = -128\n";
        let file = parser::parse(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let config: Config = from_block(body).unwrap();
        assert_eq!(config.offset, -128);

        let nml_bad = "server App:\n    offset = 128\n";
        let file2 = parser::parse(nml_bad).unwrap();
        let doc2 = crate::query::Document::new(&file2);
        let body2 = doc2.block("server", "App").body().unwrap();
        let result: Result<Config, _> = from_block(body2);
        assert!(result.is_err(), "128 should not fit in i8");
    }

    #[test]
    fn test_u64_zero_valid() {
        #[derive(Deserialize, Debug)]
        struct Config {
            count: u64,
        }
        let nml = "server App:\n    count = 0\n";
        let file = parser::parse(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let config: Config = from_block(body).unwrap();
        assert_eq!(config.count, 0);
    }

    #[test]
    fn test_u64_negative_rejected() {
        #[derive(Deserialize, Debug)]
        struct Config {
            count: u64,
        }
        let nml = "server App:\n    count = -5\n";
        let file = parser::parse(nml).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("server", "App").body().unwrap();
        let result: Result<Config, _> = from_block(body);
        assert!(result.is_err(), "negative should not fit in u64");
    }

    // --- Direct conversion function tests ---

    #[test]
    fn test_f64_to_u16_conversions() {
        assert!(f64_to_u16(0.0).is_ok());
        assert_eq!(f64_to_u16(3000.0).unwrap(), 3000);
        assert_eq!(f64_to_u16(65535.0).unwrap(), 65535);
        assert!(f64_to_u16(65536.0).is_err());
        assert!(f64_to_u16(-1.0).is_err());
        assert!(f64_to_u16(3000.5).is_err());
        assert!(f64_to_u16(f64::NAN).is_err());
        assert!(f64_to_u16(f64::INFINITY).is_err());
    }

    #[test]
    fn test_f64_to_u32_conversions() {
        assert_eq!(f64_to_u32(0.0).unwrap(), 0);
        assert_eq!(f64_to_u32(4294967295.0).unwrap(), u32::MAX);
        assert!(f64_to_u32(4294967296.0).is_err());
        assert!(f64_to_u32(-1.0).is_err());
        assert!(f64_to_u32(1.1).is_err());
    }

    #[test]
    fn test_f64_to_i32_conversions() {
        assert_eq!(f64_to_i32(0.0).unwrap(), 0);
        assert_eq!(f64_to_i32(-2147483648.0).unwrap(), i32::MIN);
        assert_eq!(f64_to_i32(2147483647.0).unwrap(), i32::MAX);
        assert!(f64_to_i32(2147483648.0).is_err());
        assert!(f64_to_i32(-2147483649.0).is_err());
        assert!(f64_to_i32(1.5).is_err());
    }

    #[test]
    fn test_deserialize_value_role_to_string() {
        #[derive(Deserialize, Debug)]
        struct Config {
            access: String,
        }
        let source = "service App:\n    access = @role/admin\n";
        let file = crate::parse(source).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("service", "App").body().unwrap();
        let config: Config = from_block(body).unwrap();
        assert_eq!(config.access, "@role/admin");
    }

    #[test]
    fn test_deserialize_list_item_role_to_vec_string() {
        #[derive(Deserialize, Debug)]
        struct Config {
            members: Vec<String>,
        }
        let source = "role admin:\n    members:\n        - @role/editor\n        - @public\n";
        let file = crate::parse(source).unwrap();
        let doc = crate::query::Document::new(&file);
        let body = doc.block("role", "admin").body().unwrap();
        let config: Config = from_block(body).unwrap();
        assert_eq!(config.members, vec!["@role/editor", "@public"]);
    }
}
