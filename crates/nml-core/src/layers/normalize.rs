//! The one normalizer (RFC 0025 §1): level normalization with a depth policy, vocabulary resolution, spelling canonicalization and the zero-item verdicts.

use std::borrow::Cow;

use crate::ast::{
    Body, BodyEntry, BodyEntryKind, ListItem, ListItemKind, Modifier, ModifierValue, NestedBlock,
    SharedProperty,
};
use crate::diagnostic::{Diagnostic, codes};
use crate::model::{FieldDef, FieldType, ModelDef, OneOfDef};
use crate::schema_index::{NameableVariant, SchemaIndex};
use crate::span::Span;
use crate::types::Value;

use super::decide::*;
use super::entries::*;
use super::instances::*;
use super::policy::*;
use super::seal::*;
use super::*;

/// The shape-only zero-item verdict — [`zero_item_at`]'s untyped arm (an
/// undeclared modifier, a model-less merge): a list-shaped entry that
/// normalized to zero items, which does not supply the list (NML2079's
/// contract) — it neither replaces, empties, nor seals. Every typed path
/// goes through `zero_item_at`; this is its sub-predicate, not a second
/// owner.
fn is_zero_item_entry(kind: &BodyEntryKind) -> bool {
    match kind {
        BodyEntryKind::NestedBlock(nb) => !nb
            .body
            .entries
            .iter()
            .any(|e| matches!(e.kind, BodyEntryKind::ListItem(_))),
        BodyEntryKind::Modifier(m) => match &m.value {
            ModifierValue::Block(items) => items.is_empty(),
            ModifierValue::Inline(sv) => {
                matches!(&sv.value, Value::Array(vs) if vs.is_empty())
            }
            ModifierValue::TypeAnnotation { .. } => false,
        },
        _ => false,
    }
}

/// The ONE normalizer's depth policy (RFC 0025 §1). `Deep` is today's
/// full composition — positional materialization, `.shared`
/// distribution, spelling normalization. `ThisLevel` (a body's own
/// entries only, nested positions passing through raw to their own
/// merge level) arrives with Phase 3, when the merge decides.
pub(in crate::layers) enum Descend {
    /// This body's OWN entries only (RFC 0025 §1): array and modifier
    /// re-spelling, each entry's own zero-item verdict (NML2079,
    /// including the verdict on a nested block's emptiness), the inline
    /// arm headers of this body's `Arms`-typed fields (one hop into the
    /// field's block, header only, no recursion), and this body's
    /// `.shared` lines consumed — distributed one level into its own
    /// direct items, as the deep pass's own-scope slice does. Nested
    /// model, oneof and union bodies, modifier-block items and
    /// `ListItem` bodies pass through RAW — their own merge level
    /// normalizes them.
    ThisLevel,
    /// The full composition: positional materialization (vocab-directed
    /// — a model walk, a list walk, or nothing), `.shared` distribution
    /// and spellings, recursing under each nested position's OWN
    /// vocabulary ([`own_vocab`]).
    Deep,
}

/// The ONE normalizer (RFC 0025 §1): a depth policy instead of a second
/// function, so the merge's per-level pass (`ThisLevel`), winner
/// interiors and the loser subtraction (`Deep`), and the seal scan's
/// probe ([`normalize_for_scan`]) can never drift into two
/// representations of one body. `sink` stamps every finding with the
/// offending layer (`None` for probe-only callers, which discard
/// diagnostics — the real pass emits them once, at the body's own merge
/// level).
pub(in crate::layers) fn normalize_level<'i, 'a, 'b>(
    index: &'i SchemaIndex,
    vocab: Vocab<'i>,
    body: &'b Body,
    descend: Descend,
    sink: Option<(InstanceId<'a>, &mut ComposeSink<'a>)>,
) -> Cow<'b, Body> {
    let mut diags = Vec::new();
    let source_path = sink.as_ref().map_or("", |(id, _)| id.source_path);
    let out = match descend {
        Descend::Deep => {
            let positional = match vocab {
                Vocab::Model(m) => crate::identity::apply_positional_model(index, m, body),
                Vocab::Items(element) => {
                    crate::identity::apply_positional_items(index, element, body)
                }
                Vocab::None => body.clone(),
            };
            let shared = crate::resolve::apply_shared_properties(&positional);
            Cow::Owned(normalize_spellings(
                index,
                vocab,
                &shared,
                source_path,
                &mut diags,
            ))
        }
        Descend::ThisLevel => this_level(index, vocab, body, source_path, &mut diags),
    };
    if let Some((id, sink)) = sink {
        for d in diags {
            sink.emit(id, d);
        }
    }
    out
}

