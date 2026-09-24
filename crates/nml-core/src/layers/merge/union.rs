//! Union-typed positions (RFC 0015): the union fold's merge legs, group replay and discard diagnosis.

use std::borrow::Cow;

use crate::ast::{Body, BodyEntry, BodyEntryKind, Identifier, NestedBlock};
use crate::diagnostic::{Diagnostic, codes};
use crate::model::{FieldType, ModelDef};
use crate::schema_index::NameableVariant;
use crate::span::Span;
use crate::types::{SpannedValue, Value};

use super::*;
use crate::layers::decide::*;
use crate::layers::entries::*;
use crate::layers::instances::*;
use crate::layers::seal::*;

/// The composed union body's output annotation, shared by both faces:
/// the authoring identifier when one was authored (the establishing
/// layer's `as`, or the pinning layer's), else a synthesized one —
/// authored by no one, deliberately, so the merged shape can never
/// re-infer a different variant.
fn union_output_annotation(
    authored: Option<Identifier>,
    synthesized: bool,
    variant: String,
    est_span: Span,
) -> Identifier {
    match (authored, synthesized) {
        (Some(id), false) => id,
        _ => Identifier {
            name: variant,
            span: est_span,
        },
    }
}

/// The replayed outcome at a union position (indexes into the supply
/// list): which supplies survive, split by where they land, the
/// establishing entry (whose name and span the output carries), and the
/// pinning entry when an authored `as` resolved an ambiguous group (its
/// identifier is the output annotation) — see [`Merger::replay_union`].
#[derive(Default)]
struct UnionReplay {
    /// The supply whose authored identifier the output annotation
    /// carries: the pinning layer's when one resolved an ambiguous
    /// group, else the establishing layer's own.
    annotated_by: Option<usize>,
    /// The surviving body supplies, in order — the FIRST is the
    /// establishing one (its entry name and span are the output's), so
    /// "establishing" is never a second field to keep in step.
    group: Vec<usize>,
    structural: Vec<usize>,
}

