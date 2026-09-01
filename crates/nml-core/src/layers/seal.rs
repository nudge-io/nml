//! Seal scans and judgments: displaced-seal collection, the arm/variant switch backstop (NML2071/NML2085), and the seal sink.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::ast::{ArmSelector, Body, BodyEntryKind, ListItemKind, ModifierValue};
use crate::diagnostic::{Diagnostic, codes};
use crate::model::{FieldDef, FieldType, ModelDef};
use crate::schema_index::{NameableVariant, SchemaIndex};
use crate::span::Span;

use super::decide::*;
use super::entries::*;
use super::instances::*;
use super::merge::*;
use super::normalize::*;
use super::policy::*;

/// Whether an entry on a `#sealed` field counts as a WRITE under the
/// engine's own semantics: for a list-shaped field a zero-item entry
/// (empty array spelling included) neither supplies nor seals (NML2079's
/// contract); for every other shape — scalars and object-typed fields —
/// every entry is a write. One predicate shared by `merge_sealed` and the
/// backstop scan so "assigned" can never mean two different things.
pub(in crate::layers) fn seal_write(f: &FieldDef, kind: &BodyEntryKind) -> bool {
    // A type-annotation modifier is a declaration, never a value: it can
    // neither seal a field nor violate a seal.
    if matches!(kind, BodyEntryKind::Modifier(m)
        if matches!(m.value, ModifierValue::TypeAnnotation { .. }))
    {
        return false;
    }
    // A zero-item entry is the ONE exemption (NML2079's contract); at a
    // union position a keyed or annotated block is a model body — a
    // write — not a zero-item list (reading it as one let an upper layer
    // replace a sealed body silently, and hid it from every backstop).
    !zero_item_at(Some(effective_type(&f.field_type)), kind)
}

/// One step of a seal's path relative to the judged position.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::layers) enum Seg {
    Field(String),
    Item(ItemKey),
    Arm(ArmSelector),
}

/// A seal's identity — hashed and compared structurally, rendered
/// non-disclosingly by the ONE rule ([`Self::at`]); the path was built
/// in three places with two join rules before, and the identity now
/// renders itself. Non-disclosure is enforced at the token holder:
/// [`ItemKey`]'s redacting `Debug` and `segment()` keep every derived
/// `Debug` above this type safe, and no textual serialization of a
/// value is ever created (RFC 0019 requirement 4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub(in crate::layers) struct FieldIdentity(Vec<Seg>);

impl FieldIdentity {
    pub(in crate::layers) fn child(&self, seg: Seg) -> Self {
        let mut segs = self.0.clone();
        segs.push(seg);
        FieldIdentity(segs)
    }

    /// The ONE join — `secret`, `[w].secret`, `nest.secret` joined at
    /// `position`: a dot for fields, brackets for items (the
    /// non-disclosing [`ItemKey::segment`]), nothing for arms (two
    /// arms' seals of one field are two FIELDS that render alike).
    pub(in crate::layers) fn at(&self, position: &str) -> String {
        let mut out = String::from(position);
        for seg in &self.0 {
            match seg {
                Seg::Field(name) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(name);
                }
                Seg::Item(key) => {
                    out.push('[');
                    out.push_str(&key.segment());
                    out.push(']');
                }
                Seg::Arm(_) => {}
            }
        }
        out
    }
}

/// One seal hit a displaced-group judgment records.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::layers) struct SealHit<'a> {
    pub(in crate::layers) id: FieldIdentity,
    pub(in crate::layers) span: Span,
    pub(in crate::layers) layer: InstanceId<'a>,
}

/// The hits of one judgment — lowest-then-document order, so `.first()`
/// is the related span; fields are the distinct identities among them,
/// assignments the distinct `(file, span)` sites.
pub(in crate::layers) type SealHits<'a> = Vec<SealHit<'a>>;