/// One entry's ThisLevel fate — `Keep` lets [`this_level`] return the
/// body BORROWED when no entry changes (a deep stack would otherwise
/// re-clone every nested subtree once per level, an O(depth²) tax the
/// one-walk engine exists to avoid).
enum LevelVerdict {
    Keep,
    /// Boxed: the payload rides the rare path only — `Keep` stays
    /// thin so the common no-change walk carries nothing.
    Replace(Box<BodyEntryKind>),
    Consume,
}

/// [`LevelVerdict::Replace`], boxed at the one door.
fn level_replace(kind: BodyEntryKind) -> LevelVerdict {
    LevelVerdict::Replace(Box::new(kind))
}

/// [`Descend::ThisLevel`] — one body level, own entries only (RFC 0025
/// §1): [`normalize_spellings`]' per-entry re-spelling and zero-item
/// verdicts without its recursion; arm headers injected one hop into
/// `Arms`-typed fields' blocks (header only — where the deep
/// positionalizer materializes them, and before [`Merger::merge_arm_set`]
/// judges a displaced set); this body's `.shared` lines distributed one
/// level into its own direct items and consumed.
fn this_level<'i, 'b>(
    index: &'i SchemaIndex,
    vocab: Vocab<'i>,
    body: &'b Body,
    source_path: &str,
    diags: &mut Vec<Diagnostic>,
) -> Cow<'b, Body> {
    let field_map = first_wins_field_map(vocab.model());
    let shared: Vec<&SharedProperty> = body
        .entries
        .iter()
        .filter_map(|e| match &e.kind {
            BodyEntryKind::SharedProperty(sp) => Some(sp),
            _ => None,
        })
        .collect();
    // Copy-on-write: entries build only from the first change.
    let mut out: Option<Vec<BodyEntry>> = None;
    for (i, entry) in body.entries.iter().enumerate() {
        let verdict = match &entry.kind {
            BodyEntryKind::Property(p) => {
                let field = field_map.get(p.name.name.as_str()).copied();
                match (&p.value.value, field) {
                    // A zero-item array at a UNION position: warned
                    // like every zero-item spelling, but kept in the
                    // array spelling (an empty block reads as an empty
                    // OBJECT of the first model variant downstream).
                    (Value::Array(_), Some(f))
                        if !is_list_like(effective_type(&f.field_type))
                            && zero_item_at(Some(effective_type(&f.field_type)), &entry.kind) =>
                    {
                        diags.push(zero_item_warning(
                            &p.name.name,
                            entry.span,
                            source_path,
                            true,
                        ));
                        LevelVerdict::Keep
                    }
                    (Value::Array(values), Some(f))
                        if is_list_like(effective_type(&f.field_type)) =>
                    {
                        if values.is_empty() {
                            diags.push(zero_item_warning(
                                &p.name.name,
                                entry.span,
                                source_path,
                                union_position(field_map.get(p.name.name.as_str()).copied()),
                            ));
                        }
                        level_replace(BodyEntryKind::NestedBlock(NestedBlock {
                            name: p.name.clone(),
                            body: Body::fresh(
                                items_from_array(values)
                                    .into_iter()
                                    .map(|item| BodyEntry {
                                        span: item.span,
                                        kind: BodyEntryKind::ListItem(item),
                                    })
                                    .collect(),
                            ),
                        }))
                    }
                    _ => LevelVerdict::Keep,
                }
            }
            BodyEntryKind::Modifier(m) => match &m.value {
                ModifierValue::Inline(sv) => match &sv.value {
                    Value::Array(values) => {
                        // NML2079 by the ONE predicate — see
                        // `normalize_spellings`' twin arm.
                        let declared = field_map.get(m.name.name.as_str()).copied();
                        if zero_item_at(
                            declared.map(|f| effective_type(&f.field_type)),
                            &entry.kind,
                        ) {
                            diags.push(zero_item_warning(
                                &m.name.name,
                                entry.span,
                                source_path,
                                union_position(declared),
                            ));
                        }
                        if declared.is_some_and(|f| !is_list_like(effective_type(&f.field_type))) {
                            LevelVerdict::Keep
                        } else {
                            level_replace(BodyEntryKind::Modifier(Modifier {
                                name: m.name.clone(),
                                value: ModifierValue::Block(items_from_array(values)),
                            }))
                        }
                    }
                    _ => LevelVerdict::Keep,
                },
                ModifierValue::Block(items) if items.is_empty() => {
                    let declared = field_map.get(m.name.name.as_str()).copied();
                    if zero_item_at(declared.map(|f| effective_type(&f.field_type)), &entry.kind) {
                        diags.push(zero_item_warning(
                            &m.name.name,
                            entry.span,
                            source_path,
                            union_position(declared),
                        ));
                    }
                    LevelVerdict::Keep
                }
                // A block modifier's ITEMS pass through raw — the
                // list level owns them.
                ModifierValue::Block(_) | ModifierValue::TypeAnnotation { .. } => {
                    LevelVerdict::Keep
                }
            },
            BodyEntryKind::NestedBlock(nb) => {
                let nb_field = field_map.get(nb.name.name.as_str()).copied();
                // This entry's own zero-item verdict; the interior is
                // the nested level's.
                let zero_item_block = nb_field
                    .map(|f| effective_type(&f.field_type))
                    .is_some_and(|ty| zero_item_at(Some(ty), &entry.kind));
                if zero_item_block {
                    diags.push(zero_item_warning(
                        &nb.name.name,
                        entry.span,
                        source_path,
                        union_position(nb_field),
                    ));
                }
                // The inline arm headers of an `Arms`-typed field:
                // one hop, header only (the identity a displaced-set
                // seal judgment must see — fixture AS1).
                match nb_field.map(|f| effective_type(&f.field_type)) {
                    Some(FieldType::Arms { target, .. }) => {
                        level_replace(BodyEntryKind::NestedBlock(NestedBlock {
                            name: nb.name.clone(),
                            body: crate::identity::map_inline_arm_bodies(
                                target,
                                &nb.body,
                                index,
                                0,
                                |_, inline_body, _| inline_body.clone(),
                            ),
                        }))
                    }
                    _ => LevelVerdict::Keep,
                }
            }
            // This body's `.shared` into its own direct items, one
            // level; interior scopes are the deep pass's.
            BodyEntryKind::ListItem(item) if !shared.is_empty() => level_replace(
                BodyEntryKind::ListItem(crate::resolve::merge_shared_into_item(item, &shared)),
            ),
            BodyEntryKind::SharedProperty(_) => LevelVerdict::Consume,
            _ => LevelVerdict::Keep,
        };
        match verdict {
            LevelVerdict::Keep => {
                if let Some(v) = &mut out {
                    v.push(entry.clone());
                }
            }
            LevelVerdict::Replace(kind) => {
                let v = out.get_or_insert_with(|| body.entries[..i].to_vec());
                v.push(BodyEntry {
                    kind: *kind,
                    span: entry.span,
                });
            }
            LevelVerdict::Consume => {
                out.get_or_insert_with(|| body.entries[..i].to_vec());
            }
        }
    }
    match out {
        Some(entries) => Cow::Owned(body.with_entries(entries)),
        None => Cow::Borrowed(body),
    }
}

