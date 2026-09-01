//! Body-entry plumbing shared across the engine: field routing, discriminator entries, list-item extraction and spelling adapters.

use std::collections::HashMap;

use crate::ast::{
    Arm, ArmTarget, Body, BodyEntry, BodyEntryKind, Identifier, ListItem, ListItemKind,
    ModifierValue,
};
use crate::model::{FieldDef, FieldType, ModelDef, OneOfDef};
use crate::span::Span;
use crate::types::{SpannedValue, Value};

use super::instances::*;
use super::policy::*;

/// One name→field lookup, FIRST-wins on duplicate names — the module's
/// one convention (SchemaIndex, PolicyCtx, the old linear `find`s), and
/// the fail-closed direction: the first duplicate's policy governs, so a
/// broken schema can never silently swap a `#sealed` for an open field.
pub(in crate::layers) fn first_wins_field_map(
    model: Option<&ModelDef>,
) -> HashMap<&str, &FieldDef> {
    let mut map = HashMap::new();
    if let Some(m) = model {
        for f in &m.fields {
            map.entry(f.name.as_str()).or_insert(f);
        }
    }
    map
}

/// The item sequence an entry carries, across every authoring spelling —
/// block form (`ListItem` entries), array-literal property, and modifier
/// (block or inline-array). `None` for non-item-bearing kinds (scalars,
/// type annotations). One extractor, so no merge path can see one
/// field's spellings as different data.
pub(in crate::layers) fn items_of(kind: &BodyEntryKind) -> Option<Vec<ListItem>> {
    match kind {
        BodyEntryKind::NestedBlock(nb) => Some(
            nb.body
                .entries
                .iter()
                .filter_map(|e| match &e.kind {
                    BodyEntryKind::ListItem(item) => Some(item.clone()),
                    _ => None,
                })
                .collect(),
        ),
        BodyEntryKind::Property(p) => match &p.value.value {
            Value::Array(vs) => Some(items_from_array(vs)),
            _ => None,
        },
        BodyEntryKind::Modifier(m) => match &m.value {
            ModifierValue::Block(items) => Some(items.clone()),
            ModifierValue::Inline(sv) => match &sv.value {
                Value::Array(vs) => Some(items_from_array(vs)),
                _ => None,
            },
            ModifierValue::TypeAnnotation { .. } => None,
        },
        _ => None,
    }
}

/// The nested-block bodies named `name` across a group of sibling bodies,
/// in group order — the sub-bodies a nested field's own compose would run
/// over, used to fold candidate arms at oneof-typed positions.
pub(in crate::layers) fn sub_bodies_at<'b, 'a>(
    siblings: &[(InstanceId<'a>, &'b Body)],
    name: &str,
) -> Vec<(InstanceId<'a>, &'b Body)> {
    let mut out = Vec::new();
    for (id, b) in siblings {
        for e in &b.entries {
            if let BodyEntryKind::NestedBlock(nb) = &e.kind {
                if nb.name.name == name {
                    out.push((*id, &nb.body));
                }
            }
        }
    }
    out
}

