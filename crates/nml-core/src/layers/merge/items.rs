//! Items: gather, then compose (RFC 0025 §3) — one fold per group, item keys, `.shared` distribution and token materialization.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::ast::{Body, BodyEntryKind, ListItem, ListItemKind, SharedProperty};
use crate::diagnostic::{Diagnostic, codes};
use crate::model::{FieldDef, FieldType, ModelDef};
use crate::schema_index::SchemaIndex;
use crate::span::Span;
use crate::types::Value;

use super::*;
use crate::layers::entries::*;
use crate::layers::instances::*;
use crate::layers::normalize::*;
use crate::layers::policy::*;
use crate::layers::seal::*;

/// All sibling items at field `name`, across BOTH list spellings, in
/// layer-then-document order — the identity-group pool for item scans.
pub(in crate::layers) fn sibling_items_at<'a, 'b>(
    siblings: &[(InstanceId<'a>, &'b Body)],
    name: &str,
) -> Vec<(InstanceId<'a>, &'b ListItem)> {
    let mut out = Vec::new();
    for (id, b) in siblings {
        for e in &b.entries {
            let named = match &e.kind {
                BodyEntryKind::NestedBlock(nb) => nb.name.name == name,
                BodyEntryKind::Modifier(m) => m.name.name == name,
                _ => false,
            };
            if named {
                for item in item_refs(&e.kind) {
                    out.push((*id, item));
                }
            }
        }
    }
    out
}

/// One identity group of a list-policy merge (RFC 0025 §3): the key and
/// its members as (layer index, item), in stack order — the first
/// member anchors the group (its key and its list slot).
struct ItemGroup {
    key: ItemKey,
    members: Vec<(usize, ListItem)>,
}