/// The seal scan's sink: hits plus the (identity, file, span) set that
/// dedups them — the same assignment of the same field re-encountered
/// is one hit, but the identity stays in the key because one span IS
/// two fields when a list-level `.shared` line distributes into several
/// items, and two files can carry byte-identical spans at one identity
/// (a linear scan of the hits per hit was the O(hits²) term of a wide
/// judgment).
pub(in crate::layers) struct SealSink<'a> {
    pub(in crate::layers) hits: SealHits<'a>,
    seen: HashSet<(FieldIdentity, &'a str, usize, usize)>,
}

impl<'a> SealSink<'a> {
    pub(in crate::layers) fn new() -> Self {
        Self {
            hits: Vec::new(),
            seen: HashSet::new(),
        }
    }

    fn hit(&mut self, id: FieldIdentity, span: Span, layer: InstanceId<'a>) {
        if self
            .seen
            .insert((id.clone(), layer.source_path, span.start, span.end))
        {
            self.hits.push(SealHit { id, span, layer });
        }
    }
}

/// Seal judgment over a displaced LIST group (an [`Establishment::Items`]
/// switch): the displaced list body is judged AS A LIST under the list
/// variant the resolver selects for a list shape (the FIRST `List`
/// variant — [`union_block_list_variant`], the one the displaced compose
/// would carry; a set variant and later list variants are unreachable
/// by block shape) — list-level `.shared` writes
/// distributed, each item's identity token materialized into its body
/// (a positional `+` field is a write) under the arm its own body
/// selects, the body then normalized and scanned under the element
/// vocabulary, seal paths prefixed with the item's non-disclosing
/// segment. Scanning item bodies in isolation missed positional tokens,
/// `.shared` writes, and bodiless items alike — "a structural group has
/// no seals" is true only for scalars.
pub(in crate::layers) fn displaced_list_seals<'a>(
    index: &SchemaIndex,
    union_ty: &FieldType,
    group: &[(InstanceId<'a>, &Body)],
) -> SealHits<'a> {
    let mut sink = SealSink::new();
    let Some(element) = union_block_list_variant(union_ty).and_then(list_inner) else {
        return sink.hits;
    };
    for (gid, gb) in group {
        let shared = crate::resolve::apply_shared_properties(gb);
        for entry in &shared.entries {
            let BodyEntryKind::ListItem(item) = &entry.kind else {
                continue;
            };
            let probe: Cow<'_, Body> = match &item.kind {
                ListItemKind::Named { body, .. } => Cow::Borrowed(body),
                ListItemKind::Shorthand { body: Some(b), .. } => Cow::Borrowed(b),
                ListItemKind::Shorthand { body: None, .. } => Cow::Owned(Body::fresh(Vec::new())),
                _ => continue,
            };
            let own = [(*gid, probe.as_ref())];
            let vocab = list_element_vocab(index, element, &own);
            // Materialize and normalize under the arm the item's OWN
            // body selects (stated-else-default); scan under the
            // fail-closed vocabulary.
            let norm_model = match element {
                FieldType::ModelRef(n) => arm_body_vocab(index, index.nameable(n), &probe)
                    .first()
                    .copied(),
                _ => vocab.first().copied(),
            };
            let Some(norm_model) = norm_model else {
                continue;
            };
            let materialized = crate::identity::materialize_item(item, norm_model).body;
            let normd = normalize_for_scan(index, norm_model, &materialized);
            let refs: Vec<(InstanceId<'a>, &Body)> = vec![(*gid, &normd)];
            // Hits join under the item's identity (`[w].secret`); the
            // emitter joins the position (`slot[w].secret`) through the
            // ONE rule (`FieldIdentity::at`).
            let iat = FieldIdentity::default().child(Seg::Item(ItemKey::of(&item.kind)));
            assigned_seals_into(index, &iat, &vocab, &refs, &mut sink);
        }
    }
    sink.hits
}