/// Normalize one authored (inlined) body under a CANDIDATE arm model for
/// seal judgment: positional materialization, `.shared` distribution,
/// spelling normalization — the scan must judge the value the displaced
/// compose WOULD CARRY. Raw-body scanning misses `.shared`-distributed
/// and positionally-materialized sealed writes; scanning bodies
/// normalized under a DIFFERENT arm counts that arm's machinery
/// injections as authored writes. Probe-only: diagnostics are discarded
/// (each level's real normalization emits its own facts exactly once).
pub(in crate::layers) fn normalize_for_scan(
    index: &SchemaIndex,
    arm: &ModelDef,
    body: &Body,
) -> Body {
    normalize_level(index, Vocab::Model(arm), body, Descend::Deep, None).into_owned()
}

pub(in crate::layers) fn union_variant_vocab<'i>(
    index: &'i SchemaIndex,
    name: &str,
    group: &[(InstanceId<'_>, &Body)],
) -> Vec<&'i ModelDef> {
    let mut vocab = Vec::new();
    match index.nameable(name) {
        Some(NameableVariant::Model(m)) => push_model(&mut vocab, m),
        Some(NameableVariant::OneOf(oneof)) => {
            for arm in candidate_arms(oneof, group) {
                if let Some(am) = variant_model_of(index, oneof, &arm) {
                    push_model(&mut vocab, am);
                }
            }
        }
        None => {}
    }
    vocab
}

