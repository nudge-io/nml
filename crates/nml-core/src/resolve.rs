//! Value resolution and shared property inheritance for NML.
//!
//! Provides [`ValueResolver`] for resolving `$ENV.KEY` secrets and fallback chains,
//! and [`apply_shared_properties`] / [`apply_array_shared_properties`] for merging
//! `.key:` (block) and `.key = value` (scalar) shared defaults into list items.

use crate::ast::*;
use crate::types::{SpannedValue, Value};

/// Resolves `Value::Secret` (environment variables) and `Value::Fallback` chains
/// into concrete values.
///
/// The resolver is pluggable: supply any `Fn(&str) -> Option<String>` to look up
/// variable names. The default [`ValueResolver::env()`] constructor reads from
/// `std::env::var`.
///
/// # Example
///
/// ```rust
/// use nml_core::resolve::ValueResolver;
/// use nml_core::types::Value;
///
/// let resolver = ValueResolver::env();
/// let resolved = resolver.resolve(&Value::String("hello".into()));
/// assert!(resolved.is_ok());
/// ```
/// Pluggable variable lookup: maps a `$ENV.KEY` name to its value, or
/// `None` when unset (which triggers the fallback chain, if any).
type VarLookup = Box<dyn Fn(&str) -> Option<String>>;

/// Pluggable `const` lookup: maps a `Value::Reference` name to the value the
/// `const` declares, or `None` when the name is not a known `const` (in which
/// case the reference resolves to its literal name).
type SymbolLookup = Box<dyn Fn(&str) -> Option<Value>>;

/// Bounds reference-chain recursion. Const cycles are normally rejected up front
/// by `SymbolTable::find_const_cycles`; this is defense-in-depth so the resolver
/// is total even if handed a cyclic lookup directly.
const MAX_RESOLVE_DEPTH: u32 = 64;

pub struct ValueResolver {
    /// `None` means the environment is off-limits: any `$ENV.X` is a hard error
    /// ([`ResolveError::EnvDisabled`]) rather than a lookup. See [`Self::without_env`].
    var_resolver: Option<VarLookup>,
    symbol_resolver: Option<SymbolLookup>,
}

impl ValueResolver {
    /// Create a resolver that reads `$ENV.KEY` from `std::env::var`.
    pub fn env() -> Self {
        Self {
            var_resolver: Some(Box::new(|key| std::env::var(key).ok())),
            symbol_resolver: None,
        }
    }

    /// Create a resolver with a custom variable lookup function.
    pub fn new(resolver: impl Fn(&str) -> Option<String> + 'static) -> Self {
        Self {
            var_resolver: Some(Box::new(resolver)),
            symbol_resolver: None,
        }
    }

    /// Create a resolver that **cannot** read the environment: any `$ENV.X` is a
    /// hard error. For contexts that must not touch process env — e.g. potentially
    /// tenant-authored config — combine with [`Self::with_symbols`] for const-only
    /// resolution.
    pub fn without_env() -> Self {
        Self {
            var_resolver: None,
            symbol_resolver: None,
        }
    }

    /// Also resolve `const` references (`Value::Reference`) via `lookup`. A
    /// reference resolves to the named `const`'s value and is then resolved
    /// recursively, so a `const` that holds `$ENV.X` (or another reference)
    /// resolves to fixpoint in this single pass. An unknown reference is left
    /// as-is (it deserializes as its literal name), matching the workflow
    /// parser's `resolve_string` fallback.
    pub fn with_symbols(mut self, lookup: impl Fn(&str) -> Option<Value> + 'static) -> Self {
        self.symbol_resolver = Some(Box::new(lookup));
        self
    }

    /// Resolve a single value through fallback chains and secret lookups.
    ///
    /// Resolution recurses into arrays, so `keys = [$ENV.A, $ENV.B]`
    /// resolves every element. An environment variable that is set but
    /// empty is treated as unset (triggering the fallback chain, if any):
    /// an empty `PORT=""` almost always means "not configured", and
    /// silently resolving to `""` would bypass explicit defaults.
    ///
    /// Resolved secrets become plain [`Value::String`]s; callers must not
    /// log or serialize resolved bodies that may contain secret material.
    pub fn resolve(&self, value: &Value) -> Result<Value, ResolveError> {
        self.resolve_at(value, 0)
    }