/// Seal judgment over a displaced group, normalized under the DISPLACED
/// vocabulary — the one "what would the displaced compose carry?" scan
/// every backstop face asks ([`fold_arm_checked`],
/// [`fold_variant_checked`], [`Merger::merge_arm_set`]). Normalizing
/// under the surviving vocabulary instead counts that vocabulary's
/// machinery injections as authored writes; scanning raw bodies misses
/// `.shared`-distributed and positionally-materialized sealed writes.
/// Empty vocab ⇒ no seals (nothing nameable was displaced).
pub(in crate::layers) fn displaced_group_seals<'a>(
    index: &SchemaIndex,
    vocab: &[&ModelDef],
    group: &[(InstanceId<'a>, &Body)],
) -> SealHits<'a> {
    let mut sink = SealSink::new();
    displaced_group_seals_into(index, &FieldIdentity::default(), vocab, group, &mut sink);
    sink.hits
}

/// [`displaced_group_seals`] into a caller-owned sink, under an identity
/// prefix (an item's `Seg::Item`, an arm's `Seg::Arm`; the emitter joins
/// the position).
pub(in crate::layers) fn displaced_group_seals_into<'a>(
    index: &SchemaIndex,
    at: &FieldIdentity,
    vocab: &[&ModelDef],
    group: &[(InstanceId<'a>, &Body)],
    sink: &mut SealSink<'a>,
) {
    if vocab.is_empty() {
        return;
    }
    let normd: Vec<(InstanceId<'a>, Body)> = group
        .iter()
        .map(|(gid, gb)| (*gid, normalize_for_scan(index, vocab[0], gb)))
        .collect();
    let refs: Vec<(InstanceId<'a>, &Body)> = normd.iter().map(|(gid, gb)| (*gid, gb)).collect();
    assigned_seals_into(index, at, vocab, &refs, sink);
}

/// The three faces of the seal backstop's NML2060. RFC 0019 binds "the
/// oneof arm switch, the union `as` switch, and the arm-set wholesale
/// replacement equally" — so one wording owner covers all three (the
/// same discipline [`layer_bound_exceeded`] applies to NML2066): the
/// count-when-multiple suffix, the teaching tail, and the cross-file
/// "sealed here" note can never drift between faces.
pub(in crate::layers) enum BackstopFace<'n> {
    /// A oneof discriminator restated at a different value.
    ArmSwitch {
        discriminator: &'n str,
        stated: &'n str,
    },
    /// A union body authored `as` a different variant.
    VariantSwitch { stated: &'n str },
    /// An arm-set field restated (v1 wholesale replacement).
    ArmSetReplacement,
}

/// How many discarded assignments a backstop rejection points at with a
/// `sealed here` note; the message carries the full counts.
pub(in crate::layers) const RELATED_SEALS: usize = 4;

/// The one NML2060 backstop rejection — position named uniformly (elided
/// at an instance root, where there is no path to name), the seal
/// identity joined here through the ONE rule ([`FieldIdentity::at`]),
/// counted by FIELD (RFC 0019's promise: distinct identities) with the
/// assignment count when it EXCEEDS the fields (a distributed `.shared`
/// line has more fields than assignments — no suffix), and an
/// action-bearing tail. One
/// `sealed here` note per distinct assignment — the first
/// `RELATED_SEALS` — each carrying its own file (`Related.source`).
pub(in crate::layers) fn seal_backstop_rejection(
    face: BackstopFace<'_>,
    path: &str,
    seals: &[SealHit<'_>],
    at: Span,
    layer: InstanceId<'_>,
) -> Diagnostic {
    let first = seals
        .first()
        .expect("a rejection records at least one seal");
    let position = if path.is_empty() {
        String::new()
    } else {
        format!(" on '{path}'")
    };
    let lead = match face {
        BackstopFace::ArmSwitch {
            discriminator,
            stated,
        } => format!("arm switch to `{discriminator} = \"{stated}\"`{position}"),
        BackstopFace::VariantSwitch { stated } => {
            format!("variant switch to `as {stated}`{position}")
        }
        BackstopFace::ArmSetReplacement => format!("arm-set replacement{position}"),
    };
    // Fields = distinct identities; assignments = distinct (file, span)
    // sites — one span IS two fields under a distributed `.shared` line,
    // and two files can assign one field at byte-identical spans.
    // Set-backed like the sink's own dedup: a linear `contains` per hit
    // was the O(hits²) term of a wide judgment again (measured: 60s
    // from a 1.1 MB hostile input at 64k items).
    let mut fields: HashSet<&FieldIdentity> = HashSet::new();
    let mut sites: HashSet<(&str, usize, usize)> = HashSet::new();
    for h in seals {
        fields.insert(&h.id);
        sites.insert((h.layer.source_path, h.span.start, h.span.end));
    }
    let (field_count, assignments) = (fields.len(), sites.len());
    let seal_field = first.id.at(path);
    let extra = field_count - 1;
    let noun = if extra == 1 { "field" } else { "fields" };
    let more = match (extra, assignments > field_count) {
        (0, false) => String::new(),
        (0, true) => format!(" ({assignments} assignments)"),
        (_, false) => format!(" (and {extra} more {noun})"),
        (_, true) => format!(" (and {extra} more {noun}; {assignments} assignments)"),
    };
    let msg = format!(
        "{lead} would discard the assigned `#sealed` field '{seal_field}'{more} — \
         replacement cannot launder a seal; compose into the lower value, or \
         unseal the field in the schema"
    );
    let mut d = Diagnostic::error(msg)
        .with_code(codes::SEALED_FIELD_VIOLATION)
        .with_span(at)
        .with_source(layer.source_path.to_string());
    let mut noted: Vec<(&str, usize, usize)> = Vec::new();
    for h in seals {
        if noted.len() == RELATED_SEALS {
            break;
        }
        let site = (h.layer.source_path, h.span.start, h.span.end);
        if noted.contains(&site) {
            continue;
        }
        noted.push(site);
        d = d.with_related_in(h.span, "sealed here", Some(h.layer.source_path.to_string()));
    }
    d
}

/// Every assigned `#sealed` field a displaced group of bodies carries,
/// validated against the `vocab` candidate models, at any depth, into a
/// caller-owned sink — the ONE dedup across every body of a judgment
/// (the displaced-list and arm-set faces each scan several bodies into
/// one sink). See [`seal_scan_body`] for the scan's contract.
fn assigned_seals_into<'a>(
    index: &SchemaIndex,
    at: &FieldIdentity,
    vocab: &[&ModelDef],
    group: &[(InstanceId<'a>, &Body)],
    sink: &mut SealSink<'a>,
) {
    for (layer, body) in group {
        seal_scan_body(index, at, vocab, body, group, *layer, sink);
    }
}

/// Add `m` to a candidate vocabulary unless a same-named model is present.
pub(in crate::layers) fn push_model<'i>(vocab: &mut Vec<&'i ModelDef>, m: &'i ModelDef) {
    if !vocab.iter().any(|x| x.name == m.name) {
        vocab.push(m);
    }
}

pub(in crate::layers) fn seal_scan_body<'a>(
    index: &SchemaIndex,
    at: &FieldIdentity,
    vocab: &[&ModelDef],
    body: &Body,
    siblings: &[(InstanceId<'a>, &Body)],
    layer: InstanceId<'a>,
    out: &mut SealSink<'a>,
) {
    // One name→fields map per scan level (the per-entry vocab scan was
    // the same quadratic width axis as the merge's field lookup). Two
    // multiplicities, deliberately different: ACROSS vocab models
    // (candidate arms) every model contributes — the fail-closed union;
    // WITHIN one model a duplicate field name is FIRST-wins, exactly the
    // policy the merge resolves — an any-sealed read here made the
    // backstop refuse switches over a field the merge itself treats as
    // open, the engine disagreeing with itself about one declaration.
    let mut field_map: HashMap<&str, Vec<&FieldDef>> = HashMap::new();
    for m in vocab {
        let mut in_model: HashSet<&str> = HashSet::new();
        for f in &m.fields {
            if in_model.insert(f.name.as_str()) {
                field_map.entry(f.name.as_str()).or_default().push(f);
            }
        }
    }
    let lookup =
        |name: &str| -> &[&FieldDef] { field_map.get(name).map(|v| v.as_slice()).unwrap_or(&[]) };
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::Property(p) => {
                for f in lookup(&p.name.name) {
                    if policy_of(f) == MergePolicy::Sealed && seal_write(f, &entry.kind) {
                        out.hit(at.child(Seg::Field(f.name.clone())), entry.span, layer);
                        break;
                    }
                }
            }
            BodyEntryKind::Modifier(m) => {
                // A modifier spelling assigns its field like any other
                // entry — skipping it would launder a sealed modifier
                // field through an arm switch.
                let fields = lookup(&m.name.name);
                // `seal_write`, not a bare zero-item check — the SAME
                // write predicate the Property/NestedBlock arms and
                // `merge_sealed` use, so the backstop and the merge can
                // never disagree about what "assigned" means (a non-list
                // sealed field writes with every entry; only a zero-item
                // LIST entry is the exemption).
                if fields
                    .iter()
                    .any(|f| policy_of(f) == MergePolicy::Sealed && seal_write(f, &entry.kind))
                {
                    out.hit(at.child(Seg::Field(m.name.name.clone())), entry.span, layer);
                    continue;
                }
                // A modifier's ITEMS are list items like any other
                // spelling's — their sealed fields must reach the scan, or
                // `|steps:` launders what `steps:` cannot.
                scan_list_items(
                    index,
                    &at.child(Seg::Field(m.name.name.clone())),
                    fields,
                    &item_refs(&entry.kind),
                    &sibling_items_at(siblings, &m.name.name),
                    layer,
                    out,
                );
            }
            BodyEntryKind::NestedBlock(nb) => {
                let fields = lookup(&nb.name.name);
                if fields.is_empty() {
                    continue;
                }
                if fields
                    .iter()
                    .any(|f| policy_of(f) == MergePolicy::Sealed && seal_write(f, &entry.kind))
                {
                    // The whole field is the write; its interior needs no
                    // separate scan.
                    out.hit(
                        at.child(Seg::Field(nb.name.name.clone())),
                        entry.span,
                        layer,
                    );
                    continue;
                }
                let fat = at.child(Seg::Field(nb.name.name.clone()));
                let sibs = sub_bodies_at(siblings, &nb.name.name);
                // Non-list targets: ONE recursion over the union of every
                // candidate field's target vocabulary. The RFC's "at any
                // depth, recursively" binds every variant-typed interior:
                // model refs, oneofs (every arm the group could have made
                // effective), UNION-typed fields (every variant the group
                // could have established — a `ModelRef`-only match here
                // was a laundering hole one union deep), and arm-set
                // interiors (scanned per inline arm body below).
                let mut child: Vec<&ModelDef> = Vec::new();
                for f in fields {
                    let ety = effective_type(&f.field_type);
                    if let FieldType::ModelRef(n) = ety {
                        match index.nameable(n) {
                            Some(NameableVariant::Model(m)) => push_model(&mut child, m),
                            Some(NameableVariant::OneOf(oneof)) => {
                                for arm in candidate_arms(oneof, &sibs) {
                                    if let Some(am) = variant_model_of(index, oneof, &arm) {
                                        push_model(&mut child, am);
                                    }
                                }
                            }
                            None => {}
                        }
                    } else if ety.union_variants().is_some() {
                        // Fail-closed, mirroring the oneof branch: every
                        // variant the sibling group states (`as`) or
                        // shape-resolves is a candidate — a seal the
                        // union accumulator would have preserved is never
                        // missed.
                        for name in candidate_variants(index, ety, &sibs) {
                            for m in union_variant_vocab(index, &name, &sibs) {
                                push_model(&mut child, m);
                            }
                        }
                    } else if let FieldType::Arms { target, .. } = ety {
                        scan_arm_bodies(index, &fat, target, &nb.body, layer, out);
                    }
                }
                if !child.is_empty() {
                    seal_scan_body(index, &fat, &child, &nb.body, &sibs, layer, out);
                }
                // List targets: "at any depth" includes list items — the
                // laundering vector reopens through a sealed field on an
                // item model otherwise (shared with the modifier spelling
                // above).
                scan_list_items(
                    index,
                    &fat,
                    fields,
                    &item_refs(&entry.kind),
                    &sibling_items_at(siblings, &nb.name.name),
                    layer,
                    out,
                );
            }
            _ => {}
        }
    }
}