/// The list-level `.shared` lines an entry carries beside its items —
/// the block spelling only: an array literal has none, and a
/// `ModifierValue::Block` carries none (RFC 0025 §3). The companion of
/// [`items_of`], which drops them.
pub(in crate::layers) fn shared_of(kind: &BodyEntryKind) -> Vec<SharedProperty> {
    match kind {
        BodyEntryKind::NestedBlock(nb) => nb
            .body
            .entries
            .iter()
            .filter_map(|e| match &e.kind {
                BodyEntryKind::SharedProperty(sp) => Some(sp.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A group member's own vocabulary at the element (RFC 0025 §§3-4): a
/// model element directly; a oneof element under the arm the member
/// states, else the schema default; a union element under its
/// annotation or shape ([`own_vocab`]); nothing when opaque or
/// ambiguous.
fn member_vocab<'i>(index: &'i SchemaIndex, target: &'i ItemTarget, body: &Body) -> Vocab<'i> {
    match target {
        ItemTarget::Model(m) => Vocab::Model(m),
        ItemTarget::OneOf(o) => Vocab::of_model(
            stated_discriminator(body, &o.discriminator)
                .as_deref()
                .or(o.default_discriminator.as_deref())
                .and_then(|d| variant_model_of(index, o, d)),
        ),
        ItemTarget::Union(ty) => own_vocab(index, ty, body),
        ItemTarget::Opaque => Vocab::None,
    }
}

/// The model a group member's own reading selects — the home of the
/// `+` token field ([`member_vocab`]'s model face).
fn member_model<'i>(
    index: &'i SchemaIndex,
    target: &'i ItemTarget,
    body: &Body,
) -> Option<&'i ModelDef> {
    member_vocab(index, target, body).model()
}

/// A group member's fold-input body (RFC 0025 §3): the raw body with
/// the layer's list-level `.shared` distributed one level, yielding to
/// the member's identity-token field (RFC 0005 §10 — the token is
/// materialized after the fold, into the lowest surviving body, so a
/// `.shared` naming the `+` field must not claim it first). A bodiless
/// scalar takes a fresh body exactly when its own reading materializes
/// it (a `+` field, not ambiguous) — or in a multi-member group, where
/// the body merge always gave it one; Reference/Role, dropped-key and
/// ambiguous bodiless members pass through bodiless, exactly as the
/// shared merge passes them today.
fn prepared_member_body(
    index: &SchemaIndex,
    target: &ItemTarget,
    shared: &[SharedProperty],
    item: &ListItem,
    multi: bool,
) -> Option<Body> {
    let raw: Option<Body> = match &item.kind {
        ListItemKind::Named { body, .. } => Some(body.clone()),
        ListItemKind::Shorthand { body: Some(b), .. } => Some(b.clone()),
        ListItemKind::Shorthand { body: None, .. } => {
            let fresh = Body::fresh(Vec::new());
            let materializes = member_model(index, target, &fresh)
                .is_some_and(|m| m.fields.iter().any(|f| f.shorthand));
            if multi || materializes {
                Some(fresh)
            } else {
                None
            }
        }
        ListItemKind::Reference(_) | ListItemKind::Role(_) => None,
    };
    raw.map(|b| {
        if shared.is_empty() || !multi {
            // A singleton distributes AFTER its token materializes
            // (order does the yielding there — no fold to precede);
            // multi-member groups distribute here, before the fold,
            // masked by the token field the post-fold injection will
            // claim.
            return b;
        }
        let mask = token_mask(index, target, item, &b);
        distribute_shared_level(&b, shared, mask.as_deref())
    })
}

/// RFC 0025 §3 — one layer's list-level `.shared` into one member's
/// body, ONE level (deeper scopes distribute when the body next
/// normalizes), yielding to the member's identity-token field: RFC 0005
/// §10 gives an item's own token the win, and the token materializes
/// only after the fold — masking here is what keeps the shared write
/// from claiming the field first. A Named key's `name` is NOT a token
/// (composition never materializes it), so a list-wide `.name` keeps
/// reaching a Named item.
/// The member's token-field mask for `.shared` distribution (RFC 0005
/// §10): a Shorthand member's `+` field, read from its own model —
/// the MULTI-member group path's stand-in for the token the post-fold
/// injection will claim (a lone member needs no mask: its token is
/// already in the body when `.shared` distributes —
/// [`prepare_lone_member`]).
fn token_mask(
    index: &SchemaIndex,
    target: &ItemTarget,
    item: &ListItem,
    b: &Body,
) -> Option<String> {
    match &item.kind {
        ListItemKind::Shorthand { .. } => member_model(index, target, b)
            .and_then(|m| m.fields.iter().find(|f| f.shorthand))
            .map(|f| f.name.clone()),
        _ => None,
    }
}

fn distribute_shared_level(body: &Body, shared: &[SharedProperty], mask: Option<&str>) -> Body {
    let selected: Vec<&SharedProperty> = shared
        .iter()
        .filter(|sp| mask != Some(sp.name.name.as_str()))
        .collect();
    crate::resolve::merge_shared_into_body(body, &selected)
}

/// The LONE-member pipeline (RFC 0025 §§3-4), shared by the surviving
/// singleton's compose and the gather drop's diagnosis: no fold to
/// precede, so the member's identity token materializes into the body
/// FIRST, under its own pre-token reading (lenient — an explicit value
/// wins), then the layer's list-level `.shared` distributes and yields
/// to the now-present token by order (RFC 0005 §10), then the reading
/// derives from the result. One pipeline, two consumers: a drop is
/// diagnosed exactly as it would have composed alone, its token
/// carrying the author's arm ascription (§4 — its OWN reading; a token
/// naming no arm reads as stated-unknown, never a guess).
fn prepare_lone_member<'i>(
    index: &'i SchemaIndex,
    target: &'i ItemTarget,
    shared: &[SharedProperty],
    item: &ListItem,
    body: Body,
) -> (Body, Vocab<'i>) {
    let body = match (&item.kind, member_model(index, target, &body)) {
        (ListItemKind::Shorthand { value, .. }, Some(m)) => {
            let token = crate::identity::ItemToken {
                value: value.clone(),
            };
            let materialized = crate::identity::materialize_token(&token, &body, m);
            if materialized.validatable {
                materialized.body
            } else {
                body
            }
        }
        _ => body,
    };
    let body = distribute_shared_level(&body, shared, None);
    let vocab = member_vocab(index, target, &body);
    (body, vocab)
}

/// Token-only prehash (kind-blind): buckets candidates for identity
/// lookups; exactness is always re-verified inside the bucket via
/// [`ItemKey::same`]/[`ItemKey::token_eq`], so a collision only costs
/// speed, never correctness. The one hard requirement is the INVERSE:
/// two tokens that compare equal MUST hash equal, or an identity would
/// be missed. So each numeric kind hashes its own value through its
/// (numeric) `Hash` — `1` and `1.0`, `90m` and `1h30m`, `1.50 USD` and
/// `1.5 USD` collide as they must, while DISTINCT numeric values scatter
/// across buckets. Hashing the type name instead (the earlier spelling)
/// collapsed every numeric-keyed item into one bucket, resurrecting the
/// O(n²) scan the bucketing exists to kill — a measured DoS.
pub(in crate::layers) fn token_prehash(key: &ItemKey) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match key {
        ItemKey::Named(n) | ItemKey::Reference(n) | ItemKey::Role(n) => n.hash(&mut h),
        ItemKey::Scalar(v) => match v {
            Value::String(s) => s.hash(&mut h),
            Value::Number(n) => n.hash(&mut h),
            Value::Duration(d) => d.hash(&mut h),
            Value::Bool(b) => b.hash(&mut h),
            // Money is not `Hash`; its equality is (amount, currency)
            // (decimal places are currency-derived).
            Value::Money(m) => {
                m.amount.hash(&mut h);
                m.currency.hash(&mut h);
            }
            // Non-scalar tokens can't key a list item; the type name is a
            // safe (degenerate) fallback for anything exotic.
            other => other.type_name().hash(&mut h),
        },
    }
    h.finish()
}

/// The list-target leg of the seal scan, shared by the block and modifier
/// spellings. Each item's oneof scope is its IDENTITY GROUP — same kind
/// AND token: the merge refuses cross-kind pairs (NML2063) and never
/// composes them, so pairing them here fabricated seal refusals of legal
/// switches. The sibling pool is bucketed ONCE by token prehash —
/// re-scanning every sibling item per item was O(items²), a measured DoS
/// from sub-megabyte input. Named path segments echo the identity;
/// scalar keys echo only the value's TYPE (scalar tokens are values and
/// are never disclosed).
pub(in crate::layers) fn scan_list_items<'a, 'b>(
    index: &SchemaIndex,
    at: &FieldIdentity,
    fields: &[&FieldDef],
    own: &[&'b ListItem],
    pool: &[(InstanceId<'a>, &'b ListItem)],
    layer: InstanceId<'a>,
    out: &mut SealSink<'a>,
) {
    // Every candidate field's element TYPE — named refs and unions
    // alike ("at any depth" binds union-typed elements too; a
    // ModelRef-only read here was a laundering hole through `[](a|b)`
    // items of a displaced variant).
    let element_types: Vec<&FieldType> = fields
        .iter()
        .filter_map(|f| match list_inner(effective_type(&f.field_type)) {
            Some(inner @ FieldType::ModelRef(_)) => Some(inner),
            Some(inner) if inner.union_variants().is_some() => Some(inner),
            _ => None,
        })
        .collect();
    if element_types.is_empty() || own.is_empty() {
        return;
    }
    let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, (_, item)) in pool.iter().enumerate() {
        buckets
            .entry(token_prehash(&ItemKey::of(&item.kind)))
            .or_default()
            .push(i);
    }
    for item in own {
        let item_body = match &item.kind {
            ListItemKind::Named { body, .. } => Some(body),
            ListItemKind::Shorthand { body, .. } => body.as_ref(),
            _ => continue,
        };
        let Some(b) = item_body else { continue };
        let key = ItemKey::of(&item.kind);
        let mut group: Vec<(InstanceId<'a>, &Body)> = Vec::new();
        if let Some(bucket) = buckets.get(&token_prehash(&key)) {
            for &i in bucket {
                let (sid, sitem) = pool[i];
                if !ItemKey::of(&sitem.kind).same(&key) {
                    continue;
                }
                let sbody = match &sitem.kind {
                    ListItemKind::Named { body, .. } => Some(body),
                    ListItemKind::Shorthand { body: Some(bb), .. } => Some(bb),
                    _ => None,
                };
                if let Some(bb) = sbody {
                    group.push((sid, bb));
                }
            }
        }
        if group.is_empty() {
            group.push((layer, b));
        }
        for ety in &element_types {
            let item_vocab = list_element_vocab(index, ety, &group);
            if !item_vocab.is_empty() {
                let iat = at.child(Seg::Item(key.clone()));
                seal_scan_body(index, &iat, &item_vocab, b, &group, layer, out);
            }
        }
    }
}

/// List-item identity: the pair (item kind, token). Kinds are part of the
/// key — a cross-kind match at an equal token is NML2063, never a merge.
/// Equality is [`Self::same`]; `Hash` is coherent with it by the same
/// rule [`token_prehash`] pins (numeric kinds hash their numeric values,
/// so semantic-equals hash equal), tagged by kind.
#[derive(Clone)]
pub(in crate::layers) enum ItemKey {
    Named(String),
    Scalar(Value),
    Reference(String),
    Role(String),
}

/// The ONE redaction point (RFC 0019 requirement 4: scalar-keyed
/// identities are not exempt — their tokens are values): a scalar key
/// prints its TYPE NAME, never the token, so every derived `Debug`
/// above this type (`Seg`, `FieldIdentity`, a sink dump in a panic
/// message) is non-disclosing by construction.
impl std::fmt::Debug for ItemKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ItemKey::Named(n) => write!(f, "Named({n})"),
            ItemKey::Reference(n) => write!(f, "Reference({n})"),
            ItemKey::Role(n) => write!(f, "Role({n})"),
            ItemKey::Scalar(v) => write!(f, "Scalar({})", v.type_name()),
        }
    }
}