/// Dotted field-path join — one spelling owner for provenance keys and
/// diagnostic paths.
pub(crate) fn join_path(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

/// A list element's merge target, resolved once per list position —
/// see [`Merger::item_target`].
pub(in crate::layers) enum ItemTarget {
    Model(ModelDef),
    OneOf(OneOfDef),
    /// A union-typed element (RFC 0015): item groups compose by the
    /// union authority.
    Union(FieldType),
    /// No schema target (structural mode, scalar elements, a dangling
    /// name): model-less deep merge.
    Opaque,
}

/// A VALUE contribution at a field: every spelling except a
/// type-annotation modifier (a declaration, inert in a bound instance —
/// it composes beside the value and never counts as one). The ONE
/// gather predicate, so every fold runs over exactly the supply set the
/// merge composes.
pub(in crate::layers) fn is_value_entry(kind: &BodyEntryKind) -> bool {
    !matches!(kind, BodyEntryKind::Modifier(m)
        if matches!(m.value, ModifierValue::TypeAnnotation { .. }))
}

/// Which merge owns a field group — [`Merger::merge_field`]'s ownership
/// order as data, so the order is a table a test enumerates cell by cell
/// ("sealed beats union beats all-modifier beats policy"), not an
/// arm-order accident.
#[derive(Clone, Copy, Debug)]
pub(in crate::layers) enum FieldRoute<'t> {
    /// `#sealed`, whatever the type or spelling.
    Sealed,
    /// A union-typed position, every spelling.
    Union(&'t FieldType),
    /// An all-modifier group (routes by output shape under its policy).
    Modifier,
    /// A list policy over mixed spellings.
    List,
    /// The default: whole-value overlay.
    Overlay,
}

/// The ownership rule, once: seal, then union, then all-modifier, then
/// policy.
pub(in crate::layers) fn route_of<'t>(
    policy: MergePolicy,
    union_ty: Option<&'t FieldType>,
    all_modifiers: bool,
) -> FieldRoute<'t> {
    match (policy, union_ty) {
        (MergePolicy::Sealed, _) => FieldRoute::Sealed,
        (_, Some(union_ty)) => FieldRoute::Union(union_ty),
        (_, None) if all_modifiers => FieldRoute::Modifier,
        (MergePolicy::Identity | MergePolicy::Append | MergePolicy::IdentityAppend, None) => {
            FieldRoute::List
        }
        (MergePolicy::Overlay, None) => FieldRoute::Overlay,
    }
}

/// A body's diagnostic anchor when no entry span is known: its
/// annotation, else its first entry, else the file start — callers with
/// an entry or item span in hand (every live face) pass that instead.
pub(in crate::layers) fn body_anchor(body: &Body) -> Span {
    body.type_annotation
        .as_ref()
        .map(|i| i.span)
        .or_else(|| body.entries.first().map(|e| e.span))
        .unwrap_or_else(|| Span::empty(0))
}

/// A stated discriminator entry — a string-valued property named like
/// the discriminator — the SELECTION predicate behind
/// [`stated_discriminator`] and [`stated_discriminator_entry`], so the
/// fold, the merge accumulator and the vocab pickers read exactly the
/// same entries. A non-string value is not a discriminator: it is a
/// type error the validator owns (NML2042, on every entry of that
/// name), never an arm selection. Stripping is a different job with a
/// different predicate — [`is_discriminator_named`].
pub(in crate::layers) fn is_discriminator_entry(entry: &BodyEntry, disc: &str) -> bool {
    matches!(&entry.kind, BodyEntryKind::Property(p)
        if p.name.name == disc && matches!(p.value.value, Value::String(_)))
}

/// ANY property named like the discriminator, whatever its value — the
/// STRIP predicate ([`without_discriminator`], the one strip site). The
/// validator's own reading (`variant_body` hides every property of the
/// name), so no discriminator-named group ever forms in the model merge
/// — parity by construction. Non-string entries stripped
/// here pass through the composed view beside the canonical entry
/// ([`Merger::merge_oneof_bodies`]) so the validator reports each at
/// its author's span (RFC 0019 erratum E16).
pub(in crate::layers) fn is_discriminator_named(entry: &BodyEntry, disc: &str) -> bool {
    matches!(&entry.kind, BodyEntryKind::Property(p) if p.name.name == disc)
}

/// The string discriminator a body states, if any — the one reader for
/// "which arm did this layer name?", shared by the fold, the merge
/// accumulator, the vocab pickers, and the arm-set seal scans.
pub(in crate::layers) fn stated_discriminator(body: &Body, disc: &str) -> Option<String> {
    stated_discriminator_entry(body, disc).and_then(|e| match &e.kind {
        BodyEntryKind::Property(p) => p.value.value.as_str().map(str::to_string),
        _ => None,
    })
}

/// The discriminator entry a body states (the [`BodyEntry`] behind
/// [`stated_discriminator`]) — for callers that need its span.
pub(in crate::layers) fn stated_discriminator_entry<'b>(
    body: &'b Body,
    disc: &str,
) -> Option<&'b BodyEntry> {
    body.entries
        .iter()
        .find(|e| is_discriminator_entry(e, disc))
}