    fn resolve_at(&self, value: &Value, depth: u32) -> Result<Value, ResolveError> {
        if depth >= MAX_RESOLVE_DEPTH {
            return Err(ResolveError::ReferenceCycle);
        }
        match value {
            Value::Fallback(primary, fallback) => match self.resolve_at(&primary.value, depth + 1) {
                Ok(val) => Ok(val),
                Err(_) => self.resolve_at(&fallback.value, depth + 1),
            },
            Value::Secret(s) => {
                let Some(key) = s.strip_prefix("$ENV.") else {
                    return Err(ResolveError::UnknownSource(s.clone()));
                };
                // No env lookup configured ⇒ the environment is off-limits here.
                let Some(var_resolver) = &self.var_resolver else {
                    return Err(ResolveError::EnvDisabled(s.clone()));
                };
                match var_resolver(key) {
                    Some(val) if !val.is_empty() => Ok(Value::String(val)),
                    _ => Err(ResolveError::EnvNotSet(key.to_string())),
                }
            }
            // A `const` reference resolves to its value and is then resolved
            // recursively (so a const holding `$ENV.X` or another reference is
            // fully resolved in this one pass). An unknown reference — or no
            // configured symbol lookup — leaves the reference untouched, so it
            // deserializes as its literal name (parser parity).
            Value::Reference(name) => match self.symbol_resolver.as_ref().and_then(|f| f(name)) {
                Some(resolved) => self.resolve_at(&resolved, depth + 1),
                None => Ok(value.clone()),
            },
            Value::Array(items) => {
                let resolved = items
                    .iter()
                    .map(|sv| Ok(SpannedValue::new(self.resolve_at(&sv.value, depth + 1)?, sv.span)))
                    .collect::<Result<Vec<_>, ResolveError>>()?;
                Ok(Value::Array(resolved))
            }
            other => Ok(other.clone()),
        }
    }

    /// Resolve all values in a body, returning a new body with concrete values.
    pub fn resolve_body(&self, body: &Body) -> Result<Body, ResolveError> {
        let entries = body
            .entries
            .iter()
            .map(|entry| self.resolve_body_entry(entry))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Body { entries })
    }

    /// Resolve all values in an `ArrayBody`.
    pub fn resolve_array_body(&self, ab: &ArrayBody) -> Result<ArrayBody, ResolveError> {
        let shared_properties = ab
            .shared_properties
            .iter()
            .map(|sp| self.resolve_shared_property(sp))
            .collect::<Result<Vec<_>, ResolveError>>()?;

        let properties = ab
            .properties
            .iter()
            .map(|p| self.resolve_property(p))
            .collect::<Result<Vec<_>, _>>()?;

        let items = ab
            .items
            .iter()
            .map(|item| self.resolve_list_item(item))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ArrayBody {
            modifiers: ab.modifiers.clone(),
            shared_properties,
            properties,
            items,
        })
    }

    fn resolve_body_entry(&self, entry: &BodyEntry) -> Result<BodyEntry, ResolveError> {
        let kind = match &entry.kind {
            BodyEntryKind::Property(p) => BodyEntryKind::Property(self.resolve_property(p)?),
            BodyEntryKind::NestedBlock(nb) => BodyEntryKind::NestedBlock(NestedBlock {
                name: nb.name.clone(),
                body: self.resolve_body(&nb.body)?,
            }),
            BodyEntryKind::SharedProperty(sp) => {
                BodyEntryKind::SharedProperty(self.resolve_shared_property(sp)?)
            }
            BodyEntryKind::ListItem(item) => BodyEntryKind::ListItem(self.resolve_list_item(item)?),
            BodyEntryKind::Modifier(_) | BodyEntryKind::FieldDefinition(_) => {
                return Ok(entry.clone());
            }
        };
        Ok(BodyEntry {
            kind,
            span: entry.span,
        })
    }

    fn resolve_shared_property(&self, sp: &SharedProperty) -> Result<SharedProperty, ResolveError> {
        Ok(SharedProperty {
            name: sp.name.clone(),
            kind: match &sp.kind {
                SharedPropertyKind::Block(body) => {
                    SharedPropertyKind::Block(self.resolve_body(body)?)
                }
                SharedPropertyKind::Scalar(sv) => {
                    SharedPropertyKind::Scalar(self.resolve_spanned(sv)?)
                }
            },
        })
    }

    fn resolve_property(&self, prop: &Property) -> Result<Property, ResolveError> {
        let resolved = self.resolve_spanned(&prop.value)?;
        Ok(Property {
            name: prop.name.clone(),
            value: resolved,
        })
    }

    fn resolve_spanned(&self, sv: &SpannedValue) -> Result<SpannedValue, ResolveError> {
        Ok(SpannedValue {
            value: self.resolve(&sv.value)?,
            span: sv.span,
        })
    }

    fn resolve_list_item(&self, item: &ListItem) -> Result<ListItem, ResolveError> {
        let kind = match &item.kind {
            ListItemKind::Named { name, body } => ListItemKind::Named {
                name: name.clone(),
                body: self.resolve_body(body)?,
            },
            ListItemKind::Shorthand(sv) => ListItemKind::Shorthand(self.resolve_spanned(sv)?),
            ListItemKind::Reference(_) | ListItemKind::Role(_) => {
                return Ok(item.clone());
            }
        };
        Ok(ListItem {
            kind,
            span: item.span,
        })
    }
}