impl PartialEq for ItemKey {
    fn eq(&self, other: &Self) -> bool {
        self.same(other)
    }
}

impl Eq for ItemKey {}

impl std::hash::Hash for ItemKey {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        std::mem::discriminant(self).hash(h);
        match self {
            ItemKey::Named(n) | ItemKey::Reference(n) | ItemKey::Role(n) => n.hash(h),
            ItemKey::Scalar(v) => match v {
                Value::String(s) => s.hash(h),
                Value::Number(n) => n.hash(h),
                Value::Duration(d) => d.hash(h),
                Value::Bool(b) => b.hash(h),
                Value::Money(m) => {
                    m.amount.hash(h);
                    m.currency.hash(h);
                }
                other => other.type_name().hash(h),
            },
        }
    }
}

impl ItemKey {
    /// The non-disclosing path segment of an item (`xs[w]`): a named
    /// item's name; a scalar-keyed item's TYPE (tokens are data, never
    /// echoed); a reference's or role's name.
    pub(in crate::layers) fn segment(&self) -> String {
        match self {
            ItemKey::Named(n) | ItemKey::Reference(n) | ItemKey::Role(n) => n.clone(),
            ItemKey::Scalar(v) => v.type_name().to_string(),
        }
    }

    pub(in crate::layers) fn of(kind: &ListItemKind) -> Self {
        match kind {
            ListItemKind::Named { name, .. } => ItemKey::Named(name.name.clone()),
            ListItemKind::Shorthand { value, .. } => ItemKey::Scalar(value.value.clone()),
            ListItemKind::Reference(id) => ItemKey::Reference(id.name.clone()),
            ListItemKind::Role(r) => ItemKey::Role(r.clone()),
        }
    }

