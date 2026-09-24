//! Oneof bodies and arm-set replacement: discriminator handling at merge time.

use crate::ast::{ArmTarget, Body, BodyEntry, BodyEntryKind};
use crate::model::{FieldDef, FieldType, ModelDef, OneOfDef};

use super::*;
use crate::layers::decide::*;
use crate::layers::entries::*;
use crate::layers::instances::*;
use crate::layers::normalize::*;
use crate::layers::policy::*;
use crate::layers::seal::*;

impl<'a, 'd> Merger<'a, 'd> {
    /// Arm-set compose `(K -> V)` (RFC 0007): v1 is overlay-only — a
    /// layer that states the field replaces the WHOLE arm set (arm order
    /// carries first-match semantics, so additions-at-the-back would
    /// quietly dead-letter behind a base `else`) — subject to the seal
    /// backstop: a replacement discarding a displaced set whose INLINE
    /// arm bodies carry assigned `#sealed` fields (typed by the arm
    /// target) is NML2060, and the rejected layer contributes nothing.
    /// The target may be a model OR a oneof (a legal arm-set target —
    /// NML2076 warns at schema load and PROMISES the backstop): a oneof
    /// target judges each displaced arm body under the arm model its own
    /// stated discriminator (else the schema default) selects — exactly
    /// what that arm's displaced compose would carry. RFC 0019 binds the
    /// backstop to all three variant forms "equally"; resolving only
    /// `index.model` here was a seal-laundering hole for oneof targets.
    pub(in crate::layers) fn merge_arm_set(
        &mut self,
        path: &str,
        field: Option<&FieldDef>,
        contributions: &[Contribution<'a>],
    ) -> (Option<BodyEntry>, SurvivorSet) {
        let arms_target = field.and_then(|f| match effective_type(&f.field_type) {
            FieldType::Arms { target, .. } => Some(target.as_ref()),
            _ => None,
        });
        let target_name = arms_target.and_then(|t| match t {
            FieldType::ModelRef(n) => Some(n.as_str()),
            _ => None,
        });
        let target = target_name.and_then(|n| self.index.nameable(n));
        let mut effective: Option<usize> = None;
        for (ci, c) in contributions.iter().enumerate() {
            let BodyEntryKind::NestedBlock(nb) = &c.entry.kind else {
                continue;
            };
            if !nb
                .body
                .entries
                .iter()
                .any(|e| matches!(e.kind, BodyEntryKind::Arm(_)))
            {
                continue;
            }
            let Some(prev) = effective.map(|i| &contributions[i]) else {
                effective = Some(ci);
                continue;
            };
            {
                let BodyEntryKind::NestedBlock(prev_nb) = &prev.entry.kind else {
                    unreachable!("effective is nested by construction")
                };
                let mut sink = SealSink::new();
                for e in &prev_nb.body.entries {
                    let BodyEntryKind::Arm(arm) = &e.kind else {
                        continue;
                    };
                    let ArmTarget::Inline { body, .. } = &arm.target else {
                        continue;
                    };
                    let vocab = arm_body_vocab(self.index, target, body);
                    // Each displaced inline arm body is its own scope
                    // (`Seg::Arm`) — two arms' seals are two fields.
                    let aat = FieldIdentity::default().child(Seg::Arm(arm.selector.clone()));
                    displaced_group_seals_into(
                        self.index,
                        &aat,
                        &vocab,
                        &[(prev.layer, body)],
                        &mut sink,
                    );
                }
                let seals = sink.hits;
                if !seals.is_empty() {
                    self.emit(
                        c.layer,
                        seal_backstop_rejection(
                            BackstopFace::ArmSetReplacement,
                            path,
                            &seals,
                            c.entry.span,
                            c.layer,
                        ),
                    );
                    // Rejected: this layer contributes nothing.
                    continue;
                }
            }
            effective = Some(ci);
        }
        let Some(widx) = effective.or_else(|| contributions.len().checked_sub(1)) else {
            return (None, Vec::new());
        };
        let winner = &contributions[widx];
        self.record(path, winner.layer, winner.entry.span);
        // The effective set is the survivor; a losing set stays silent
        // (`Arm` interiors are never spelling-normalized — RFC 0025 §4).
        // The winner's inline arm bodies deep-normalize here (§2).
        let entry = self.emit_deep(
            field.map(|f| &f.field_type),
            winner.layer,
            winner.entry.clone(),
        );
        (Some(entry), vec![widx])
    }

    /// Oneof compose: the effective arm accumulates bottom-up from the
    /// schema default; omission inherits; a stated equal value deep-merges;
    /// a stated different value switches — wholesale, subject to the seal
    /// backstop (a discarded body with an assigned `#sealed` field, at any
    /// depth, is NML2060). THE FOLD DECIDES HERE, over the RAW bodies
    /// (RFC 0025 §2 — the only representation a seal judgment may start
    /// from); survivors then normalize under the decided arm inside
    /// [`Self::merge_model_bodies`], and the caller's subtraction home
    /// diagnoses the losers under their own readings (§4). An identity
    /// group's `token` materializes into the LOWEST surviving body
    /// before the body merge (§3). The composed entry follows the HEAD
    /// of the surviving group ([`Composed`]), derived from
    /// [`surviving_indexes`] under the oneof face — the one survivorship
    /// rule, never a second accumulator.
    pub(in crate::layers) fn merge_oneof_bodies(
        &mut self,
        path: &str,
        oneof: &OneOfDef,
        layers: &[(InstanceId<'a>, Body)],
        token: Option<&crate::identity::ItemToken>,
    ) -> Composed {
        let refs: Vec<(InstanceId<'a>, &Body)> = layers.iter().map(|(l, b)| (*l, b)).collect();
        let owned_trace = fold_arm_checked(self.index, oneof, &refs).1;
        #[cfg(test)]
        let owned_trace = {
            let mut t = owned_trace;
            fold_seams(path, &mut t);
            t
        };
        let trace: &[Decision<'a>] = &owned_trace;
        // The diagnostics pass — rejections report from their recorded
        // seals, the union-only verdicts are loud (fail safe, never
        // silent); survivorship is NOT tracked here.
        for (idx, (layer, body)) in layers.iter().enumerate() {
            let stated_entry = stated_discriminator_entry(body, &oneof.discriminator);
            // Positional: the trace was folded over these exact entries in
            // this exact order (alignment was checked above; the local
            // fold produces one decision per entry by construction).
            match &trace[idx].1 {
                ArmDecision::Rejected { seals } => {
                    let stated = stated_discriminator(body, &oneof.discriminator)
                        .unwrap_or_else(|| "?".to_string());
                    let at = stated_entry.map(|e| e.span).unwrap_or(seals[0].span);
                    self.emit(
                        *layer,
                        seal_backstop_rejection(
                            BackstopFace::ArmSwitch {
                                discriminator: &oneof.discriminator,
                                stated: &stated,
                            },
                            path,
                            seals,
                            at,
                            *layer,
                        ),
                    );
                    // Switch rejected: this layer contributes nothing.
                }
                // The oneof fold never discards (only unions have
                // structural variants) and never pins — fail SAFE AND
                // LOUD if the invariant ever breaks: say so (a silent
                // no-compose is the failure class NML2085 exists to make
                // visible), matching the module's fail-safe precedent
                // for believed-unreachable arms.
                ArmDecision::Discarded { .. } | ArmDecision::Pinned => {
                    self.internal_invariant(
                        path,
                        stated_entry
                            .map(|e| e.span)
                            .unwrap_or_else(|| body_anchor(body)),
                        *layer,
                        "a union-only verdict at a oneof position",
                        InvariantOutcome::Dropped,
                    );
                }
                ArmDecision::Switch | ArmDecision::Join => {}
            }
        }
        // The oneof FACE of survivorship: `Pinned` is a union-only
        // verdict, diagnosed `Dropped` above — the merge must agree with
        // its own message and exclude it (the union face keeps pins).
        let survivors: SurvivorSet = surviving_indexes(trace, Face::OneOf);
        debug_assert!(
            layers.is_empty() || !surviving_indexes(trace, Face::Union).is_empty(),
            "index 0 is always Switch or Join, so a non-empty stack has a survivor"
        );
        if !layers.is_empty() && survivors.is_empty() {
            // Reachable only under a tampered trace (index 0 is Switch or
            // Join on every real fold): every layer was diagnosed above as
            // not composed — compose what the diagnostics say.
            return Composed {
                body: Body::fresh(Vec::new()),
                head: 0,
                survivors,
            };
        }
        let head = survivors.first().copied().unwrap_or(0);
        // The effective discriminator is the FIRST stated one among the
        // survivors: after the last accepted switch every survivor omits
        // or restates it ([`fold_arm_checked`] — stating anything else
        // is a Switch or a Rejected), so the first statement is the
        // switch's own when one happened, the restated default
        // otherwise. Its entry is the canonical one the composed body
        // carries up front.
        let mut effective: Option<String> = oneof.default_discriminator.clone();
        let mut disc_entry: Option<(InstanceId<'a>, BodyEntry)> = None;
        for &i in &survivors {
            let (layer, body) = &layers[i];
            if let Some(entry) = stated_discriminator_entry(body, &oneof.discriminator) {
                effective = stated_discriminator(body, &oneof.discriminator);
                disc_entry = Some((*layer, entry.clone()));
                break;
            }
        }
        let group: Vec<(InstanceId<'a>, Body)> =
            survivors.iter().map(|&i| layers[i].clone()).collect();
        let arm_model = effective
            .as_ref()
            .and_then(|d| self.variant_model(oneof, d));
        // Strip discriminator entries from the group (the accumulator owns
        // the discriminator; one canonical entry is re-added below), then
        // materialize an identity group's token into the LOWEST surviving
        // body — after the fold, before the body merge (RFC 0025 §3):
        // injected earlier, an explicit restatement above a sealed `+`
        // would bypass write-once; injected into every survivor, the
        // copies would need the strip this design deleted.
        let mut stripped: Vec<(InstanceId<'a>, Body)> = group
            .iter()
            .map(|(l, b)| (*l, without_discriminator(b, &oneof.discriminator)))
            .collect();
        if let (Some(tok), Some(m), Some(first)) = (token, arm_model.as_ref(), stripped.first_mut())
        {
            let materialized = crate::identity::materialize_token(tok, &first.1, m);
            if materialized.validatable {
                first.1 = materialized.body;
            }
        }
        let mut merged = self.merge_model_bodies(path, arm_model.as_ref(), &stripped);
        // The surviving group's NON-STRING discriminator entries pass
        // through, in layer order, after the canonical entry (first,
        // when no survivor states a string one): the strip is by name,
        // so they never compose over each other, and the validator
        // reports each at its author's span (NML2042). Validator-facing
        // passthroughs, never effective entries — no provenance row. A
        // later STRING restatement is neither canonical nor passed
        // through: first-wins, exactly as the validator reads it.
        let mut front: Vec<BodyEntry> = Vec::new();
        if let Some((layer, entry)) = disc_entry {
            let disc_path = join_path(path, &oneof.discriminator);
            self.record(&disc_path, layer, entry.span);
            front.push(entry);
        }
        front.extend(
            group
                .iter()
                .flat_map(|(_, b)| &b.entries)
                .filter(|e| {
                    is_discriminator_named(e, &oneof.discriminator)
                        && !is_discriminator_entry(e, &oneof.discriminator)
                })
                .cloned(),
        );
        if !front.is_empty() {
            front.extend(merged.entries.iter().cloned());
            merged = merged.with_entries(front);
        }
        Composed {
            body: merged,
            head,
            survivors,
        }
    }

    fn variant_model(&self, oneof: &OneOfDef, discriminator: &str) -> Option<ModelDef> {
        variant_model_of(self.index, oneof, discriminator).cloned()
    }
}