/// A oneof body without its discriminator-NAMED properties — the ONE
/// strip the merge applies before merging an arm body (RFC 0023 Part
/// C's strip-by-name, single-sided since RFC 0025: the merge strips,
/// there is no second walk to keep in step). By NAME, not by string-ness
/// ([`is_discriminator_named`]): a non-string entry is a type error,
/// not a field contribution, and letting it into the merge composed it
/// over siblings (`kind = 6` overlaying `kind = 5`) and fed the NML2054
/// union field a supply the validator says can never be set.
pub(in crate::layers) fn without_discriminator(body: &Body, disc: &str) -> Body {
    body.with_entries(
        body.entries
            .iter()
            .filter(|e| !is_discriminator_named(e, disc))
            .cloned()
            .collect(),
    )
}

/// `Value::Role`/`Value::Reference` map to their matching `ListItemKind`;
/// anything else is a bodiless scalar-keyed `Shorthand`. Mapping roles or
/// references to `Shorthand` would make `|deny = [@ops]` and a block-form
/// `- @ops` a cross-kind pair at an equal token — the exact inverse of the
/// spelling invariance normalization exists to provide.
pub(in crate::layers) fn items_from_array(values: &[SpannedValue]) -> Vec<ListItem> {
    values
        .iter()
        .map(|v| {
            let kind = match &v.value {
                Value::Role(r) => ListItemKind::Role(r.clone()),
                Value::Reference(name) => ListItemKind::Reference(Identifier {
                    name: name.clone(),
                    span: v.span,
                }),
                _ => ListItemKind::Shorthand {
                    value: v.clone(),
                    body: None,
                },
            };
            ListItem { kind, span: v.span }
        })
        .collect()
}

/// A synthesized LIST body over `items` — the item-bearing spellings'
/// common shape for a scan or a Deep diagnosis.
pub(in crate::layers) fn list_body_of(items: Vec<ListItem>) -> Body {
    Body::fresh(
        items
            .into_iter()
            .map(|item| BodyEntry {
                span: item.span,
                kind: BodyEntryKind::ListItem(item),
            })
            .collect(),
    )
}

/// A model-level `Arm` entry's deep pass (RFC 0025 §2): its inline body
/// consumes its own `.shared` scopes — the set is untyped at a model
/// level, so no positional, no headers and no spellings, exactly as the
/// whole-layer pass treated a direct arm entry.
pub(in crate::layers) fn deep_arm_entry(entry: BodyEntry) -> BodyEntry {
    let BodyEntryKind::Arm(arm) = &entry.kind else {
        return entry;
    };
    let ArmTarget::Inline { name, body } = &arm.target else {
        return entry;
    };
    BodyEntry {
        span: entry.span,
        kind: BodyEntryKind::Arm(Arm {
            selector: arm.selector.clone(),
            selector_span: arm.selector_span,
            target: ArmTarget::Inline {
                name: name.clone(),
                body: crate::resolve::apply_shared_properties(body),
            },
        }),
    }
}

/// One layer's contribution to one field: the entry plus where it came from.
#[derive(Clone)]
pub(in crate::layers) struct Contribution<'a> {
    pub(in crate::layers) layer: InstanceId<'a>,
    pub(in crate::layers) entry: BodyEntry,
}

/// The item references an entry carries, across the two body-bearing list
/// spellings (block form and block-form modifiers). Inline-array items
/// are bodiless — nothing inside them to scan.
pub(in crate::layers) fn item_refs(kind: &BodyEntryKind) -> Vec<&ListItem> {
    match kind {
        BodyEntryKind::NestedBlock(nb) => nb
            .body
            .entries
            .iter()
            .filter_map(|e| match &e.kind {
                BodyEntryKind::ListItem(item) => Some(item),
                _ => None,
            })
            .collect(),
        BodyEntryKind::Modifier(m) => match &m.value {
            ModifierValue::Block(items) => items.iter().collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}