/// Errors from value resolution.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ResolveError {
    #[error("environment variable '{0}' not set")]
    EnvNotSet(String),
    #[error("unknown variable source in '{0}'")]
    UnknownSource(String),
    #[error("reference resolution exceeded depth limit (cyclic const reference?)")]
    ReferenceCycle,
    #[error("environment variables are not available in this context (got '{0}')")]
    EnvDisabled(String),
}

impl ResolveError {
    /// If this is a denied `$ENV` reference ([`Self::EnvDisabled`] — the environment is
    /// off-limits in the resolving context), the referenced variable text (e.g.
    /// `"$ENV.GROQ_API_KEY"`). Lets a caller replace the generic message with
    /// domain-specific guidance without matching the variant directly.
    pub fn env_disabled_var(&self) -> Option<&str> {
        match self {
            ResolveError::EnvDisabled(var) => Some(var),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// SharedProperty inheritance merging
// ---------------------------------------------------------------------------

/// Merge shared property defaults (`.key:` block or `.key = value`) into `ListItem::Named`
/// entries within a `Body`.
///
/// Block shared properties inject a nested block; scalar ones inject a property. If a list item
/// already has an entry with the same name, the item's entry wins (for blocks, shallow merge
/// into an existing nested block of that name).
/// SharedProperty entries are removed from the output (consumed by the merge).
pub fn apply_shared_properties(body: &Body) -> Body {
    let shared: Vec<&SharedProperty> = body
        .entries
        .iter()
        .filter_map(|e| match &e.kind {
            BodyEntryKind::SharedProperty(sp) => Some(sp),
            _ => None,
        })
        .collect();

    if shared.is_empty() {
        return body.clone();
    }

    let entries = body
        .entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            BodyEntryKind::SharedProperty(_) => None,
            BodyEntryKind::ListItem(item) => {
                let merged = merge_shared_into_item(item, &shared);
                Some(BodyEntry {
                    kind: BodyEntryKind::ListItem(merged),
                    span: entry.span,
                })
            }
            _ => Some(entry.clone()),
        })
        .collect();

    Body { entries }
}

/// Merge shared property defaults from an `ArrayBody` into its list items.
///
/// Same semantics as [`apply_shared_properties`] but operates on `ArrayBody`'s
/// dedicated `shared_properties` field.
pub fn apply_array_shared_properties(array_body: &ArrayBody) -> Vec<ListItem> {
    if array_body.shared_properties.is_empty() {
        return array_body.items.clone();
    }

    let shared_refs: Vec<&SharedProperty> = array_body.shared_properties.iter().collect();
    array_body
        .items
        .iter()
        .map(|item| merge_shared_into_item(item, &shared_refs))
        .collect()
}

fn merge_shared_into_item(item: &ListItem, shared: &[&SharedProperty]) -> ListItem {
    match &item.kind {
        ListItemKind::Named { name, body } => {
            let merged_body = merge_shared_into_body(body, shared);
            ListItem {
                kind: ListItemKind::Named {
                    name: name.clone(),
                    body: merged_body,
                },
                span: item.span,
            }
        }
        _ => item.clone(),
    }
}

fn merge_shared_into_body(body: &Body, shared: &[&SharedProperty]) -> Body {
    let existing_names: Vec<&str> = body
        .entries
        .iter()
        .filter_map(|e| match &e.kind {
            BodyEntryKind::Property(p) => Some(p.name.name.as_str()),
            BodyEntryKind::NestedBlock(nb) => Some(nb.name.name.as_str()),
            BodyEntryKind::SharedProperty(sp) => Some(sp.name.name.as_str()),
            _ => None,
        })
        .collect();

    let mut entries = body.entries.clone();

    for sp in shared {
        let sp_name = sp.name.name.as_str();
        match &sp.kind {
            SharedPropertyKind::Scalar(value) => {
                if !existing_names.contains(&sp_name) {
                    entries.push(BodyEntry {
                        kind: BodyEntryKind::Property(Property {
                            name: sp.name.clone(),
                            value: value.clone(),
                        }),
                        span: sp.name.span,
                    });
                }
            }
            SharedPropertyKind::Block(shared_body) => {
                if existing_names.contains(&sp_name) {
                    merge_shared_block_into_nested(&mut entries, &sp.name, shared_body);
                } else {
                    entries.push(BodyEntry {
                        kind: BodyEntryKind::NestedBlock(NestedBlock {
                            name: sp.name.clone(),
                            body: shared_body.clone(),
                        }),
                        span: sp.name.span,
                    });
                }
            }
        }
    }

    Body { entries }
}

/// When the item already has a nested block with the same name as the shared block, shallow-merge
/// shared body entries into it (item wins on child name collision).
fn merge_shared_block_into_nested(
    entries: &mut [BodyEntry],
    block_name: &Identifier,
    shared_body: &Body,
) {
    for entry in entries.iter_mut() {
        let entry_body = match &mut entry.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == block_name.name => &mut nb.body,
            _ => continue,
        };

        let child_names: Vec<String> = entry_body
            .entries
            .iter()
            .filter_map(|e| match &e.kind {
                BodyEntryKind::Property(p) => Some(p.name.name.clone()),
                BodyEntryKind::NestedBlock(nb) => Some(nb.name.name.clone()),
                _ => None,
            })
            .collect();

        for sp_entry in &shared_body.entries {
            let sp_entry_name = match &sp_entry.kind {
                BodyEntryKind::Property(p) => Some(p.name.name.as_str()),
                BodyEntryKind::NestedBlock(nb) => Some(nb.name.name.as_str()),
                _ => None,
            };
            if let Some(name) = sp_entry_name {
                if !child_names.iter().any(|n| n == name) {
                    entry_body.entries.push(sp_entry.clone());
                }
            }
        }

        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn resolve_array_elements() {
        let r = ValueResolver::new(|key| match key {
            "A" => Some("alpha".into()),
            "B" => Some("beta".into()),
            _ => None,
        });
        let arr = Value::Array(vec![
            SpannedValue::new(Value::Secret("$ENV.A".into()), crate::span::Span::new(0, 6)),
            SpannedValue::new(
                Value::Secret("$ENV.B".into()),
                crate::span::Span::new(8, 14),
            ),
            SpannedValue::new(Value::String("lit".into()), crate::span::Span::new(16, 21)),
        ]);
        let Value::Array(resolved) = r.resolve(&arr).unwrap() else {
            panic!("expected array");
        };
        assert_eq!(resolved[0].value, Value::String("alpha".into()));
        assert_eq!(resolved[1].value, Value::String("beta".into()));
        assert_eq!(resolved[2].value, Value::String("lit".into()));
    }

    #[test]
    fn resolve_array_unset_env_errors() {
        let r = ValueResolver::new(|_| None);
        let arr = Value::Array(vec![SpannedValue::new(
            Value::Secret("$ENV.MISSING".into()),
            crate::span::Span::new(0, 12),
        )]);
        assert!(matches!(
            r.resolve(&arr),
            Err(ResolveError::EnvNotSet(key)) if key == "MISSING"
        ));
    }

    #[test]
    fn empty_env_var_treated_as_unset_triggers_fallback() {
        let r = ValueResolver::new(|key| {
            if key == "PORT" {
                Some(String::new())
            } else {
                None
            }
        });
        let fallback = Value::Fallback(
            Box::new(SpannedValue::new(
                Value::Secret("$ENV.PORT".into()),
                crate::span::Span::new(0, 9),
            )),
            Box::new(SpannedValue::new(
                Value::number(8080.0),
                crate::span::Span::new(12, 16),
            )),
        );
        assert_eq!(r.resolve(&fallback).unwrap(), Value::number(8080.0));
    }

    #[test]
    fn empty_env_var_without_fallback_errors() {
        let r = ValueResolver::new(|_| Some(String::new()));
        assert!(matches!(
            r.resolve(&Value::Secret("$ENV.KEY".into())),
            Err(ResolveError::EnvNotSet(key)) if key == "KEY"
        ));
    }

    #[test]
    fn resolve_literal_passthrough() {
        let r = ValueResolver::env();
        assert_eq!(
            r.resolve(&Value::String("hello".into())).unwrap(),
            Value::String("hello".into())
        );
        assert_eq!(
            r.resolve(&Value::number(42.0)).unwrap(),
            Value::number(42.0)
        );
        assert_eq!(r.resolve(&Value::Bool(true)).unwrap(), Value::Bool(true));
    }

    #[test]
    fn resolve_env_var() {
        let r = ValueResolver::new(|key| {
            if key == "MY_PORT" {
                Some("9090".into())
            } else {
                None
            }
        });
        let result = r.resolve(&Value::Secret("$ENV.MY_PORT".into()));
        assert_eq!(result.unwrap(), Value::String("9090".into()));
    }

    #[test]
    fn resolve_env_var_missing() {
        let r = ValueResolver::new(|_| None);
        let result = r.resolve(&Value::Secret("$ENV.MISSING".into()));
        assert!(result.is_err());
    }

    #[test]
    fn resolve_fallback_primary() {
        let r = ValueResolver::new(|key| {
            if key == "PORT" {
                Some("8080".into())
            } else {
                None
            }
        });
        let val = Value::Fallback(
            Box::new(SpannedValue::new(
                Value::Secret("$ENV.PORT".into()),
                crate::span::Span::empty(0),
            )),
            Box::new(SpannedValue::new(
                Value::number(3000.0),
                crate::span::Span::empty(0),
            )),
        );
        assert_eq!(r.resolve(&val).unwrap(), Value::String("8080".into()));
    }

    #[test]
    fn resolve_fallback_to_default() {
        let r = ValueResolver::new(|_| None);
        let val = Value::Fallback(
            Box::new(SpannedValue::new(
                Value::Secret("$ENV.MISSING".into()),
                crate::span::Span::empty(0),
            )),
            Box::new(SpannedValue::new(
                Value::number(3000.0),
                crate::span::Span::empty(0),
            )),
        );
        assert_eq!(r.resolve(&val).unwrap(), Value::number(3000.0));
    }

    #[test]
    fn resolve_unknown_source() {
        let r = ValueResolver::env();
        let result = r.resolve(&Value::Secret("$FOOBAR.KEY".into()));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unknown variable source"));
    }

    // -------------------------------------------------------------------
    // Const reference resolution (RFC 0002 §9)
    // -------------------------------------------------------------------

    #[test]
    fn reference_passthrough_without_symbols() {
        // Backward-compatible: with no symbol lookup, a reference is untouched.
        let r = ValueResolver::env();
        assert_eq!(
            r.resolve(&Value::Reference("foo".into())).unwrap(),
            Value::Reference("foo".into())
        );
    }

    #[test]
    fn unknown_reference_is_literal() {
        // Parity with the parser's `resolve_string`: an unknown ref stays a
        // reference (which deserializes as its literal name), not an error.
        let r = ValueResolver::new(|_| None).with_symbols(|_| None);
        assert_eq!(
            r.resolve(&Value::Reference("missing".into())).unwrap(),
            Value::Reference("missing".into())
        );
    }

    #[test]
    fn const_reference_resolves_and_chains() {
        let r = ValueResolver::new(|_| None).with_symbols(|name| match name {
            "model" => Some(Value::Reference("defaultModel".into())),
            "defaultModel" => Some(Value::String("llama".into())),
            _ => None,
        });
        assert_eq!(
            r.resolve(&Value::Reference("model".into())).unwrap(),
            Value::String("llama".into())
        );
    }

    #[test]
    fn const_wrapping_env_resolves_in_one_pass() {
        // `const base = $ENV.B`: the reference resolves to the const's `$ENV.B`,
        // which is then env-resolved — all in one pass, order-independent.
        let r = ValueResolver::new(|k| (k == "B").then(|| "envval".to_string()))
            .with_symbols(|name| (name == "base").then(|| Value::Secret("$ENV.B".into())));
        assert_eq!(
            r.resolve(&Value::Reference("base".into())).unwrap(),
            Value::String("envval".into())
        );
    }

    #[test]
    fn without_env_rejects_secrets_but_resolves_consts() {
        // A const-only resolver: consts resolve, `$ENV` is a hard error (never reads
        // the environment), and an unknown reference still passes through as a literal.
        let r = ValueResolver::without_env()
            .with_symbols(|name| (name == "model").then(|| Value::String("llama".into())));

        assert_eq!(
            r.resolve(&Value::Reference("model".into())).unwrap(),
            Value::String("llama".into())
        );
        assert!(matches!(
            r.resolve(&Value::Secret("$ENV.SECRET".into())),
            Err(ResolveError::EnvDisabled(s)) if s == "$ENV.SECRET"
        ));
        assert_eq!(
            r.resolve(&Value::Reference("unknown".into())).unwrap(),
            Value::Reference("unknown".into())
        );
    }

    #[test]
    fn reference_cycle_is_bounded() {
        // Defense-in-depth: a cyclic lookup terminates with an error, not a hang.
        let r = ValueResolver::new(|_| None).with_symbols(|name| match name {
            "a" => Some(Value::Reference("b".into())),
            "b" => Some(Value::Reference("a".into())),
            _ => None,
        });
        assert!(matches!(
            r.resolve(&Value::Reference("a".into())),
            Err(ResolveError::ReferenceCycle)
        ));
    }

    #[test]
    fn resolve_body_deep() {
        let r = ValueResolver::new(|key| {
            if key == "HOST" {
                Some("localhost".into())
            } else {
                None
            }
        });
        let nml = "server App:\n    host = $ENV.HOST\n    port = 8080\n    db:\n        url = $ENV.DB_URL | \"postgres://localhost/dev\"\n";
        let file = parser::parse(nml).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };
        let resolved = r.resolve_body(body).unwrap();

        // Check host resolved
        if let BodyEntryKind::Property(p) = &resolved.entries[0].kind {
            assert_eq!(p.value.value, Value::String("localhost".into()));
        } else {
            panic!("expected property");
        }

        // Check nested db.url fallback resolved
        if let BodyEntryKind::NestedBlock(nb) = &resolved.entries[2].kind {
            if let BodyEntryKind::Property(p) = &nb.body.entries[0].kind {
                assert_eq!(
                    p.value.value,
                    Value::String("postgres://localhost/dev".into())
                );
            }
        }
    }