/// The variant a union position composes BLOCK-shaped items under: the
/// FIRST `List` variant in source order — the resolver's own selection
/// for a list-item body (`resolve_type_in_body`), so the displaced-list
/// judgment, the NML2076 promise and the items' normalization vocabulary
/// all bind to exactly the variant the validator would. A set variant is
/// never selected by block shape (it is reachable only by array literal,
/// which carries scalars) and a second list variant is unreachable — a
/// judgment under either would judge the wrong list (a set variant ahead
/// of the list variant let a switch discard sealed items silently).
pub(in crate::layers) fn union_block_list_variant(ty: &FieldType) -> Option<&FieldType> {
    ty.union_variants()?
        .iter()
        .find(|v| matches!(v, FieldType::List(_)))
}

/// Whether a type admits item-bearing spellings — a list/set field, or
/// a union with ANY list-like variant (a set variant admits `= []`) —
/// the one gate for "is a zero-item entry a no-op here" (NML2079, the
/// union classifier, the seal-write predicate) so a union position is
/// treated like the list it can be.
pub(in crate::layers) fn admits_items(ty: &FieldType) -> bool {
    is_list_like(ty)
        || ty
            .union_variants()
            .is_some_and(|vs| vs.iter().any(is_list_like))
}

/// The zero-item verdict for a BODY at `ty`: at a union position (which
/// admits bodies too) only an entry-less, un-annotated block — `.shared`
/// lines are distributed, not owned, so a `.shared`-only block counts
/// as entry-less raw and normalized alike; a keyed or annotated block is
/// a model body and a WRITE. Never true at a list field (there every
/// item-less block is zero-item, via [`zero_item_at`]).
pub(in crate::layers) fn zero_item_body_at(ty: &FieldType, body: &Body) -> bool {
    !is_list_like(ty)
        && admits_items(ty)
        && body.type_annotation.is_none()
        && !body
            .entries
            .iter()
            .any(|e| !matches!(e.kind, BodyEntryKind::SharedProperty(_)))
}