    /// Same (kind, token) — the full identity.
    pub(in crate::layers) fn same(&self, other: &Self) -> bool {
        self.same_kind(other) && self.token_eq(other)
    }

    fn same_kind(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (ItemKey::Named(_), ItemKey::Named(_))
                | (ItemKey::Scalar(_), ItemKey::Scalar(_))
                | (ItemKey::Reference(_), ItemKey::Reference(_))
                | (ItemKey::Role(_), ItemKey::Role(_))
        )
    }

    /// Token equality across kinds (`semantic_eq` for scalar tokens) — used
    /// to detect cross-kind collisions.
    fn token_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ItemKey::Scalar(a), ItemKey::Scalar(b)) => a.semantic_eq(b),
            (ItemKey::Named(a), ItemKey::Named(b))
            | (ItemKey::Reference(a), ItemKey::Reference(b))
            | (ItemKey::Role(a), ItemKey::Role(b)) => a == b,
            (ItemKey::Named(a), ItemKey::Reference(b))
            | (ItemKey::Reference(a), ItemKey::Named(b)) => a == b,
            (ItemKey::Named(a), ItemKey::Scalar(Value::String(b)))
            | (ItemKey::Scalar(Value::String(a)), ItemKey::Named(b)) => a == b,
            _ => false,
        }
    }

    fn is_scalar(&self) -> bool {
        matches!(self, ItemKey::Scalar(_))
    }
}

