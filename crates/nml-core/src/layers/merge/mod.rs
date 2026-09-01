//! The merge engine: `Merger` over a linearized stack — model bodies, fields, policies, deep emission and discard diagnosis.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::ast::{
    Body, BodyEntry, BodyEntryKind, Identifier, ListItem, Modifier, ModifierValue, NestedBlock,
    SharedProperty,
};
use crate::diagnostic::{Diagnostic, codes};
use crate::diff::Origin;
use crate::model::{FieldDef, FieldType, ModelDef};
use crate::schema_index::{NameableVariant, SchemaIndex};
use crate::span::Span;

mod items;
mod oneof;
mod union;

use super::*;
use crate::layers::decide::*;
use crate::layers::entries::*;
use crate::layers::instances::*;
use crate::layers::normalize::*;
use crate::layers::policy::*;
use crate::layers::seal::*;
pub(in crate::layers) use items::*;
pub(in crate::layers) use union::*;

/// What the engine did with the contribution when an invariant broke.
#[derive(Clone, Copy)]
pub(in crate::layers) enum InvariantOutcome {
    /// The contribution was left out of the composed body.
    Dropped,
}

/// The NML2086 diagnostic — a violated internal composition invariant,
/// named with its position (elided at an instance root) and what became
/// of the contribution. Pure, so its wording is testable in every
/// build; [`Merger::internal_invariant`] pairs it with the debug
/// assertion.
pub(in crate::layers) fn internal_invariant_diag(
    path: &str,
    what: &str,
    outcome: InvariantOutcome,
) -> Diagnostic {
    let position = if path.is_empty() {
        String::new()
    } else {
        format!(" at '{path}'")
    };
    let became = match outcome {
        InvariantOutcome::Dropped => "this layer's contribution was not composed",
    };
    Diagnostic::error(format!(
        "internal composition invariant violated{position} ({what}) — {became}; \
         please report the input"
    ))
    .with_code(codes::INTERNAL_COMPOSE_INVARIANT)
}

/// One discarded contribution's coordinates for NML2085: where it sits,
/// which layer authored it, and where (and in which layer) the
/// establishment it lost to was made.
pub(in crate::layers) struct DiscardSite<'a> {
    est_at: Span,
    est_layer: InstanceId<'a>,
    at: Span,
    layer: InstanceId<'a>,
}

/// The surviving contribution indexes of one merge level (RFC 0025 §4)
/// — indexes into the level authority's input; every member NOT in the
/// set is a loser the subtraction diagnoses under its own readings
/// ([`Merger::diagnose_discards`]).
pub(in crate::layers) type SurvivorSet = Vec<usize>;

/// A composed body plus the group-relative index of the HEAD of its
/// surviving group — the layer whose contribution the composed body IS:
/// the base when nothing switched; the switching layer after an
/// accepted switch; unchanged by a pin (a pin names, it does not
/// displace) or by a rejection. The head extends RFC 0019's receiver
/// rule from the body to the composed ENTRY (span, name identifier,
/// provenance row — erratum E15): the entry keeps the base SLOT
/// (replace in place), but a finding on the composed block anchors at
/// the layer that produced it. `survivors` carries the level's
/// surviving input indexes up to the subtraction home (RFC 0025 §4).
pub(in crate::layers) struct Composed {
    body: Body,
    head: usize,
    survivors: SurvivorSet,
}

impl Composed {
    /// A merge with no switch semantics (model and model-less bodies):
    /// the head is the base and every input survives.
    fn base(body: Body, members: usize) -> Self {
        Composed {
            body,
            head: 0,
            survivors: (0..members).collect(),
        }
    }
}

pub(in crate::layers) struct Merger<'a, 'd> {
    pub(in crate::layers) index: &'a SchemaIndex,
    pub(in crate::layers) sink: &'d mut ComposeSink<'a>,
    pub(in crate::layers) origins: ProvenanceTable,
}

impl<'a, 'd> Merger<'a, 'd> {
    /// Route a finding to the sink stamped with the offending layer
    /// (RFC 0025 §5). Every merge emitter goes through here.
    pub(in crate::layers) fn emit(&mut self, layer: InstanceId<'a>, d: Diagnostic) {
        self.sink.emit(layer, d);
    }

    pub(in crate::layers) fn record(&mut self, path: &str, layer: InstanceId<'_>, span: Span) {
        self.origins.push((
            path.to_string(),
            Origin::File {
                file: layer.source_path.into(),
                span,
            },
        ));
    }