/// The zero-item verdict for an ENTRY — the ONE predicate behind NML2079,
/// the seal-write exemption, the modifier overlay's no-op skip, and the
/// union classifier's `Empty`. Typed (`Some`): only a type that admits
/// items has zero-item entries at all — for a list field any entry
/// without items; at a union position `= []`, an empty modifier, or an
/// entry-less un-annotated block (a model body is a write). Untyped
/// (`None`, an undeclared modifier or a model-less merge): by shape.
pub(in crate::layers) fn zero_item_at(ty: Option<&FieldType>, kind: &BodyEntryKind) -> bool {
    let Some(ty) = ty else {
        return is_zero_item_entry(kind);
    };
    if !admits_items(ty) {
        return false;
    }
    match kind {
        BodyEntryKind::Property(p) => {
            matches!(&p.value.value, Value::Array(vs) if vs.is_empty())
        }
        BodyEntryKind::NestedBlock(nb) if !is_list_like(ty) => zero_item_body_at(ty, &nb.body),
        _ => is_zero_item_entry(kind),
    }
}

/// The scan vocabulary of one list ELEMENT over an item group — a model
/// element directly, a oneof element under every arm the group could
/// have made effective (fail-closed, the schema default plus each
/// stated discriminator), a union element under every variant the
/// group could have established. One owner for the displaced-list
/// judgment and the general list scan.
pub(in crate::layers) fn list_element_vocab<'i>(
    index: &'i SchemaIndex,
    element: &FieldType,
    group: &[(InstanceId<'_>, &Body)],
) -> Vec<&'i ModelDef> {
    let mut vocab: Vec<&'i ModelDef> = Vec::new();
    match element {
        FieldType::ModelRef(n) => match index.nameable(n) {
            Some(NameableVariant::Model(m)) => push_model(&mut vocab, m),
            Some(NameableVariant::OneOf(oneof)) => {
                for arm in candidate_arms(oneof, group) {
                    if let Some(am) = variant_model_of(index, oneof, &arm) {
                        push_model(&mut vocab, am);
                    }
                }
            }
            None => {}
        },
        ty if ty.union_variants().is_some() => {
            for name in candidate_variants(index, ty, group) {
                for m in union_variant_vocab(index, &name, group) {
                    push_model(&mut vocab, m);
                }
            }
        }
        _ => {}
    }
    vocab
}

pub(in crate::layers) fn arm_body_vocab<'i>(
    index: &'i SchemaIndex,
    target: Option<NameableVariant<'i>>,
    body: &Body,
) -> Vec<&'i ModelDef> {
    match target {
        Some(NameableVariant::Model(m)) => vec![m],
        Some(NameableVariant::OneOf(o)) => stated_discriminator(body, &o.discriminator)
            .or_else(|| o.default_discriminator.clone())
            .and_then(|d| variant_model_of(index, o, &d))
            .into_iter()
            .collect(),
        None => Vec::new(),
    }
}

/// One variant-name → arm-model lookup for every consumer.
pub(in crate::layers) fn variant_model_of<'i>(
    index: &'i SchemaIndex,
    oneof: &OneOfDef,
    disc: &str,
) -> Option<&'i ModelDef> {
    oneof
        .variants
        .iter()
        .find(|(k, _)| k == disc)
        .and_then(|(_, m)| index.model(m))
}

/// The normalization vocabulary of a oneof position: the body's own
/// stated discriminator, else the schema default — its own reading
/// (RFC 0025 §1).
fn oneof_vocab<'i>(index: &'i SchemaIndex, name: &str, body: &Body) -> Option<&'i ModelDef> {
    let oneof = index.oneof(name)?;
    let disc = fold_arm(oneof, &[body])?;
    variant_model_of(index, oneof, &disc)
}