    #[test]
    fn shared_property_merge_basic() {
        let nml = r#"
workflow W:
    .defaults:
        retries = 3
        timeout = "30s"
    - StepA:
        provider = "fast"
    - StepB:
        provider = "slow"
        defaults:
            retries = 5
"#;
        let file = parser::parse(nml).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };

        let merged = apply_shared_properties(body);

        // SharedProperty should be removed
        assert!(merged
            .entries
            .iter()
            .all(|e| !matches!(&e.kind, BodyEntryKind::SharedProperty(_))));

        // StepA should have defaults injected as a NestedBlock
        if let BodyEntryKind::ListItem(item) = &merged.entries[0].kind {
            if let ListItemKind::Named { body, .. } = &item.kind {
                let has_defaults = body.entries.iter().any(|e| {
                    matches!(
                        &e.kind,
                        BodyEntryKind::NestedBlock(nb) if nb.name.name == "defaults"
                    )
                });
                assert!(has_defaults, "StepA should have inherited .defaults");
            }
        }

        // StepB already has defaults -- should keep its own retries=5
        if let BodyEntryKind::ListItem(item) = &merged.entries[1].kind {
            if let ListItemKind::Named { body, .. } = &item.kind {
                if let Some(entry) = body.entries.iter().find(|e| {
                    matches!(
                        &e.kind,
                        BodyEntryKind::NestedBlock(nb) if nb.name.name == "defaults"
                    )
                }) {
                    if let BodyEntryKind::NestedBlock(nb) = &entry.kind {
                        // Should have retries=5 (from item) and timeout="30s" (from shared)
                        assert_eq!(nb.body.entries.len(), 2);
                    }
                }
            }
        }
    }

    #[test]
    fn shared_property_no_shared_passthrough() {
        let nml = r#"
workflow W:
    - StepA:
        provider = "fast"
"#;
        let file = parser::parse(nml).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };

        let merged = apply_shared_properties(body);
        assert_eq!(merged.entries.len(), body.entries.len());
    }

    // -------------------------------------------------------------------
    // Phase 3: ValueResolver and SharedProperty edge cases
    // -------------------------------------------------------------------

    #[test]
    fn resolve_triple_chained_fallback() {
        let r = ValueResolver::new(|_| None);
        let val = Value::Fallback(
            Box::new(SpannedValue::new(
                Value::Secret("$ENV.A".into()),
                crate::span::Span::empty(0),
            )),
            Box::new(SpannedValue::new(
                Value::Fallback(
                    Box::new(SpannedValue::new(
                        Value::Secret("$ENV.B".into()),
                        crate::span::Span::empty(0),
                    )),
                    Box::new(SpannedValue::new(
                        Value::Fallback(
                            Box::new(SpannedValue::new(
                                Value::Secret("$ENV.C".into()),
                                crate::span::Span::empty(0),
                            )),
                            Box::new(SpannedValue::new(
                                Value::number(42.0),
                                crate::span::Span::empty(0),
                            )),
                        ),
                        crate::span::Span::empty(0),
                    )),
                ),
                crate::span::Span::empty(0),
            )),
        );
        assert_eq!(r.resolve(&val).unwrap(), Value::number(42.0));
    }

    #[test]
    fn resolve_template_string_passthrough() {
        let r = ValueResolver::env();
        let segs = vec![crate::types::TemplateSegment::Literal("hello".into())];
        let val = Value::TemplateString(segs.clone());
        let resolved = r.resolve(&val).unwrap();
        assert_eq!(resolved, Value::TemplateString(segs));
    }

    #[test]
    fn resolve_body_error_propagation() {
        let r = ValueResolver::new(|_| None);
        let nml = "server App:\n    secret = $ENV.REQUIRED\n    port = 8080\n";
        let file = parser::parse(nml).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };
        let result = r.resolve_body(body);
        assert!(
            result.is_err(),
            "unresolvable secret should fail entire body"
        );
    }

    #[test]
    fn shared_property_collision_item_wins() {
        let nml = r#"
workflow W:
    .defaults:
        retries = 3
        timeout = "30s"
    - StepA:
        retries = 10
"#;
        let file = parser::parse(nml).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };

        let merged = apply_shared_properties(body);
        let BodyEntryKind::ListItem(item) = &merged.entries[0].kind else {
            panic!("expected first merged entry to be a list item");
        };
        let ListItemKind::Named { body, .. } = &item.kind else {
            panic!("expected named list item");
        };

        // Item's own retries=10 must win over the shared default.
        let retries = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::Property(p) if p.name.name == "retries" => Some(&p.value.value),
                _ => None,
            })
            .expect("merged item must keep its own retries property");
        assert_eq!(*retries, Value::number(10.0));

        // The `.defaults:` block shared property is injected as a nested
        // block (scalar shared properties inject as top-level properties;
        // block shared properties inject as nested blocks).
        let defaults = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "defaults" => Some(&nb.body),
                _ => None,
            })
            .expect("shared `.defaults:` block must be injected into the item");
        let timeout = defaults
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::Property(p) if p.name.name == "timeout" => Some(&p.value.value),
                _ => None,
            })
            .expect("injected defaults block must carry timeout");
        assert_eq!(*timeout, Value::String("30s".into()));
    }

    #[test]
    fn shared_property_on_shorthand_items_ignored() {
        let nml = r#"
workflow W:
    .defaults:
        retries = 3
    - "plain-item"
"#;
        let file = parser::parse(nml).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };

        let merged = apply_shared_properties(body);
        // Shorthand items should pass through unchanged
        let items: Vec<_> = merged
            .entries
            .iter()
            .filter(|e| matches!(&e.kind, BodyEntryKind::ListItem(_)))
            .collect();
        assert_eq!(items.len(), 1);
        if let BodyEntryKind::ListItem(item) = &items[0].kind {
            assert!(matches!(&item.kind, ListItemKind::Shorthand(_)));
        }
    }

    #[test]
    fn multiple_shared_properties() {
        let nml = r#"
workflow W:
    .config:
        retries = 3
    .limits:
        timeout = "30s"
    - StepA:
        provider = "fast"
"#;
        let file = parser::parse(nml).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };

        let merged = apply_shared_properties(body);
        // Both shared properties should be consumed
        assert!(merged
            .entries
            .iter()
            .all(|e| !matches!(&e.kind, BodyEntryKind::SharedProperty(_))));
        // StepA should have config and limits injected
        if let BodyEntryKind::ListItem(item) = &merged.entries[0].kind {
            if let ListItemKind::Named { body, .. } = &item.kind {
                let has_config = body.entries.iter().any(|e| {
                    matches!(
                        &e.kind,
                        BodyEntryKind::NestedBlock(nb) if nb.name.name == "config"
                    )
                });
                let has_limits = body.entries.iter().any(|e| {
                    matches!(
                        &e.kind,
                        BodyEntryKind::NestedBlock(nb) if nb.name.name == "limits"
                    )
                });
                assert!(has_config, "StepA should inherit .config");
                assert!(has_limits, "StepA should inherit .limits");
            }
        }
    }

    #[test]
    fn shared_property_scalar_merge_in_body() {
        let nml = r#"workflow W:
    .interval = 7200
    - StepA:
        name = "a"
"#;
        let file = parser::parse(nml).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };
        let merged = apply_shared_properties(body);
        if let BodyEntryKind::ListItem(item) = &merged.entries[0].kind {
            if let ListItemKind::Named { body, .. } = &item.kind {
                let interval = body.entries.iter().find_map(|e| match &e.kind {
                    BodyEntryKind::Property(p) if p.name.name == "interval" => Some(p),
                    _ => None,
                });
                let interval = interval.expect("interval property");
                assert_eq!(interval.value.value, Value::number(7200.0));
            }
        } else {
            panic!("expected list item");
        }
    }

    #[test]
    fn shared_property_scalar_item_wins() {
        let nml = r#"workflow W:
    .interval = 7200
    - StepA:
        interval = 100
        name = "a"
"#;
        let file = parser::parse(nml).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };
        let merged = apply_shared_properties(body);
        if let BodyEntryKind::ListItem(item) = &merged.entries[0].kind {
            if let ListItemKind::Named { body, .. } = &item.kind {
                let props: Vec<_> = body
                    .entries
                    .iter()
                    .filter_map(|e| match &e.kind {
                        BodyEntryKind::Property(p) if p.name.name == "interval" => {
                            Some(p.value.value.clone())
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(props.len(), 1);
                assert_eq!(props[0], Value::number(100.0));
            }
        } else {
            panic!("expected list item");
        }
    }

    #[test]
    fn shared_property_scalar_and_block_together() {
        let nml = r#"workflow W:
    .interval = 900
    .defaults:
        retries = 3
    - StepA:
        name = "a"
"#;
        let file = parser::parse(nml).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };
        let merged = apply_shared_properties(body);
        if let BodyEntryKind::ListItem(item) = &merged.entries[0].kind {
            if let ListItemKind::Named { body, .. } = &item.kind {
                let interval = body.entries.iter().find_map(|e| match &e.kind {
                    BodyEntryKind::Property(p) if p.name.name == "interval" => Some(&p.value.value),
                    _ => None,
                });
                assert_eq!(interval, Some(&Value::number(900.0)));
                let has_defaults = body.entries.iter().any(|e| {
                    matches!(
                        &e.kind,
                        BodyEntryKind::NestedBlock(nb) if nb.name.name == "defaults"
                    )
                });
                assert!(has_defaults);
            }
        } else {
            panic!("expected list item");
        }
    }

    #[test]
    fn shared_property_scalar_merge_array_body() {
        let nml = r#"[]item items:
    .interval = 500
    - Row:
        key = "k"
"#;
        let file = parser::parse(nml).unwrap();
        let arr = match &file.declarations[0].kind {
            DeclarationKind::Array(a) => a,
            _ => panic!("expected array"),
        };
        let items = apply_array_shared_properties(&arr.body);
        assert_eq!(items.len(), 1);
        if let ListItemKind::Named { body, .. } = &items[0].kind {
            let interval = body.entries.iter().find_map(|e| match &e.kind {
                BodyEntryKind::Property(p) if p.name.name == "interval" => Some(&p.value.value),
                _ => None,
            });
            assert_eq!(interval, Some(&Value::number(500.0)));
        } else {
            panic!("expected named item");
        }
    }

    #[test]
    fn resolve_shared_scalar_in_shared_property() {
        let r = ValueResolver::new(|k| {
            if k == "PORT" {
                Some("9090".into())
            } else {
                None
            }
        });
        let nml = "workflow W:\n    .port = $ENV.PORT\n    - S:\n        host = \"h\"\n";
        let file = parser::parse(nml).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };
        let resolved = r.resolve_body(body).expect("resolve");
        let merged = apply_shared_properties(&resolved);
        if let BodyEntryKind::ListItem(item) = &merged.entries[0].kind {
            if let ListItemKind::Named { body, .. } = &item.kind {
                let port = body.entries.iter().find_map(|e| match &e.kind {
                    BodyEntryKind::Property(p) if p.name.name == "port" => Some(&p.value.value),
                    _ => None,
                });
                assert_eq!(port, Some(&Value::String("9090".into())));
            }
        } else {
            panic!("expected list item");
        }
    }

    #[test]
    fn resolve_deeply_nested() {
        let r = ValueResolver::new(|key| {
            if key == "HOST" {
                Some("localhost".into())
            } else {
                None
            }
        });
        let nml = "server App:\n    a:\n        b:\n            host = $ENV.HOST\n";
        let file = parser::parse(nml).unwrap();
        let body = match &file.declarations[0].kind {
            DeclarationKind::Block(b) => &b.body,
            _ => panic!("expected block"),
        };
        let resolved = r.resolve_body(body).unwrap();
        // Traverse: a -> b -> host
        if let BodyEntryKind::NestedBlock(a) = &resolved.entries[0].kind {
            if let BodyEntryKind::NestedBlock(b) = &a.body.entries[0].kind {
                if let BodyEntryKind::Property(p) = &b.body.entries[0].kind {
                    assert_eq!(p.value.value, Value::String("localhost".into()));
                }
            }
        }
    }
}