// ─────────────────────────────────────────────────────────────── tests ──

impl<'a, 'd> Merger<'a, 'd> {
    /// GATHER-THEN-COMPOSE (RFC 0025 §3): one pass groups items by
    /// identity — layer order, first-seen group order, the token-prehash
    /// buckets kept — then each group composes ONCE
    /// ([`Self::compose_group`]): each layer's list-level `.shared`
    /// distributed into that layer's members (yielding to a scalar key's
    /// token field, RFC 0005 §10), one fold per group through the
    /// element's authority, the group's token materialized into the
    /// lowest surviving body before the body merge, survivors normalized
    /// under the decided variant, and the fold's losers and the gather's
    /// drops diagnosed under their own readings (§4 — the item home of
    /// the subtraction; a dropped item lives inside a surviving list
    /// entry, so no per-(layer, entry) subtraction reaches it). Set
    /// dedupe rides resolved-instance validation (NML2030); the merge
    /// itself is shape-agnostic.
    pub(in crate::layers) fn merge_items(
        &mut self,
        path: &str,
        policy: MergePolicy,
        target: &ItemTarget,
        per_layer: &[(InstanceId<'a>, Span, Vec<SharedProperty>, Vec<ListItem>)],
    ) -> Vec<ListItem> {
        let (groups, drops) = self.gather_items(path, policy, per_layer);
        let mut out: Vec<ListItem> = Vec::with_capacity(groups.len());
        let mut named_rows: Vec<(InstanceId<'a>, Span)> = Vec::new();
        for group in &groups {
            let (item, owner) = self.compose_group(path, target, per_layer, group);
            named_rows.push((owner, item.span));
            out.push(item);
        }
        // Named identity items' provenance rows, after every group's
        // interior — scalar keys are values and never key a row.
        for (group, (owner, span)) in groups.iter().zip(named_rows) {
            if let ItemKey::Named(n) = &group.key {
                self.record(&format!("{path}[{n}]"), owner, span);
            }
        }
        // §4 — a gather drop is a loser with no group (a dropped member
        // never anchors one): its interior diagnoses under its OWN
        // reading, through the very pipeline a surviving singleton
        // composes by ([`prepare_lone_member`]) — token materialized
        // first, then `.shared` distributed (order does the yielding,
        // RFC 0005 §10), then the reading derived — so a drop and its
        // surviving twin read alike, the token carrying the author's
        // arm ascription into the diagnosis.
        let index = self.index;
        for (li, item) in &drops {
            let drop_path = format!("{path}[{}]", ItemKey::of(&item.kind).segment());
            let (layer, _, shared, _) = &per_layer[*li];
            self.diagnose_discards(&drop_path, std::slice::from_ref(item), &Vec::new(), |it| {
                prepared_member_body(index, target, shared, it, false).map(|b| {
                    let (b, vocab) = prepare_lone_member(index, target, shared, it, b);
                    (*layer, Cow::Owned(b), vocab)
                })
            });
        }
        out
    }

    /// Compose ONE identity group (RFC 0025 §3), returning the composed
    /// item and its owning layer. A Reference/Role group and a bodiless
    /// singleton have nothing to fold — the anchor passes through (an
    /// identical restatement is a no-op). A SINGLETON with a body
    /// deep-normalizes under its own reading — no merge level runs, so
    /// no merge-level records and no annotation synthesis, exactly the
    /// treatment the whole-layer pass gave an unpaired item (its own
    /// fold would decide nothing a single body does not already state).
    /// A multi-member group folds ONCE over the members'
    /// shared-distributed bodies; the composed item keeps the anchor's
    /// list slot, and its span, identity token and owner follow the
    /// HEAD of the surviving group (RFC 0019 E15).
    fn compose_group(
        &mut self,
        path: &str,
        target: &ItemTarget,
        per_layer: &[(InstanceId<'a>, Span, Vec<SharedProperty>, Vec<ListItem>)],
        group: &ItemGroup,
    ) -> (ListItem, InstanceId<'a>) {
        let item_path = format!("{path}[{}]", group.key.segment());
        let multi = group.members.len() > 1;
        let index = self.index;
        // §3: members' — each member's fold-input body.
        let prepared: Vec<(InstanceId<'a>, &ListItem, Option<Body>)> = group
            .members
            .iter()
            .map(|(li, item)| {
                let (layer, _, shared, _) = &per_layer[*li];
                (
                    *layer,
                    item,
                    prepared_member_body(index, target, shared, item, multi),
                )
            })
            .collect();
        let (anchor_layer, anchor_item) = (prepared[0].0, prepared[0].1);
        if !multi {
            let (_, _, shared, _) = &per_layer[group.members[0].0];
            let composed = match prepared.into_iter().next() {
                Some((layer, item, Some(body))) => {
                    // The lone-member pipeline ([`prepare_lone_member`]:
                    // token first, then `.shared`, then the reading),
                    // then the body deep-normalizes under that reading —
                    // the whole-layer pass's exact treatment of an
                    // unpaired item.
                    let (body, vocab) = prepare_lone_member(index, target, shared, item, body);
                    let deep = normalize_level(
                        index,
                        vocab,
                        &body,
                        Descend::Deep,
                        Some((layer, &mut *self.sink)),
                    )
                    .into_owned();
                    let kind = match &item.kind {
                        ListItemKind::Named { name, .. } => ListItemKind::Named {
                            name: name.clone(),
                            body: deep,
                        },
                        _ => ListItemKind::Shorthand {
                            value: match &item.kind {
                                ListItemKind::Shorthand { value, .. } => value.clone(),
                                _ => unreachable!("a body-bearing item is Named or Shorthand"),
                            },
                            body: Some(deep),
                        },
                    };
                    ListItem {
                        kind,
                        span: item.span,
                    }
                }
                // Bodiless singleton (a Reference/Role, a dropped-key or
                // ambiguous scalar): the anchor passes through.
                _ => anchor_item.clone(),
            };
            return (composed, anchor_layer);
        }
        let bodies: Vec<(InstanceId<'a>, Body)> = prepared
            .iter()
            .filter_map(|(l, _, b)| b.as_ref().map(|b| (*l, b.clone())))
            .collect();
        if bodies.is_empty() {
            // A Reference/Role group: an identical restatement is a
            // no-op; the anchor passes through.
            return (anchor_item.clone(), anchor_layer);
        }
        debug_assert_eq!(
            bodies.len(),
            prepared.len(),
            "a group folds all its members' bodies or none ({item_path})"
        );
        let token = match &anchor_item.kind {
            ListItemKind::Shorthand { value, .. } => Some(crate::identity::ItemToken {
                value: value.clone(),
            }),
            _ => None,
        };
        let anchors: Vec<Span> = group.members.iter().map(|(_, it)| it.span).collect();
        let composed =
            self.merge_item_bodies(&item_path, token.as_ref(), target, &bodies, &anchors);
        // §4 — the fold's losers, at the item home, under their own
        // readings over the body the fold actually read.
        self.diagnose_discards(&item_path, &prepared, &composed.survivors, |(l, _, b)| {
            b.as_ref()
                .map(|b| (*l, Cow::Borrowed(b), member_vocab(index, target, b)))
        });
        let (head_layer, head_item, _) = &prepared[composed.head];
        let kind = match &head_item.kind {
            ListItemKind::Named { name, .. } => ListItemKind::Named {
                name: name.clone(),
                body: composed.body,
            },
            ListItemKind::Shorthand { value, .. } => ListItemKind::Shorthand {
                value: value.clone(),
                body: Some(composed.body),
            },
            // Unreachable (a body-bearing group is Named or Shorthand by
            // construction) — fail safe, keep the head as authored.
            other => other.clone(),
        };
        let composed_item = ListItem {
            kind,
            span: head_item.span,
        };
        (composed_item, *head_layer)
    }

    /// The GATHER (RFC 0025 §3), one pass in layer order: a second
    /// same-key item in one layer under an identity policy is an NML2063
    /// duplicate; a same-kind group joins, else a token-equal cross-kind
    /// match is NML2063 — both only once a lower layer has supplied at
    /// least one item, while within the supplying base itself a
    /// token-equal pair of different kinds is legal and both are kept;
    /// no group under an established `#identity` base is NML2067;
    /// `#append` concatenates a same-key scalar as a new singleton group
    /// and rejects a non-scalar redefinition; a Reference/Role
    /// restatement joins its group. Every drop is diagnosed here and
    /// returned for the item home's subtraction.
    fn gather_items(
        &mut self,
        path: &str,
        policy: MergePolicy,
        per_layer: &[(InstanceId<'a>, Span, Vec<SharedProperty>, Vec<ListItem>)],
    ) -> (Vec<ItemGroup>, Vec<(usize, ListItem)>) {
        let mut groups: Vec<ItemGroup> = Vec::new();
        // Token-prehash buckets over the groups (kind-blind, so same-kind
        // and cross-kind candidates share a bucket; exactness re-verified
        // inside) — the per-item linear scans were O(items²), an editor
        // and CLI DoS axis on large lists.
        let mut group_index: HashMap<u64, Vec<usize>> = HashMap::new();
        let mut drops: Vec<(usize, ListItem)> = Vec::new();
        // NML2067 fires only once some lower layer actually SUPPLIED items
        // (≥1) — a zero-item base entry neither supplies nor establishes
        // the list (NML2079's contract), so it must not turn every higher
        // tier's first real items into "unmatched overlays".
        let mut base_established = false;
        for (li, (layer, _span, _shared, items)) in per_layer.iter().enumerate() {
            let mut seen_this_layer: HashMap<u64, Vec<ItemKey>> = HashMap::new();
            let supplied = !items.is_empty();
            for item in items {
                let key = ItemKey::of(&item.kind);
                let prehash = token_prehash(&key);
                // Duplicate identity WITHIN one layer's list (identity-keyed
                // policies only).
                if matches!(policy, MergePolicy::Identity | MergePolicy::IdentityAppend)
                    && seen_this_layer
                        .get(&prehash)
                        .is_some_and(|ks| ks.iter().any(|k| k.same(&key)))
                {
                    self.emit(
                        *layer,
                        Diagnostic::error(format!(
                            "duplicate identity in one layer's '{path}' list — \
                             the merge key must be unique before it can be \
                             merged on; delete the duplicate"
                        ))
                        .with_code(codes::IDENTITY_REDEFINITION)
                        .with_span(item.span)
                        .with_source(layer.source_path.to_string()),
                    );
                    drops.push((li, item.clone()));
                    continue;
                }
                seen_this_layer
                    .entry(prehash)
                    .or_default()
                    .push(key.clone());
                // Same-kind identity first: a base may legally hold a
                // shorthand "a" AND a named a: (the within-layer duplicate
                // check keys on kind+token), so a first-token-equal lookup
                // would bind an overlay's named a to the SCALAR group and
                // misfire NML2063 while its true merge partner sits right
                // there. Cross-kind token collision is only the verdict
                // when no same-kind partner exists.
                let existing = {
                    let bucket: &[usize] = group_index
                        .get(&prehash)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    bucket
                        .iter()
                        .copied()
                        .find(|&g| groups[g].key.same(&key))
                        .or_else(|| {
                            bucket
                                .iter()
                                .copied()
                                .find(|&g| groups[g].key.token_eq(&key))
                        })
                };
                let new_group =
                    |groups: &mut Vec<ItemGroup>, group_index: &mut HashMap<u64, Vec<usize>>| {
                        group_index.entry(prehash).or_default().push(groups.len());
                        groups.push(ItemGroup {
                            key: key.clone(),
                            members: vec![(li, item.clone())],
                        });
                    };
                match existing {
                    None => {
                        if base_established && policy == MergePolicy::Identity {
                            // Unmatched overlay item without #append.
                            let mut msg = format!(
                                "item matches no base identity in '{path}' \
                                 (the schema grants overriding, not adding)"
                            );
                            if let Some(hint) = self.named_hint(&key, &groups) {
                                msg.push_str(&format!(" — did you mean '{hint}'?"));
                            }
                            self.emit(
                                *layer,
                                Diagnostic::error(msg)
                                    .with_code(codes::UNMATCHED_OVERLAY_ITEM)
                                    .with_span(item.span)
                                    .with_source(layer.source_path.to_string()),
                            );
                            drops.push((li, item.clone()));
                            continue;
                        }
                        new_group(&mut groups, &mut group_index);
                    }
                    Some(g) => {
                        if !base_established {
                            // Base-internal duplicates are the base's own
                            // business at merge time (identity-keyed dupes
                            // were already diagnosed above; scalar dupes are
                            // ordinary list semantics): a separate group.
                            new_group(&mut groups, &mut group_index);
                            continue;
                        }
                        // Cross-kind match at an equal token: NML2063 under
                        // every identity-keyed policy.
                        if !groups[g].key.same_kind(&key) {
                            self.emit(
                                *layer,
                                Diagnostic::error(format!(
                                    "item matches an existing identity in \
                                     '{path}' across item kinds — match the \
                                     base's spelling"
                                ))
                                .with_code(codes::IDENTITY_REDEFINITION)
                                .with_span(item.span)
                                .with_source(layer.source_path.to_string()),
                            );
                            drops.push((li, item.clone()));
                            continue;
                        }
                        match policy {
                            MergePolicy::Append => {
                                if groups[g].key.is_scalar() {
                                    // Scalar concatenation: duplicates
                                    // legal, each its own singleton group
                                    // (never paired).
                                    new_group(&mut groups, &mut group_index);
                                } else {
                                    self.emit(
                                        *layer,
                                        Diagnostic::error(format!(
                                            "item redefines an existing \
                                             identity in '{path}' — the \
                                             schema grants adding, not \
                                             overriding; ask for `#identity`"
                                        ))
                                        .with_code(codes::IDENTITY_REDEFINITION)
                                        .with_span(item.span)
                                        .with_source(layer.source_path.to_string()),
                                    );
                                    drops.push((li, item.clone()));
                                }
                            }
                            MergePolicy::Identity | MergePolicy::IdentityAppend => {
                                groups[g].members.push((li, item.clone()));
                            }
                            MergePolicy::Overlay | MergePolicy::Sealed => {
                                unreachable!("list policies only")
                            }
                        }
                    }
                }
            }
            if supplied {
                base_established = true;
            }
        }
        (groups, drops)
    }

    /// Identity-matched item bodies merge ONCE per group (RFC 0025 §3),
    /// by the element's kind: a oneof element routes through the arm
    /// accumulator (seal enforcement, backstop), a union element through
    /// the union authority (establishment, backstop, annotation
    /// synthesis), a model element deep-merges. The group's `token`
    /// reaches the decided level through the signature and materializes
    /// into the lowest surviving body before the body merge. One anchor
    /// per body (an entry-less item body has no span of its own).
    fn merge_item_bodies(
        &mut self,
        item_path: &str,
        token: Option<&crate::identity::ItemToken>,
        target: &ItemTarget,
        sub: &[(InstanceId<'a>, Body)],
        anchors: &[Span],
    ) -> Composed {
        match target {
            ItemTarget::OneOf(oneof) => self.merge_oneof_bodies(item_path, oneof, sub, token),
            ItemTarget::Union(ty) => self.merge_union_bodies(item_path, ty, sub, anchors, token),
            ItemTarget::Model(m) => Composed::base(
                self.merge_model_with_token(item_path, Some(m), sub, token),
                sub.len(),
            ),
            ItemTarget::Opaque => {
                Composed::base(self.merge_model_bodies(item_path, None, sub), sub.len())
            }
        }
    }

    fn named_hint(&self, key: &ItemKey, groups: &[ItemGroup]) -> Option<String> {
        // NML2067's did-you-mean discloses NAMED identities only —
        // scalar-keyed tokens are values and are never echoed.
        let ItemKey::Named(input) = key else {
            return None;
        };
        let names: Vec<&str> = groups
            .iter()
            .filter_map(|g| match &g.key {
                ItemKey::Named(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        crate::suggest::suggest(input, names.iter().copied()).map(|s| s.to_string())
    }
}