/// The vocabulary a body normalizes under: a model's fields, or — for a
/// LIST body — its element type, resolved per ITEM by [`own_vocab`]. One
/// owner for "which model normalizes this body", so model, oneof and
/// union elements are peers (oneof- and union-element items had NO
/// vocabulary: their zero-item entries went unwarned, their arrays
/// un-re-spelled, unlike the same items under a model element).
#[derive(Clone, Copy)]
pub(in crate::layers) enum Vocab<'i> {
    None,
    Model(&'i ModelDef),
    /// A list body: the element type its items resolve under.
    Items(&'i FieldType),
}

impl<'i> Vocab<'i> {
    pub(in crate::layers) fn of_model(model: Option<&'i ModelDef>) -> Self {
        model.map_or(Vocab::None, Vocab::Model)
    }

    /// The model whose fields this body's entries are read by (`None`
    /// for a list body — its items carry their own).
    pub(in crate::layers) fn model(self) -> Option<&'i ModelDef> {
        match self {
            Vocab::Model(m) => Some(m),
            Vocab::None | Vocab::Items(_) => None,
        }
    }
}

/// The normalization vocabulary of a NAMED type: a model directly; a
/// oneof under the body's own stated discriminator, else the schema
/// default (or its list fields silently keep their Property spelling
/// and every policy misses them). One owner (it was written twice),
/// resolved in the one order (`SchemaIndex::nameable`).
fn named_vocab<'i>(index: &'i SchemaIndex, name: &str, body: &Body) -> Option<&'i ModelDef> {
    match index.nameable(name)? {
        NameableVariant::Model(m) => Some(m),
        NameableVariant::OneOf(_) => oneof_vocab(index, name, body),
    }
}

/// A body's own vocabulary at a position (RFC 0025 §1) — plan-free and
/// context-free, so a discarded body's diagnosis is a pure function of
/// the body: a model directly; a oneof under the arm the body states,
/// else the schema default; a list or set position by its element type;
/// a union under the variant its annotation or shape selects — a
/// list-shaped body under the first `List` variant's element (the
/// resolver's own selection), and NONE when the D2 oracle calls it
/// ambiguous or the stated arm is unknown (never a guess). `target` is
/// the position's declared type (`list_inner(effective_type(..))` for
/// items).
pub(in crate::layers) fn own_vocab<'i>(
    index: &'i SchemaIndex,
    target: &'i FieldType,
    body: &Body,
) -> Vocab<'i> {
    match effective_type(target) {
        FieldType::ModelRef(name) => Vocab::of_model(named_vocab(index, name, body)),
        FieldType::List(inner) | FieldType::Set(inner) => Vocab::Items(inner.as_ref()),
        ty if ty.union_variants().is_some() => {
            match UnionSupply::classify(index, ty, Cow::Borrowed(body)) {
                // Block-shaped items are their own scope: they normalize
                // under the first `List` variant's element type, exactly
                // like a plain list field's items.
                UnionSupply::Items { .. } => union_block_list_variant(ty)
                    .and_then(list_inner)
                    .map_or(Vocab::None, Vocab::Items),
                // A model body: its authored or inferred variant. An
                // oracle-ambiguous body gets NO vocabulary: normalizing
                // under the resolver's first-wins guess would inject
                // that variant's machinery into a body compose refuses
                // to assign a variant.
                supply => Vocab::of_model(
                    supply
                        .nameable_variant()
                        .and_then(|v| named_vocab(index, v, body)),
                ),
            }
        }
        _ => Vocab::None,
    }
}

/// Normalize one list item's body — under its own vocabulary
/// ([`own_vocab`] over the element type: an item scope reads only its
/// own body, never a container's decision). Shared by the item and
/// modifier-block spellings, so both normalize alike.
pub(in crate::layers) fn normalize_item<'i>(
    index: &'i SchemaIndex,
    vocab: Vocab<'i>,
    item: &ListItem,
    source_path: &str,
    diags: &mut Vec<Diagnostic>,
) -> ListItem {
    let body_vocab = |body: &Body| match vocab {
        Vocab::Items(element) => own_vocab(index, element, body),
        other => other,
    };
    let kind = match &item.kind {
        ListItemKind::Named { name, body } => ListItemKind::Named {
            name: name.clone(),
            body: normalize_spellings(index, body_vocab(body), body, source_path, diags),
        },
        ListItemKind::Shorthand {
            value,
            body: Some(b),
        } => ListItemKind::Shorthand {
            value: value.clone(),
            body: Some(normalize_spellings(
                index,
                body_vocab(b),
                b,
                source_path,
                diags,
            )),
        },
        other => other.clone(),
    };
    ListItem {
        kind,
        span: item.span,
    }
}