/// The bare-list rule's winner (its INDEX into the slice): the highest
/// contribution that SUPPLIES items (≥1) replaces wholesale; zero-item
/// entries are warned no-ops; when no layer supplies items the field
/// survives authored-empty rather than dropping (a valid inherited
/// `xs = []` must not turn into a missing-required error). One owner
/// for every list spelling and for the list slice of a union position.
pub(in crate::layers) fn bare_list_winner(contributions: &[&Contribution<'_>]) -> Option<usize> {
    contributions
        .iter()
        .rposition(|c| items_of(&c.entry.kind).is_some_and(|v| !v.is_empty()))
        .or_else(|| {
            contributions
                .iter()
                .rposition(|c| items_of(&c.entry.kind).is_some())
        })
        .or_else(|| contributions.len().checked_sub(1))
}

impl<'a, 'd> Merger<'a, 'd> {
    /// Union compose (RFC 0015 nominal unions) at FIELD scope: every
    /// contribution is a supply through the ONE constructor
    /// ([`union_supplies`]), classified and folded over the RAW bodies —
    /// the only representation a seal judgment may start from (a fold
    /// over normalized bodies counts a variant's machinery injections as
    /// authored writes; RFC 0025 §2). The establishment picks the output
    /// — a named-variant merge with an explicit annotation, a model-less
    /// un-annotated merge for an ambiguous group (NML2052 stays the
    /// validator's), or the structural overlay of the surviving whole
    /// values. Survivors per RFC 0025 §4: the replay's group for the
    /// body establishments, the bare-list winner or the last structural
    /// survivor for the structural ones.
    pub(in crate::layers) fn merge_union(
        &mut self,
        path: &str,
        union_ty: &FieldType,
        contributions: &[Contribution<'a>],
    ) -> (Option<BodyEntry>, SurvivorSet) {
        let entries: Vec<(InstanceId<'a>, &BodyEntry)> =
            contributions.iter().map(|c| (c.layer, &c.entry)).collect();
        let supplies = union_supplies(self.index, union_ty, &entries);
        let (established, owned_trace) = fold_variant_checked(self.index, union_ty, &supplies);
        #[cfg(test)]
        let owned_trace = {
            let mut t = owned_trace;
            fold_seams(path, &mut t);
            t
        };
        let trace: &[Decision<'a>] = &owned_trace;
        let Some(established) = established else {
            // Zero-item entries only: nothing supplies the position — it
            // survives authored-empty rather than dropping (the bare-list
            // rule's own verdict). A block survivor is re-spelled `= []`,
            // the one spelling every consumer reads as the empty list
            // (an entry-less block reads as an empty OBJECT of the first
            // model variant downstream); a modifier survivor keeps its
            // spelling. Zero-item entries were warned at ThisLevel;
            // every contribution survives (RFC 0025 §4).
            let all: SurvivorSet = (0..contributions.len()).collect();
            let Some(last) = contributions.last() else {
                return (None, all);
            };
            self.record(path, last.layer, last.entry.span);
            let (name, span) = match &last.entry.kind {
                BodyEntryKind::NestedBlock(nb) => (nb.name.clone(), nb.name.span),
                _ => return (Some(last.entry.clone()), all),
            };
            return (
                Some(BodyEntry {
                    span: last.entry.span,
                    kind: BodyEntryKind::Property(crate::ast::Property {
                        name,
                        value: SpannedValue::new(Value::Array(Vec::new()), span),
                    }),
                }),
                all,
            );
        };
        let replay = self.replay_union(path, &established, trace, &supplies, |i| {
            contributions[i].entry.span
        });
        // The body group, with a members index mapping each group
        // position back to its contribution — the head a variant merge
        // returns is group-relative.
        let mut members: Vec<usize> = Vec::new();
        let mut group: Vec<(InstanceId<'a>, Body)> = Vec::new();
        for &i in &replay.group {
            if let Some(b) = supplies[i].1.body() {
                members.push(i);
                group.push((contributions[i].layer, b.clone()));
            }
        }
        let survivors: SurvivorSet = match &established {
            Establishment::Named { .. } | Establishment::Ambiguous { .. } => replay.group.clone(),
            // Structural survivors are picked below (the bare-list
            // winner, or the last structural survivor).
            Establishment::Value | Establishment::Items => Vec::new(),
        };
        match established {
            Establishment::Named { .. } | Establishment::Ambiguous { .. } => {
                let Some(&est) = replay.group.first() else {
                    // Unreachable by construction (a body establishment
                    // has a body-bearing establishing supply) — fail
                    // safe and LOUD, never silently drop the field.
                    let Some(last) = contributions.last() else {
                        return (None, survivors);
                    };
                    self.internal_invariant(
                        path,
                        last.entry.span,
                        last.layer,
                        "a body establishment with no establishing supply",
                        InvariantOutcome::Dropped,
                    );
                    return (Some(last.entry.clone()), survivors);
                };
                let BodyEntryKind::NestedBlock(est_nb) = &contributions[est].entry.kind else {
                    // Same: a body establishment is a nested block by
                    // construction (synthesized list bodies classify
                    // only as Items/Empty).
                    let c = &contributions[est];
                    self.internal_invariant(
                        path,
                        c.entry.span,
                        c.layer,
                        "a body establishment on a non-block entry",
                        InvariantOutcome::Dropped,
                    );
                    self.record(path, c.layer, c.entry.span);
                    return (Some(c.entry.clone()), survivors);
                };
                let est_span = contributions[est].entry.span;
                let (out_body, head) = match &established {
                    Establishment::Named {
                        variant,
                        synthesized,
                    } => {
                        let composed = self.merge_variant_group(path, variant, &group, None);
                        let mut merged = composed.body;
                        // The authored identifier: the pinning layer's
                        // `as` when one resolved an ambiguous group,
                        // else the establishing layer's own. The
                        // ANNOTATION stays on the establishment — an arm
                        // switch beneath moves the entry, never the
                        // establishing or pinning identifier.
                        let authored = replay
                            .annotated_by
                            .and_then(|i| supplies[i].1.body())
                            .or(Some(&est_nb.body))
                            .and_then(|b| b.type_annotation.clone());
                        merged.type_annotation = Some(union_output_annotation(
                            authored,
                            *synthesized,
                            variant.clone(),
                            est_span,
                        ));
                        // The group-relative head names the contribution
                        // whose span and name the composed entry carries
                        // (RFC 0019 E15); est is the fail-safe.
                        let head = members.get(composed.head).copied().unwrap_or(est);
                        (merged, head)
                    }
                    // The D2 oracle refused to pick a variant, so compose
                    // does too: model-less deep merge, NO annotation — the
                    // composed body reaches the validator exactly as
                    // ambiguous as the authored one, and NML2052 fires
                    // there with its full teaching.
                    _ => (self.merge_model_bodies(path, None, &group), est),
                };
                // A head member is body-bearing, so its entry is a nested
                // block by construction; est (already verified above) is
                // the fail-safe should that ever break.
                let (head, head_nb) = match &contributions[head].entry.kind {
                    BodyEntryKind::NestedBlock(nb) => (head, nb),
                    _ => (est, est_nb),
                };
                let head_span = contributions[head].entry.span;
                self.record(path, contributions[head].layer, head_span);
                (
                    Some(BodyEntry {
                        span: head_span,
                        kind: BodyEntryKind::NestedBlock(NestedBlock {
                            name: head_nb.name.clone(),
                            body: out_body,
                        }),
                    }),
                    survivors,
                )
            }
            Establishment::Value | Establishment::Items => self.structural_overlay(
                path,
                union_ty,
                &established,
                contributions,
                &replay.structural,
            ),
        }
    }

    /// Carry a union trace out over its supplies — ONE replay for both
    /// faces (field scope and item scope), index-based so each face maps
    /// survivors back to its own representation: rejections and
    /// discards are reported (with the context the fold recorded — the
    /// merge never re-derives a verdict), a switch restarts the group, a
    /// pin joins and takes the annotation, a join lands in the body
    /// group or the structural slice by the establishment in force
    /// (under a list establishment the effective list is the highest
    /// supplier, so "established here" follows it), and a zero-item
    /// entry joins as a no-op.
    fn replay_union(
        &mut self,
        path: &str,
        established: &Establishment,
        trace: &[Decision<'a>],
        supplies: &[(InstanceId<'a>, UnionSupply<'_>)],
        anchor: impl Fn(usize) -> Span,
    ) -> UnionReplay {
        let mut replay = UnionReplay::default();
        let body_group = matches!(
            established,
            Establishment::Named { .. } | Establishment::Ambiguous { .. }
        );
        // The establishing site (span, layer) in force — the "established
        // here" note and the same-layer relation of a later discard.
        let mut est: Option<(Span, InstanceId<'a>)> = None;
        for (idx, (layer, supply)) in supplies.iter().enumerate() {
            // Positional: the trace was folded over these exact supplies
            // in this exact order — the local fold produces exactly one
            // decision per supply, by construction.
            match &trace[idx].1 {
                ArmDecision::Rejected { seals } => {
                    let body = supply
                        .body()
                        .expect("only an authored `as` body is rejected");
                    self.report_variant_switch_rejection(path, seals, body, anchor(idx), *layer);
                }
                ArmDecision::Discarded { over, lost } => {
                    // A discard is judged against an establishment in
                    // force, so the establishing site is recorded by now;
                    // fail safe (anchor on the discard itself) and loud
                    // should that invariant ever break.
                    let (est_at, est_layer) = match est {
                        Some(site) => site,
                        None => {
                            self.internal_invariant(
                                path,
                                anchor(idx),
                                *layer,
                                "a discard before any establishment",
                                InvariantOutcome::Dropped,
                            );
                            (anchor(idx), *layer)
                        }
                    };
                    let site = DiscardSite {
                        est_at,
                        est_layer,
                        at: anchor(idx),
                        layer: *layer,
                    };
                    self.discarded_union_contribution(path, over, lost, site);
                }
                ArmDecision::Switch => {
                    replay.group.clear();
                    replay.structural.clear();
                    replay.group.push(idx);
                    replay.annotated_by = Some(idx);
                    est = Some((anchor(idx), *layer));
                }
                ArmDecision::Pinned => {
                    replay.group.push(idx);
                    replay.annotated_by = Some(idx);
                    est = Some((anchor(idx), *layer));
                }
                ArmDecision::Join => match supply {
                    UnionSupply::Empty => {}
                    _ if body_group && supply.body().is_some() => {
                        // The first body of a group establishes it.
                        if replay.group.is_empty() {
                            replay.annotated_by = Some(idx);
                            est = Some((anchor(idx), *layer));
                        }
                        replay.group.push(idx);
                    }
                    _ => {
                        // A scalar overlay's later value wins; a list's
                        // highest item supplier wins — either way the
                        // effective entry is the latest structural
                        // survivor.
                        est = Some((anchor(idx), *layer));
                        replay.structural.push(idx);
                    }
                },
            }
        }
        replay
    }

    /// The one NML2060 emission for a rejected union `as` switch, shared
    /// by the field-scope and item-scope faces.
    fn report_variant_switch_rejection(
        &mut self,
        path: &str,
        seals: &[SealHit<'a>],
        body: &Body,
        fallback_at: Span,
        layer: InstanceId<'a>,
    ) {
        let stated = body
            .type_annotation
            .as_ref()
            .map(|i| i.name.as_str())
            .unwrap_or("?");
        let at = body
            .type_annotation
            .as_ref()
            .map(|i| i.span)
            .unwrap_or(fallback_at);
        self.emit(
            layer,
            seal_backstop_rejection(
                BackstopFace::VariantSwitch { stated },
                path,
                seals,
                at,
                layer,
            ),
        );
    }

    /// Merge a union group under its decided variant — a model directly,
    /// a oneof through the arm fold. An identity group's `token` (RFC
    /// 0025 §3) rides through to the decided level, which materializes
    /// it into the lowest surviving body before the body merge; a
    /// dangling variant name has no model, so no materialization.
    fn merge_variant_group(
        &mut self,
        path: &str,
        variant: &str,
        group: &[(InstanceId<'a>, Body)],
        token: Option<&crate::identity::ItemToken>,
    ) -> Composed {
        let index = self.index;
        match index.nameable(variant) {
            Some(NameableVariant::OneOf(oneof)) => {
                self.merge_oneof_bodies(path, oneof, group, token)
            }
            Some(NameableVariant::Model(model)) => Composed::base(
                self.merge_model_with_token(path, Some(model), group, token),
                group.len(),
            ),
            None => Composed::base(self.merge_model_bodies(path, None, group), group.len()),
        }
    }

    /// [`Self::merge_model_bodies`] with an identity group's token
    /// materialized into the LOWEST surviving body first — at a model
    /// level every body survives, so the base — before the body merge
    /// (RFC 0025 §3). Materialization diagnostics are discarded, as the
    /// deep positionalizer discards them; a token with no `+` field on
    /// the decided model injects nothing.
    pub(in crate::layers) fn merge_model_with_token(
        &mut self,
        path: &str,
        model: Option<&ModelDef>,
        group: &[(InstanceId<'a>, Body)],
        token: Option<&crate::identity::ItemToken>,
    ) -> Body {
        if let (Some(tok), Some(m), Some((id, base))) = (token, model, group.first()) {
            let materialized = crate::identity::materialize_token(tok, base, m);
            if materialized.validatable {
                let mut owned: Vec<(InstanceId<'a>, Body)> = group.to_vec();
                owned[0] = (*id, materialized.body);
                return self.merge_model_bodies(path, model, &owned);
            }
        }
        self.merge_model_bodies(path, model, group)
    }

    /// NML2085 — RFC 0019's loud-not-silent floor for the union shape
    /// conflict the fold cannot compose: a contribution that can neither
    /// merge into the establishment in force nor switch it. The face is
    /// keyed on the (establishment it lost to, establishment it would
    /// have made) pair the FOLD recorded — the merge never re-derives it
    /// — leads with the position and the establishment (naming the
    /// relation: a lower layer, or an earlier entry in this same layer),
    /// names the losing spelling and what WOULD switch, and points a
    /// related note at the establishing entry.
    fn discarded_union_contribution(
        &mut self,
        path: &str,
        over: &Establishment,
        lost: &Establishment,
        site: DiscardSite<'a>,
    ) {
        let relation = if site.est_layer == site.layer {
            "by an earlier entry in this same layer"
        } else {
            "by a lower layer"
        };
        let clause = format!("{} {relation}", over.clause());
        let msg = match (over, lost) {
            (
                Establishment::Named { .. } | Establishment::Ambiguous { .. },
                Establishment::Value | Establishment::Items,
            ) => {
                let noun = match lost {
                    Establishment::Items => "list",
                    _ => "value",
                };
                let fix = match over {
                    Establishment::Ambiguous { candidates } => format!(
                        "resolve the establishing body with `as <{}>`, or switch with an \
                         authored `as` on a nested body",
                        candidates.join(" | ")
                    ),
                    _ => "compose into the established variant, or switch with an \
                          authored `as` on a nested body"
                        .to_string(),
                };
                let target = match over {
                    Establishment::Ambiguous { .. } => "an established body",
                    _ => "an established variant",
                };
                format!(
                    "'{path}' is established {clause} — a whole-value \
                     spelling never switches {target}; this {noun} is \
                     discarded ({fix})"
                )
            }
            (
                Establishment::Value | Establishment::Items,
                Establishment::Named { .. } | Establishment::Ambiguous { .. },
            ) => {
                let hint = match lost {
                    Establishment::Named { variant, .. } => {
                        format!("author `as {variant}` to switch")
                    }
                    Establishment::Ambiguous { candidates } => {
                        format!("author `as <{}>` to switch", candidates.join(" | "))
                    }
                    _ => unreachable!("matched a body establishment"),
                };
                let instead = match over {
                    Establishment::Items => "supply a list instead",
                    _ => "supply a scalar value instead",
                };
                format!(
                    "'{path}' is established {clause} — an un-annotated \
                     body never switches the variant; this body is \
                     discarded ({hint}, or {instead})"
                )
            }
            (Establishment::Value, Establishment::Items)
            | (Establishment::Items, Establishment::Value) => {
                let (noun, instead) = match lost {
                    Establishment::Items => ("list", "supply a scalar value instead"),
                    _ => ("scalar", "supply a list instead"),
                };
                format!(
                    "'{path}' is established {clause} — a {noun} value \
                     cannot merge into it, and structural variants have no \
                     `as` spelling to switch between; this value is \
                     discarded ({instead})"
                )
            }
            // Same-class pairs never discard (a body over a body
            // establishment joins; same-shape structural supplies join)
            // — pinned by the rule-table test; loud if ever reached.
            (
                Establishment::Named { .. } | Establishment::Ambiguous { .. },
                Establishment::Named { .. } | Establishment::Ambiguous { .. },
            )
            | (Establishment::Value, Establishment::Value)
            | (Establishment::Items, Establishment::Items) => {
                self.internal_invariant(
                    path,
                    site.at,
                    site.layer,
                    "a same-class discard",
                    InvariantOutcome::Dropped,
                );
                return;
            }
        };
        // A body establishment was made at one entry; a structural one
        // is merely the value in force (the latest scalar, the highest
        // list supplier) — name it honestly.
        let note = match over {
            Establishment::Named { .. } | Establishment::Ambiguous { .. } => "established here",
            Establishment::Value | Establishment::Items => "in force here",
        };
        self.emit(
            site.layer,
            Diagnostic::error(msg)
                .with_code(codes::DISCARDED_UNION_CONTRIBUTION)
                .with_span(site.at)
                .with_source(site.layer.source_path.to_string())
                .with_related_in(
                    site.est_at,
                    note,
                    Some(site.est_layer.source_path.to_string()),
                ),
        );
    }

    /// NML2051 for authored `as` annotations naming no variant — the
    /// fold's fail-safe treats them as un-annotated (a bogus name must
    /// not switch anything), but the composed view replaces the
    /// annotation before the validator can see the authored one, so the
    /// merge is the one place left to report it — through the SAME
    /// builder the validator uses, so a non-`uses` base declaration's
    /// own raw finding and this one are one FindingKey (one home).
    pub(in crate::layers) fn report_unknown_union_annotations<'b>(
        &mut self,
        union_ty: &FieldType,
        bodies: impl Iterator<Item = (InstanceId<'a>, &'b Body)>,
    ) {
        let Some(variants) = union_ty.union_variants() else {
            return;
        };
        for (layer, body) in bodies {
            let Some(ann) = &body.type_annotation else {
                continue;
            };
            if stated_variant(self.index, union_ty, body).is_some() {
                continue;
            }
            self.emit(
                layer,
                self.index
                    .unknown_union_variant(variants, ann)
                    .with_source(layer.source_path.to_string()),
            );
        }
    }

    /// Union compose over identity-matched ITEM bodies — the body-level
    /// face of [`Self::merge_union`], with the same
    /// establishment/switch/backstop/annotation contract and the same
    /// replay, folded ONCE per group over the raw, shared-distributed
    /// member bodies (RFC 0025 §3). An item scope reads only its own
    /// bodies — the fold is local by construction. Reachable under an
    /// identity policy on a union-element list (NML2068 at schema load,
    /// but composition still runs over the loaded schema and embedders
    /// may skip the loader's policy check), so it is a live face, not
    /// defense in depth. `anchors` are the items' own spans, one per
    /// body (an entry-less item body has no span of its own to anchor
    /// on); `token` is the group's scalar key, materialized by the
    /// decided variant's level.
    pub(in crate::layers) fn merge_union_bodies(
        &mut self,
        path: &str,
        union_ty: &FieldType,
        layers: &[(InstanceId<'a>, Body)],
        anchors: &[Span],
        token: Option<&crate::identity::ItemToken>,
    ) -> Composed {
        debug_assert_eq!(layers.len(), anchors.len(), "one anchor per item body");
        let anchor = |i: usize| -> Span { anchors[i] };
        self.report_unknown_union_annotations(union_ty, layers.iter().map(|(l, b)| (*l, b)));
        let supplies: Vec<(InstanceId<'a>, UnionSupply<'_>)> = layers
            .iter()
            .map(|(l, b)| {
                (
                    *l,
                    UnionSupply::classify(self.index, union_ty, Cow::Borrowed(b)),
                )
            })
            .collect();
        let (established, trace) = fold_variant_checked(self.index, union_ty, &supplies);
        #[cfg(test)]
        let trace = {
            let mut t = trace;
            fold_seams(path, &mut t);
            t
        };
        let Some(established) = established else {
            // Zero-item bodies only: nothing to establish — model-less
            // merge keeps whatever (nothing) they carry; the head is the
            // base; every layer survives (RFC 0025 §4).
            return Composed::base(self.merge_model_bodies(path, None, layers), layers.len());
        };
        let replay = self.replay_union(path, &established, &trace, &supplies, anchor);
        let group: Vec<(InstanceId<'a>, Body)> =
            replay.group.iter().map(|&i| layers[i].clone()).collect();
        match established {
            Establishment::Named {
                variant,
                synthesized,
            } => {
                let Some(&est) = replay.group.first() else {
                    let (l, _) = &layers[0];
                    self.internal_invariant(
                        path,
                        anchor(0),
                        *l,
                        "an item body establishment with no establishing supply",
                        InvariantOutcome::Dropped,
                    );
                    // Merging the (empty) group here panicked on "stack
                    // is non-empty" — fail safe over ALL the item bodies
                    // instead, model-less.
                    return Composed::base(
                        self.merge_model_bodies(path, None, layers),
                        layers.len(),
                    );
                };
                let authored = replay
                    .annotated_by
                    .map(|i| &layers[i].1)
                    .unwrap_or(&layers[est].1)
                    .type_annotation
                    .clone();
                let composed = self.merge_variant_group(path, &variant, &group, token);
                let mut merged = composed.body;
                merged.type_annotation = Some(union_output_annotation(
                    authored,
                    synthesized,
                    variant,
                    anchor(est),
                ));
                // The head maps through the group to the item body whose
                // span, name token and owner the merged item carries
                // (RFC 0019 E15); est is the fail-safe.
                let head = replay.group.get(composed.head).copied().unwrap_or(est);
                Composed {
                    body: merged,
                    head,
                    survivors: replay.group,
                }
            }
            Establishment::Ambiguous { .. } => Composed {
                body: self.merge_model_bodies(path, None, &group),
                head: replay.group.first().copied().unwrap_or(0),
                survivors: replay.group,
            },
            // Structural item bodies: model-less deep merge of the
            // surviving slice — the pre-union-route verdict, preserved
            // for structural spellings. No single layer IS a deep-merged
            // body; the first structural survivor anchors; the WHOLE
            // structural slice survives (a model-less deep merge —
            // RFC 0025 §4).
            Establishment::Value | Establishment::Items => {
                let survivors: Vec<(InstanceId<'a>, Body)> = replay
                    .structural
                    .iter()
                    .map(|&i| layers[i].clone())
                    .collect();
                Composed {
                    body: self.merge_model_bodies(path, None, &survivors),
                    head: replay.structural.first().copied().unwrap_or(0),
                    survivors: replay.structural,
                }
            }
        }
    }
}