    /// Raw, array-ref-inlined bodies in; the composed body out (RFC 0025
    /// §2). A oneof root is a subtraction home (§4): a rejected or
    /// switch-displaced LAYER's whole body is diagnosed under its own
    /// stated arm, else the schema default — a model root subtracts only
    /// at the model level.
    pub(in crate::layers) fn compose_root(
        &mut self,
        root: &str,
        inlined: &[(InstanceId<'a>, Body)],
    ) -> Body {
        // The one resolution order (`nameable`) — a model before a
        // oneof of the same name, as every pass reads it.
        let index = self.index;
        match index.nameable(root) {
            Some(NameableVariant::OneOf(oneof)) => {
                let composed = self.merge_oneof_bodies("", oneof, inlined, None);
                self.diagnose_discards("", inlined, &composed.survivors, |(id, body)| {
                    let vocab = Vocab::of_model(
                        fold_arm(oneof, &[body]).and_then(|d| variant_model_of(index, oneof, &d)),
                    );
                    Some((*id, Cow::Borrowed(body), vocab))
                });
                composed.body
            }
            Some(NameableVariant::Model(model)) => {
                self.merge_model_bodies("", Some(model), inlined)
            }
            None => self.merge_model_bodies("", None, inlined),
        }
    }

    /// ONE owner of the loser subtraction's loop (RFC 0025 §4): every
    /// member not in `survivors` projects to `(layer, body, vocabulary)`
    /// — its OWN reading — and is diagnosed by a Deep normalization
    /// whose output is discarded; `None` is a bodyless member
    /// (Reference/Role items, scalar entries), silent. A forgotten
    /// survivor produces a FALSE dead-body warning instead of a silently
    /// missing one; the `DISCARDS` seam is the observable.
    pub(in crate::layers) fn diagnose_discards<'i, M>(
        &mut self,
        path: &str,
        members: &[M],
        survivors: &SurvivorSet,
        project: impl for<'m> Fn(&'m M) -> Option<(InstanceId<'a>, Cow<'m, Body>, Vocab<'i>)>,
    ) {
        debug_assert!(
            survivors.iter().all(|&i| i < members.len()),
            "survivors index into the members at '{path}'"
        );
        for (i, m) in members.iter().enumerate() {
            if survivors.contains(&i) {
                continue;
            }
            let Some((layer, body, vocab)) = project(m) else {
                continue;
            };
            #[cfg(test)]
            DISCARDS.with(|d| {
                d.borrow_mut()
                    .push((path.to_string(), layer.name.to_string()));
            });
            let _ = normalize_level(
                self.index,
                vocab,
                body.as_ref(),
                Descend::Deep,
                Some((layer, &mut *self.sink)),
            );
        }
    }

    /// Merge RAW bodies against a model: step 0 normalizes each body
    /// `ThisLevel` under the level's model (RFC 0025 §2) — nested
    /// bodies pass through raw to their own level — then one VALUE
    /// entry per field name (a declaration passes through ahead of it),
    /// replace in place at the base entry's position
    /// (`Body::with_entries` on the establishing layer's body — the
    /// receiver carries the annotation). This is a subtraction home
    /// (§4): a losing contribution's NESTED body is diagnosed under its
    /// own reading — its own-level facts were already ThisLevel's.
    pub(in crate::layers) fn merge_model_bodies(
        &mut self,
        path: &str,
        model: Option<&ModelDef>,
        raw: &[(InstanceId<'a>, Body)],
    ) -> Body {
        let mut layers: Vec<(InstanceId<'a>, Cow<'_, Body>)> = Vec::with_capacity(raw.len());
        for (id, b) in raw {
            let leveled = normalize_level(
                self.index,
                Vocab::of_model(model),
                b,
                Descend::ThisLevel,
                Some((*id, &mut *self.sink)),
            );
            layers.push((*id, leveled));
        }
        let layers = &layers;
        let establishing: &Body = layers
            .iter()
            .find(|(_, b)| !b.entries.is_empty())
            .map(|(_, b)| b)
            .unwrap_or(&layers.last().expect("stack is non-empty").1);

        // One name→field map per merge level: the per-entry linear scan of
        // `model.fields` was O(width²) on wide fully-populated bodies —
        // measured at tens of seconds from a sub-megabyte hostile file
        // across 16 layers. FIRST-wins on a duplicate field name, exactly
        // like the linear `find` this replaced — last-wins would silently
        // swap which duplicate's `#sealed` governs (fail-open on a broken
        // schema, the inverse of the module's doctrine).
        let field_map = first_wins_field_map(model);
        // Gather contributions per entry name, in first-seen (base) order;
        // pass-through kinds (stray items, field definitions) carry
        // EVERY layer's copies through unchanged — they are not merged,
        // and on valid input at most one layer supplies them.
        let mut order: Vec<String> = Vec::new();
        let mut by_name: HashMap<String, Vec<Contribution<'a>>> = HashMap::new();
        let mut passthrough: Vec<(InstanceId<'a>, BodyEntry)> = Vec::new();
        // Declarations (type-annotation modifiers) ahead of their field's
        // composed value — the LAST one per field; never a contribution
        // (they neither seal, switch, nor replace: RFC 0019 errata E12).
        let mut declarations: HashMap<String, BodyEntry> = HashMap::new();
        let mut arm_sets: Vec<(InstanceId<'a>, Vec<BodyEntry>)> = Vec::new();
        for (layer, body) in layers {
            let mut layer_arms: Vec<BodyEntry> = Vec::new();
            for entry in &body.entries {
                // One gather rule for every named entry kind. Spelling is
                // authoring, not identity: when the schema declares the
                // field, a modifier entry gathers under the SAME key as
                // the property/block spellings — two groups for one field
                // would let each spelling dodge the other's seal (the
                // dual-spelling write-once bypass) and emit two composed
                // entries for one field. Undeclared modifier names keep
                // the `|` namespace: with no schema saying otherwise the
                // two spellings are structurally distinct, and validation
                // flags them.
                let named = match &entry.kind {
                    BodyEntryKind::Property(p) => Some(p.name.name.clone()),
                    BodyEntryKind::NestedBlock(nb) => Some(nb.name.name.clone()),
                    BodyEntryKind::Modifier(m) => {
                        Some(if field_map.contains_key(m.name.name.as_str()) {
                            m.name.name.clone()
                        } else {
                            format!("|{}", m.name.name)
                        })
                    }
                    _ => None,
                };
                if let Some(name) = named {
                    if !by_name.contains_key(&name) && !declarations.contains_key(&name) {
                        order.push(name.clone());
                    }
                    if is_value_entry(&entry.kind) {
                        by_name.entry(name).or_default().push(Contribution {
                            layer: *layer,
                            entry: entry.clone(),
                        });
                    } else {
                        declarations.insert(name, entry.clone());
                    }
                    continue;
                }
                match &entry.kind {
                    BodyEntryKind::Property(_)
                    | BodyEntryKind::NestedBlock(_)
                    | BodyEntryKind::Modifier(_) => unreachable!("gathered above"),
                    BodyEntryKind::Arm(_) => layer_arms.push(entry.clone()),
                    // SharedProperty was consumed by normalization; a
                    // survivor means the body was list-shaped (handled by
                    // list merge) — pass through from its own layer.
                    BodyEntryKind::SharedProperty(_)
                    | BodyEntryKind::FieldDefinition(_)
                    | BodyEntryKind::ListItem(_) => {
                        passthrough.push((*layer, entry.clone()));
                    }
                }
            }
            if !layer_arms.is_empty() {
                arm_sets.push((*layer, layer_arms));
            }
        }

        let mut entries: Vec<BodyEntry> = Vec::new();
        for name in &order {
            let Some(contributions) = by_name.get(name) else {
                // A declaration-only field: the declaration IS the entry.
                if let Some(decl) = declarations.remove(name) {
                    entries.push(decl);
                }
                continue;
            };
            // Undeclared modifier groups keep their `|` key, which can
            // never match a field name (`|` is not an identifier
            // character) — so a plain map lookup is total; the old
            // `"|" + f.name == name` disjunct was unreachable once the
            // gather canonicalized declared modifiers.
            let field = field_map.get(name.as_str()).copied();
            // The declaration precedes its value — as authored (declare,
            // then assign), so the composed view reads like the source.
            if let Some(decl) = declarations.remove(name) {
                entries.push(decl);
            }
            let (entry, survivors) = self.merge_field(path, name, field, contributions);
            self.diagnose_field_losers(path, name, field, contributions, &survivors);
            if let Some(entry) = entry {
                entries.push(entry);
            }
        }
        // Arm-set fields compose by overlay: the highest layer that states
        // arms replaces the whole set. The seal backstop for displaced arm
        // bodies binds through the arm target's typed body, which arrives
        // with union compose (arm targets are consumer-typed references in
        // this slice, so there is no schema-named sealed surface to scan
        // yet — the replacement itself is loud in `nml resolve`). The
        // winning set's inline bodies still consume their own `.shared`
        // scopes (RFC 0025 §2's deep pass recursed into arm bodies); the
        // set is untyped here, so no positional and no headers.
        if let Some((_, arms)) = arm_sets.last() {
            for entry in arms {
                entries.push(deep_arm_entry(entry.clone()));
            }
        }
        // Passthrough items are output-bound entries that pass no merge
        // level (RFC 0025 §2): their interiors deep-normalize here —
        // `.shared` scopes then spellings, never positional, exactly the
        // spellings a direct item in a model body received before.
        for (pt_layer, entry) in passthrough {
            let entry = match entry.kind {
                BodyEntryKind::ListItem(item) => BodyEntry {
                    span: entry.span,
                    kind: BodyEntryKind::ListItem(self.deep_item(
                        Vocab::of_model(model),
                        pt_layer,
                        &item,
                    )),
                },
                _ => entry,
            };
            entries.push(entry);
        }
        establishing.with_entries(entries)
    }

    /// The model home of the loser subtraction (RFC 0025 §4): a losing
    /// contribution's NESTED body — its own-level facts were already
    /// emitted by ThisLevel — diagnosed Deep under its own reading.
    /// Arm-set losers stay silent (`Arm` interiors are never
    /// normalized); scalar losers carry no body; a losing block
    /// modifier's items are its body.
    fn diagnose_field_losers(
        &mut self,
        path: &str,
        name: &str,
        field: Option<&FieldDef>,
        contributions: &[Contribution<'a>],
        survivors: &SurvivorSet,
    ) {
        let fty = field.map(|f| effective_type(&f.field_type));
        if matches!(fty, Some(FieldType::Arms { .. })) {
            return;
        }
        let field_path = join_path(path, name);
        let index = self.index;
        self.diagnose_discards(&field_path, contributions, survivors, |c| {
            match &c.entry.kind {
                BodyEntryKind::NestedBlock(nb) => Some((
                    c.layer,
                    Cow::Borrowed(&nb.body),
                    fty.map_or(Vocab::None, |t| own_vocab(index, t, &nb.body)),
                )),
                BodyEntryKind::Modifier(m) => match &m.value {
                    ModifierValue::Block(items) if !items.is_empty() => Some((
                        c.layer,
                        Cow::Owned(list_body_of(items.clone())),
                        fty.and_then(list_inner).map_or(Vocab::None, Vocab::Items),
                    )),
                    _ => None,
                },
                _ => None,
            }
        });
    }

    /// Which merge owns a group — ownership, in order:
    ///
    /// 0. **Declarations are not contributions.** A type-annotation
    ///    modifier (`|slot (a | b)` inside an instance body) is a
    ///    declaration, never a value: the gather routes it to the
    ///    passthrough ahead of the composed value (the composed view keeps
    ///    it, so the validator still checks it), and it never reaches a
    ///    merge (it neither seals, switches, nor replaces). Only the last
    ///    declaration of a field survives; earlier ones are dropped.
    /// 1. **`#sealed`** → `merge_sealed`, whatever the type or spelling:
    ///    write-once is judged by `seal_write` alone (no annotation
    ///    synthesis, no NML2085 — the seal rejects the upper entry
    ///    before its shape is read).
    /// 2. **A union-typed position** → `merge_union`, every spelling; a
    ///    dependent's bogus `as` is reported here (NML2051), on the
    ///    sealed route too.
    /// 3. **An all-modifier group** → `merge_modifier` under its policy.
    /// 4. **Policy**: list policies → `merge_list`; overlay →
    ///    `merge_overlay`.
    fn merge_field(
        &mut self,
        path: &str,
        name: &str,
        field: Option<&FieldDef>,
        contributions: &[Contribution<'a>],
    ) -> (Option<BodyEntry>, SurvivorSet) {
        let policy = field.map(policy_of).unwrap_or_default();
        let field_path = join_path(path, name);
        let union_ty = field
            .map(|f| effective_type(&f.field_type))
            .filter(|t| t.union_variants().is_some());
        if let Some(union_ty) = union_ty {
            self.report_unknown_union_annotations(
                union_ty,
                contributions.iter().filter_map(|c| match &c.entry.kind {
                    BodyEntryKind::NestedBlock(nb) => Some((c.layer, &nb.body)),
                    _ => None,
                }),
            );
        }
        let all_modifiers = contributions
            .iter()
            .all(|c| matches!(c.entry.kind, BodyEntryKind::Modifier(_)));
        match route_of(policy, union_ty, all_modifiers) {
            FieldRoute::Sealed => self.merge_sealed(&field_path, field, contributions),
            FieldRoute::Union(union_ty) => self.merge_union(&field_path, union_ty, contributions),
            // All-modifier groups route by KIND (modifier output shape);
            // a group MIXING spellings of one declared field routes by
            // policy — `items_of` gives every merge path one view of the
            // items regardless of spelling.
            FieldRoute::Modifier => self.merge_modifier(&field_path, policy, field, contributions),
            FieldRoute::List => self.merge_list(&field_path, policy, field, contributions),
            FieldRoute::Overlay => self.merge_overlay(&field_path, field, contributions),
        }
    }

    /// `#sealed`: write-once from the bottom. Any higher assignment is
    /// NML2060 — even at the identical value (with a structural deletion
    /// fix; on a sealed field this takes precedence over NML2084). The
    /// winner passes no merge level, so its interior deep-normalizes
    /// here (RFC 0025 §2); survivors: the first real writer, or an
    /// all-zero stack's last entry (§4).
    fn merge_sealed(
        &mut self,
        path: &str,
        field: Option<&FieldDef>,
        contributions: &[Contribution<'a>],
    ) -> (Option<BodyEntry>, SurvivorSet) {
        // "Assigned" means ONE thing, on BOTH sides of the seal: the
        // shared `seal_write` predicate. A zero-item LIST entry (NML2079's
        // contract is explicitly list-scoped) can neither hold a seal —
        // the first REAL assignment seals — nor violate one: above a
        // sealed list it is the same warned no-op it is everywhere else.
        // Every other shape — scalars, and crucially object-typed fields,
        // whose nested blocks hold properties rather than list items —
        // writes with every entry: classifying an object body as "zero
        // items" would let an upper layer silently replace a sealed
        // object wholesale. All-zero stacks pass the last entry through
        // so the field stays present-but-empty for validation. A missing
        // field definition fails closed: every entry is a write.
        let is_write = |kind: &BodyEntryKind| field.is_none_or(|f| seal_write(f, kind));
        let first_real = contributions.iter().position(|c| is_write(&c.entry.kind));
        let Some(pos) = first_real else {
            let Some(last) = contributions.last() else {
                return (None, Vec::new());
            };
            self.record(path, last.layer, last.entry.span);
            let entry =
                self.emit_deep(field.map(|f| &f.field_type), last.layer, last.entry.clone());
            return (Some(entry), vec![contributions.len() - 1]);
        };
        let (first, rest) = contributions[pos..]
            .split_first()
            .expect("position yields a non-empty tail");
        for c in rest {
            if !is_write(&c.entry.kind) {
                continue;
            }
            // Equal-value detection spans the scalar SPELLINGS — the
            // promised deletion fix must not vanish because one side was
            // modifier-spelled.
            let scalar_of = |kind: &BodyEntryKind| match kind {
                BodyEntryKind::Property(p) => Some(p.value.value.clone()),
                BodyEntryKind::Modifier(m) => match &m.value {
                    ModifierValue::Inline(sv) => Some(sv.value.clone()),
                    _ => None,
                },
                _ => None,
            };
            let equal = match (scalar_of(&first.entry.kind), scalar_of(&c.entry.kind)) {
                (Some(a), Some(b)) => a.semantic_eq(&b),
                _ => false,
            };
            // "A lower layer" reads wrong when both entries sit in ONE
            // body — name the real relationship.
            let by = if first.layer == c.layer {
                "an earlier assignment in this same layer"
            } else {
                "a lower layer"
            };
            let mut d = if equal {
                Diagnostic::error(format!(
                    "'{path}' is already sealed to this same value by \
                     {by}; restating it would silently decouple if the \
                     base changes — delete this assignment"
                ))
                .with_deletion(c.entry.span)
            } else {
                Diagnostic::error(format!(
                    "assignment to `#sealed` field '{path}' — {by} already \
                     fixed it"
                ))
            };
            d = d
                .with_code(codes::SEALED_FIELD_VIOLATION)
                .with_span(c.entry.span)
                .with_source(c.layer.source_path.to_string())
                .with_related_in(
                    first.entry.span,
                    "sealed here",
                    Some(first.layer.source_path.to_string()),
                );
            self.emit(c.layer, d);
        }
        self.record(path, first.layer, first.entry.span);
        let entry = self.emit_deep(
            field.map(|f| &f.field_type),
            first.layer,
            first.entry.clone(),
        );
        (Some(entry), vec![pos])
    }

    /// Overlay: scalars replace (later wins, NML2084 on a dead delta);
    /// nested blocks deep-merge recursively — with variant identity
    /// composing before fields for oneof-typed positions.
    fn merge_overlay(
        &mut self,
        path: &str,
        field: Option<&FieldDef>,
        contributions: &[Contribution<'a>],
    ) -> (Option<BodyEntry>, SurvivorSet) {
        let target = field.map(|f| effective_type(&f.field_type));
        // Union-typed positions never reach here: `merge_field` routes
        // every spelling of a union field to the union authority first.
        debug_assert!(
            !target.is_some_and(|t| t.union_variants().is_some()),
            "union positions are routed by merge_field"
        );
        // Arm-set field `(K -> V)` (RFC 0007): v1 composes by overlay
        // only — whole-set replacement, subject to the seal backstop.
        if let Some(FieldType::Arms { .. }) = target {
            return self.merge_arm_set(path, field, contributions);
        }
        if let Some(FieldType::List(_) | FieldType::Set(_)) = target {
            // Bare-overlay list, across EVERY spelling (`items_of` sees
            // block, array-property, and modifier forms as one): the
            // highest layer that SUPPLIES items (≥1) replaces wholesale;
            // zero-item entries were already warned and are no-ops — an
            // `xs = []` must never EMPTY the base list through the
            // scalar-overlay path. When NO layer supplies items, the
            // field survives authored-empty rather than dropping (a
            // valid inherited `xs = []` must not turn into a
            // missing-required error). The winner passes no merge level
            // — its interior deep-normalizes here — and the losing
            // lists' interiors are the subtraction's (RFC 0025 §§2, 4).
            let refs: Vec<&Contribution<'a>> = contributions.iter().collect();
            let Some(widx) = bare_list_winner(&refs) else {
                return (None, Vec::new());
            };
            let winner = &contributions[widx];
            self.record(path, winner.layer, winner.entry.span);
            let entry = self.emit_deep(
                field.map(|f| &f.field_type),
                winner.layer,
                winner.entry.clone(),
            );
            return (Some(entry), vec![widx]);
        }
        // An object-typed field (model- or oneof-ModelRef) ALWAYS
        // deep-merges its nested contributions — the RFC's "nested blocks
        // always deep-merge, there is no whole-object replacement form,
        // which is what closes object-level seal laundering". Routing by
        // whether EVERY contribution is nested let a scalar/modifier
        // spelling (`cfg = "gone"`, invalid for an object field) drop the
        // group into scalar-overlay and discard a sealed nested body.
        // Route by TARGET instead: gather the nested bodies and
        // deep-merge them. A dropped non-nested spelling is currently
        // SILENT (the substituted validation view never sees it —
        // tracked as the scalar-on-object validator gap); it still can
        // never win the field.
        let nested: Vec<(usize, InstanceId<'a>, &NestedBlock)> = contributions
            .iter()
            .enumerate()
            .filter_map(|(i, c)| match &c.entry.kind {
                BodyEntryKind::NestedBlock(nb) => Some((i, c.layer, nb)),
                _ => None,
            })
            .collect();
        if let Some(FieldType::ModelRef(type_name)) = target {
            let index = self.index;
            // The one resolution order (`nameable`), the same reading
            // every pass shares: a model before a oneof of the same name.
            let named = index.nameable(type_name);
            if let (Some(named), false) = (named, nested.is_empty()) {
                let sub: Vec<(InstanceId<'a>, Body)> = nested
                    .iter()
                    .map(|(_, l, nb)| (*l, nb.body.clone()))
                    .collect();
                let composed = match named {
                    NameableVariant::OneOf(oneof) => {
                        self.merge_oneof_bodies(path, oneof, &sub, None)
                    }
                    NameableVariant::Model(model) => {
                        Composed::base(self.merge_model_bodies(path, Some(model), &sub), sub.len())
                    }
                };
                // The composed entry carries the HEAD contribution's span
                // and name — the base when nothing switched, the switching
                // layer after an accepted arm switch (RFC 0019 E15) — so a
                // finding on the composed block (a switched arm's missing
                // field) anchors at the layer that produced the body. The
                // nested level's survivors map back to contribution
                // indexes; a non-nested spelling is a scalar loser with
                // no body — silent, unchanged.
                let survivors: SurvivorSet =
                    composed.survivors.iter().map(|&gi| nested[gi].0).collect();
                let (_, head_layer, head_nb) = nested[composed.head];
                self.record(path, head_layer, head_nb.name.span);
                return (
                    Some(BodyEntry {
                        span: head_nb.name.span,
                        kind: BodyEntryKind::NestedBlock(NestedBlock {
                            name: head_nb.name.clone(),
                            body: composed.body,
                        }),
                    }),
                    survivors,
                );
            }
        }
        // All-nested groups WITHOUT a resolvable object target — the
        // structural (no-schema) mode, an undeclared field, a dangling
        // type name — still deep-merge, name-keyed and model-less:
        // wholesale replacement here silently discarded every lower
        // layer's nested data (and the structural mode is a documented,
        // fixture-pinned capability). Two carve-outs keep this honest:
        // a RESOLVABLE non-object target keeps plain last-wins — a
        // scalar-typed field holding nested garbage is invalid either
        // way (deep-merging just reshapes the garbage), and UNION-typed
        // groups never reach here (any nested contribution routed to
        // `merge_union` above); and ITEM-BEARING groups follow the
        // bare-list rule below.
        let unresolvable_target = matches!(target, None | Some(FieldType::ModelRef(_)));
        if unresolvable_target && !nested.is_empty() && nested.len() == contributions.len() {
            // The bare-list rule (RFC 0019: an un-granted list "replaces
            // wholesale") binds structural mode too: deep-merging an
            // item-bearing group would CONCATENATE the layers' items and
            // duplicate restated identities — silent artifact corruption.
            let item_bearing = nested.iter().any(|(_, _, nb)| {
                nb.body
                    .entries
                    .iter()
                    .any(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
            });
            if item_bearing {
                let refs: Vec<&Contribution<'a>> = contributions.iter().collect();
                let Some(widx) = bare_list_winner(&refs) else {
                    return (None, Vec::new());
                };
                let winner = &contributions[widx];
                self.record(path, winner.layer, winner.entry.span);
                let entry = self.emit_deep(None, winner.layer, winner.entry.clone());
                return (Some(entry), vec![widx]);
            }
            let sub: Vec<(InstanceId<'a>, Body)> = nested
                .iter()
                .map(|(_, l, nb)| (*l, nb.body.clone()))
                .collect();
            let (_, base_layer, base_nb) = nested[0];
            let merged = self.merge_model_bodies(path, None, &sub);
            self.record(path, base_layer, base_nb.name.span);
            return (
                Some(BodyEntry {
                    span: base_nb.name.span,
                    kind: BodyEntryKind::NestedBlock(NestedBlock {
                        name: base_nb.name.clone(),
                        body: merged,
                    }),
                }),
                (0..contributions.len()).collect(),
            );
        }
        // Scalar overlay: later wins; a dead delta warns (NML2084). The
        // loser is a scalar with no interior (NML2084 is its own), so
        // every contribution survives (RFC 0025 §4).
        let refs: Vec<&Contribution<'a>> = contributions.iter().collect();
        let entry = self
            .scalar_overlay(path, &refs)
            .map(|(layer, e)| self.emit_deep(field.map(|f| &f.field_type), layer, e));
        (entry, (0..contributions.len()).collect())
    }

    fn item_target(&self, field: Option<&FieldDef>) -> ItemTarget {
        let Some(inner) = field.and_then(|f| list_inner(effective_type(&f.field_type))) else {
            return ItemTarget::Opaque;
        };
        if inner.union_variants().is_some() {
            return ItemTarget::Union(inner.clone());
        }
        let FieldType::ModelRef(n) = inner else {
            return ItemTarget::Opaque;
        };
        match self.index.nameable(n) {
            Some(NameableVariant::Model(m)) => ItemTarget::Model(m.clone()),
            Some(NameableVariant::OneOf(o)) => ItemTarget::OneOf(o.clone()),
            None => ItemTarget::Opaque,
        }
    }

    /// NML2086 — an internal composition invariant the engine holds to
    /// be unreachable was reached: fail safe and LOUD (the position
    /// elided at an instance root), never silently wrong. The module's
    /// precedent for believed-unreachable arms. The diagnostic makes the
    /// reach visible in every build; the compose boundary
    /// ([`resolve_layers`]) asserts on it in debug builds, so every
    /// compose-driven test stays loud while a direct `Merger` test can
    /// observe the diagnostic AND the fail-safe composition it
    /// describes in every build.
    pub(in crate::layers) fn internal_invariant(
        &mut self,
        path: &str,
        at: Span,
        layer: InstanceId<'a>,
        what: &str,
        outcome: InvariantOutcome,
    ) {
        self.emit(
            layer,
            internal_invariant_diag(path, what, outcome)
                .with_span(at)
                .with_source(layer.source_path.to_string()),
        );
    }

    /// Plain overlay over a union position's structural survivors,
    /// dispatched on the establishment (a homogeneous slice by
    /// construction — the fold discards every cross-shape supply): a
    /// list establishment composes by the bare-list rule, a scalar one
    /// by scalar overlay (later wins, NML2084 on a dead delta) — the
    /// same two rules `merge_overlay` applies to structurally TYPED
    /// fields, through the same owners. Survivors (RFC 0025 §4): the
    /// bare-list winner (losing lists' interiors are the subtraction's
    /// — fixture U1), or the last structural survivor.
    pub(in crate::layers) fn structural_overlay(
        &mut self,
        path: &str,
        union_ty: &FieldType,
        established: &Establishment,
        contributions: &[Contribution<'a>],
        structural: &[usize],
    ) -> (Option<BodyEntry>, SurvivorSet) {
        let refs: Vec<&Contribution<'a>> = structural.iter().map(|&i| &contributions[i]).collect();
        match established {
            Establishment::Items => {
                let Some(w) = bare_list_winner(&refs) else {
                    return (None, Vec::new());
                };
                let widx = structural[w];
                let winner = &contributions[widx];
                self.record(path, winner.layer, winner.entry.span);
                let entry = self.emit_deep(Some(union_ty), winner.layer, winner.entry.clone());
                (Some(entry), vec![widx])
            }
            _ => {
                let survivors: SurvivorSet =
                    structural.last().map(|&i| vec![i]).unwrap_or_default();
                let entry = self
                    .scalar_overlay(path, &refs)
                    .map(|(layer, e)| self.emit_deep(Some(union_ty), layer, e));
                (entry, survivors)
            }
        }
    }

    /// Scalar overlay: later wins (returned with its layer); a dead
    /// delta warns (NML2084). The one owner for the rule, shared by
    /// structurally typed fields and the scalar slice of a union
    /// position.
    fn scalar_overlay(
        &mut self,
        path: &str,
        contributions: &[&Contribution<'a>],
    ) -> Option<(InstanceId<'a>, BodyEntry)> {
        let winner = contributions.last()?;
        for pair in contributions.windows(2) {
            if let (BodyEntryKind::Property(lo), BodyEntryKind::Property(hi)) =
                (&pair[0].entry.kind, &pair[1].entry.kind)
            {
                if lo.value.value.semantic_eq(&hi.value.value) {
                    self.emit(
                        pair[1].layer,
                        Diagnostic::warning(format!(
                            "'{path}' restates the effective lower value \
                             unchanged — a dead delta that silently \
                             decouples when the base changes"
                        ))
                        .with_code(codes::DEAD_DELTA)
                        .with_span(pair[1].entry.span)
                        .with_source(pair[1].layer.source_path.to_string()),
                    );
                }
            }
        }
        self.record(path, winner.layer, winner.entry.span);
        Some((winner.layer, winner.entry.clone()))
    }

    fn merge_modifier(
        &mut self,
        path: &str,
        policy: MergePolicy,
        field: Option<&FieldDef>,
        contributions: &[Contribution<'a>],
    ) -> (Option<BodyEntry>, SurvivorSet) {
        // A modifier without a list-policy grant composes like a bare list:
        // the highest layer that supplies items replaces wholesale (its
        // items are authored, not overridden). Only granted policies merge.
        if policy == MergePolicy::Overlay {
            // Same zero-item contract as bare lists: an empty overlay
            // modifier must never EMPTY the base's list (a security-shaped
            // allow-by-emptying); it is a warned no-op, and only an
            // item-bearing layer replaces.
            // Declared-type-aware: only a field that admits items has a
            // zero-item no-op (a `[]` on a declared scalar modifier is a
            // VALUE — a type error the validator owns, and it must reach
            // the composed view to be reported).
            let zero_item = |kind: &BodyEntryKind| {
                zero_item_at(field.map(|f| effective_type(&f.field_type)), kind)
            };
            // Declarations never reach a merge (the gather routes them to
            // the passthrough), so every contribution here is a value:
            // the highest item-bearing one wins; a zero-item-only group
            // survives authored-empty. The winner's items deep-normalize
            // here; losing modifiers' items are the subtraction's
            // (RFC 0025 §§2, 4).
            let Some(widx) = contributions
                .iter()
                .rposition(|c| !zero_item(&c.entry.kind))
                .or_else(|| contributions.len().checked_sub(1))
            else {
                return (None, Vec::new());
            };
            let winner = &contributions[widx];
            self.record(path, winner.layer, winner.entry.span);
            let entry = self.emit_deep(
                field.map(|f| &f.field_type),
                winner.layer,
                winner.entry.clone(),
            );
            return (Some(entry), vec![widx]);
        }
        // One tuple per LAYER, its entries' items concatenated in document
        // order — per-contribution tuples would let a layer stating the
        // field twice dodge the within-layer duplicate-identity check and
        // misread its own later entries as another layer's overlay. A
        // `ModifierValue::Block` carries no `.shared` lines (RFC 0025
        // §3): the tuple's shared slot stays empty.
        let mut survivors: SurvivorSet = Vec::new();
        let mut items_per_layer: Vec<(InstanceId<'a>, Span, &Identifier, Vec<ListItem>)> =
            Vec::new();
        for (ci, c) in contributions.iter().enumerate() {
            let BodyEntryKind::Modifier(m) = &c.entry.kind else {
                continue;
            };
            let Some(items) = items_of(&c.entry.kind) else {
                continue;
            };
            survivors.push(ci);
            match items_per_layer.last_mut() {
                Some((l, _, _, acc)) if *l == c.layer => acc.extend(items),
                _ => items_per_layer.push((c.layer, c.entry.span, &m.name, items)),
            }
        }
        let Some((base_layer, base_span, name, _)) = items_per_layer.first() else {
            // No item-bearing contribution at all (e.g. only a
            // type-annotation form): keep the last entry rather than
            // silently deleting the field from the composed body.
            let all: SurvivorSet = (0..contributions.len()).collect();
            let Some(last) = contributions.last() else {
                return (None, all);
            };
            self.record(path, last.layer, last.entry.span);
            return (Some(last.entry.clone()), all);
        };
        let target = self.item_target(field);
        let merged = self.merge_items(
            path,
            policy,
            &target,
            &items_per_layer
                .iter()
                .map(|(l, sp, _, items)| (*l, *sp, Vec::new(), items.clone()))
                .collect::<Vec<_>>(),
        );
        self.record(path, *base_layer, *base_span);
        (
            Some(BodyEntry {
                span: *base_span,
                kind: BodyEntryKind::Modifier(Modifier {
                    name: (*name).clone(),
                    value: ModifierValue::Block(merged),
                }),
            }),
            survivors,
        )
    }

    /// List-policy merge over per-layer item vectors, each layer's
    /// list-level `.shared` lines carried beside its items (RFC 0025 §3
    /// — `merge_items` distributes them into that layer's members before
    /// the fold, yielding to an item's identity token).
    fn merge_list(
        &mut self,
        path: &str,
        policy: MergePolicy,
        field: Option<&FieldDef>,
        contributions: &[Contribution<'a>],
    ) -> (Option<BodyEntry>, SurvivorSet) {
        let target = self.item_target(field);
        // One tuple per LAYER (same-layer entries concatenate in document
        // order — the within-layer duplicate check must see the whole
        // layer), extracting items across EVERY spelling via `items_of`:
        // block, escaped-normalization array property (a spelling gap must
        // merge, never vanish), and modifier entries mixed with the
        // others of one declared field. Every item-bearing contribution
        // survives; a non-item spelling is a scalar loser with no body —
        // silently skipped, unchanged (RFC 0025 §4).
        let mut survivors: SurvivorSet = Vec::new();
        let mut per_layer: Vec<(InstanceId<'a>, Span, Vec<SharedProperty>, Vec<ListItem>)> =
            Vec::new();
        for (ci, c) in contributions.iter().enumerate() {
            let Some(items) = items_of(&c.entry.kind) else {
                continue;
            };
            survivors.push(ci);
            let shared = shared_of(&c.entry.kind);
            match per_layer.last_mut() {
                Some((l, _, sh, acc)) if *l == c.layer => {
                    sh.extend(shared);
                    acc.extend(items);
                }
                _ => per_layer.push((c.layer, c.entry.span, shared, items)),
            }
        }
        let Some(base) = contributions.first() else {
            return (None, survivors);
        };
        let name = match &base.entry.kind {
            BodyEntryKind::NestedBlock(nb) => nb.name.clone(),
            BodyEntryKind::Property(p) => p.name.clone(),
            BodyEntryKind::Modifier(m) => m.name.clone(),
            _ => return (None, survivors),
        };
        let merged = self.merge_items(path, policy, &target, &per_layer);
        // Output shape follows the base's spelling: a modifier-spelled
        // base stays a modifier entry (the parallel arm of the engine),
        // everything else canonicalizes to the block spelling.
        let kind = if matches!(&base.entry.kind, BodyEntryKind::Modifier(_)) {
            BodyEntryKind::Modifier(Modifier {
                name,
                value: ModifierValue::Block(merged),
            })
        } else {
            BodyEntryKind::NestedBlock(NestedBlock {
                name,
                body: Body::fresh(
                    merged
                        .into_iter()
                        .map(|item| BodyEntry {
                            span: item.span,
                            kind: BodyEntryKind::ListItem(item),
                        })
                        .collect(),
                ),
            })
        };
        (
            Some(BodyEntry {
                span: base.entry.span,
                kind,
            }),
            survivors,
        )
    }

    /// RFC 0025 §2 — deep-normalize an output-bound entry that passes no
    /// merge level (a sealed first write, an arm-set winner, a bare-list
    /// winner, a structural or modifier overlay winner): its INTERIOR
    /// only, under its own vocabulary — the entry's own-level facts
    /// (spelling, its own NML2079) were already emitted by ThisLevel. An
    /// `Arms` target recurses per inline arm body under the arm target's
    /// own reading: positional and `.shared` only, no spellings (`Vocab`
    /// has no arms case and spellings never enter `Arm` entries, as
    /// today); modifier-block items get `.shared` scopes and spellings,
    /// never positional (the deep positionalizer does not walk modifier
    /// entries). Scalar spellings pass through — nothing inside.
    pub(in crate::layers) fn emit_deep(
        &mut self,
        target: Option<&FieldType>,
        layer: InstanceId<'a>,
        entry: BodyEntry,
    ) -> BodyEntry {
        let ety = target.map(effective_type);
        if let Some(FieldType::Arms {
            target: arm_target, ..
        }) = ety
        {
            return self.deep_arm_set(arm_target, entry);
        }
        let index = self.index;
        let kind = match entry.kind {
            BodyEntryKind::NestedBlock(nb) => {
                let vocab = ety.map_or(Vocab::None, |t| own_vocab(index, t, &nb.body));
                let body = normalize_level(
                    index,
                    vocab,
                    &nb.body,
                    Descend::Deep,
                    Some((layer, &mut *self.sink)),
                )
                .into_owned();
                BodyEntryKind::NestedBlock(NestedBlock {
                    name: nb.name,
                    body,
                })
            }
            BodyEntryKind::Modifier(m) => match m.value {
                ModifierValue::Block(items) => {
                    let vocab = ety.and_then(list_inner).map_or(Vocab::None, Vocab::Items);
                    let items = items
                        .iter()
                        .map(|item| self.deep_item(vocab, layer, item))
                        .collect();
                    BodyEntryKind::Modifier(Modifier {
                        name: m.name,
                        value: ModifierValue::Block(items),
                    })
                }
                value => BodyEntryKind::Modifier(Modifier {
                    name: m.name,
                    value,
                }),
            },
            other => other,
        };
        BodyEntry {
            kind,
            span: entry.span,
        }
    }

    /// One passthrough item's deep pass (RFC 0025 §2): its own `.shared`
    /// scopes, then spellings — never positional, exactly the treatment
    /// a direct item in a model body and a modifier-block item received
    /// from the whole-layer pass.
    fn deep_item(&mut self, vocab: Vocab<'_>, layer: InstanceId<'a>, item: &ListItem) -> ListItem {
        let shared = crate::resolve::apply_shared_in_item(item.clone());
        let mut diags = Vec::new();
        let item = normalize_item(self.index, vocab, &shared, layer.source_path, &mut diags);
        for d in diags {
            self.emit(layer, d);
        }
        item
    }

    /// The `Arms` leg of [`Self::emit_deep`]: each inline arm body under
    /// the arm target's own reading — positional and `.shared` only, no
    /// spellings; the header was injected by ThisLevel, where the deep
    /// positionalizer materializes it (and injection is lenient, so the
    /// second pass here is a no-op on it).
    fn deep_arm_set(&mut self, arm_target: &FieldType, entry: BodyEntry) -> BodyEntry {
        let BodyEntryKind::NestedBlock(nb) = &entry.kind else {
            return entry;
        };
        let index = self.index;
        let body = crate::identity::map_inline_arm_bodies(
            arm_target,
            &nb.body,
            index,
            0,
            |elem, inline_body, depth| {
                let positional =
                    crate::identity::apply_positional_against(index, elem, inline_body, depth);
                crate::resolve::apply_shared_properties(&positional)
            },
        );
        BodyEntry {
            span: entry.span,
            kind: BodyEntryKind::NestedBlock(NestedBlock {
                name: nb.name.clone(),
                body,
            }),
        }
    }
}