fn normalize_spellings<'i>(
    index: &'i SchemaIndex,
    vocab: Vocab<'i>,
    body: &Body,
    source_path: &str,
    diags: &mut Vec<Diagnostic>,
) -> Body {
    // One name→field map per body level (wide bodies × wide models made
    // the per-entry linear scan quadratic — a compose-path DoS axis).
    // FIRST-wins on a duplicate field name, exactly like the linear
    // `find` this replaced: a `collect()` would be last-wins, silently
    // swapping which duplicate's policy (e.g. `#sealed`) governs.
    let field_map = first_wins_field_map(vocab.model());
    let entries = body
        .entries
        .iter()
        .map(|entry| {
            let kind = match &entry.kind {
                BodyEntryKind::Property(p) => {
                    let field = field_map.get(p.name.name.as_str()).copied();
                    match (&p.value.value, field) {
                        // A zero-item array at a UNION position: warned
                        // like every zero-item spelling, but kept in the
                        // array spelling (an empty block reads as an empty
                        // OBJECT of the first model variant downstream).
                        (Value::Array(_), Some(f))
                            if !is_list_like(effective_type(&f.field_type))
                                && zero_item_at(
                                    Some(effective_type(&f.field_type)),
                                    &entry.kind,
                                ) =>
                        {
                            diags.push(zero_item_warning(
                                &p.name.name,
                                entry.span,
                                source_path,
                                true,
                            ));
                            entry.kind.clone()
                        }
                        (Value::Array(values), Some(f))
                            if is_list_like(effective_type(&f.field_type)) =>
                        {
                            if values.is_empty() {
                                diags.push(zero_item_warning(
                                    &p.name.name,
                                    entry.span,
                                    source_path,
                                    union_position(field_map.get(p.name.name.as_str()).copied()),
                                ));
                            }
                            BodyEntryKind::NestedBlock(NestedBlock {
                                name: p.name.clone(),
                                body: Body::fresh(
                                    items_from_array(values)
                                        .into_iter()
                                        .map(|item| BodyEntry {
                                            span: item.span,
                                            kind: BodyEntryKind::ListItem(item),
                                        })
                                        .collect(),
                                ),
                            })
                        }
                        _ => entry.kind.clone(),
                    }
                }
                BodyEntryKind::Modifier(m) => match &m.value {
                    ModifierValue::Inline(sv) => match &sv.value {
                        Value::Array(values) => {
                            // NML2079 by the ONE predicate: a declared
                            // NON-list modifier restated as `[]` is a type
                            // error's business, not a zero-item entry; an
                            // undeclared name is judged by shape
                            // (fail-closed: modifiers are list-carriers by
                            // convention).
                            let declared = field_map.get(m.name.name.as_str()).copied();
                            if zero_item_at(
                                declared.map(|f| effective_type(&f.field_type)),
                                &entry.kind,
                            ) {
                                diags.push(zero_item_warning(
                                    &m.name.name,
                                    entry.span,
                                    source_path,
                                    union_position(declared),
                                ));
                            }
                            // Block-form is the canonical spelling of a LIST
                            // modifier; at a union position the inline
                            // array keeps its spelling (a block modifier
                            // under a union type is not a valid instance
                            // spelling downstream — `items_of` reads the
                            // array either way).
                            if declared
                                .is_some_and(|f| !is_list_like(effective_type(&f.field_type)))
                            {
                                entry.kind.clone()
                            } else {
                                BodyEntryKind::Modifier(Modifier {
                                    name: m.name.clone(),
                                    value: ModifierValue::Block(items_from_array(values)),
                                })
                            }
                        }
                        _ => entry.kind.clone(),
                    },
                    // The block-form empty modifier is a zero-item list
                    // entry like every other spelling — the RFC's "always
                    // diagnosed, never silently ignored" admits no
                    // spelling exception.
                    ModifierValue::Block(items) if items.is_empty() => {
                        let declared = field_map.get(m.name.name.as_str()).copied();
                        if zero_item_at(
                            declared.map(|f| effective_type(&f.field_type)),
                            &entry.kind,
                        ) {
                            diags.push(zero_item_warning(
                                &m.name.name,
                                entry.span,
                                source_path,
                                union_position(declared),
                            ));
                        }
                        entry.kind.clone()
                    }
                    // A block modifier's items normalize like a nested
                    // block's (under the declared element type, per
                    // item) — the modifier spelling is a peer, not an
                    // exemption.
                    ModifierValue::Block(items) => {
                        let element = field_map
                            .get(m.name.name.as_str())
                            .and_then(|f| list_inner(effective_type(&f.field_type)));
                        let items_vocab = element.map_or(Vocab::None, Vocab::Items);
                        BodyEntryKind::Modifier(Modifier {
                            name: m.name.clone(),
                            value: ModifierValue::Block(
                                items
                                    .iter()
                                    .map(|item| {
                                        normalize_item(index, items_vocab, item, source_path, diags)
                                    })
                                    .collect(),
                            ),
                        })
                    }
                    ModifierValue::TypeAnnotation { .. } => entry.kind.clone(),
                },
                BodyEntryKind::NestedBlock(nb) => {
                    let nb_field = field_map.get(nb.name.name.as_str()).copied();
                    // The nested body's OWN vocabulary (RFC 0025 §1) —
                    // the one resolver every consumer shares.
                    let inner_vocab =
                        nb_field.map_or(Vocab::None, |f| own_vocab(index, &f.field_type, &nb.body));
                    // Zero-item block form: on a list field, any body
                    // without items; at a UNION position (where a keyed
                    // body is a model body, not an empty list) only an
                    // entry-less, un-annotated block.
                    let zero_item_block = nb_field
                        .map(|f| effective_type(&f.field_type))
                        .is_some_and(|ty| zero_item_at(Some(ty), &entry.kind));
                    if zero_item_block {
                        diags.push(zero_item_warning(
                            &nb.name.name,
                            entry.span,
                            source_path,
                            union_position(nb_field),
                        ));
                    }
                    BodyEntryKind::NestedBlock(NestedBlock {
                        name: nb.name.clone(),
                        body: normalize_spellings(index, inner_vocab, &nb.body, source_path, diags),
                    })
                }
                BodyEntryKind::ListItem(item) => {
                    BodyEntryKind::ListItem(normalize_item(index, vocab, item, source_path, diags))
                }
                _ => entry.kind.clone(),
            };
            BodyEntry {
                kind,
                span: entry.span,
            }
        })
        .collect();
    body.with_entries(entries)
}

/// Whether a field is a union position (for the zero-item warning's
/// wording: nothing there is "the list").
fn union_position(field: Option<&FieldDef>) -> bool {
    field.is_some_and(|f| effective_type(&f.field_type).union_variants().is_some())
}

fn zero_item_warning(field: &str, span: Span, source_path: &str, at_union: bool) -> Diagnostic {
    let msg = if at_union {
        format!(
            "'{field}' normalizes to zero items in a composing layer — it \
             supplies nothing (a zero-item entry never establishes a \
             variant), and \"empty the base value\" has no merge spelling"
        )
    } else {
        format!(
            "'{field}' normalizes to zero items in a composing layer — it does \
             not supply the list, and \"empty the base list\" has no merge \
             spelling"
        )
    };
    Diagnostic::warning(msg)
        .with_code(codes::ZERO_ITEM_LAYER_ENTRY)
        .with_span(span)
        .with_source(source_path.to_string())
}

// ───────────────────────────────────────────────────────────── resolve ──
