//! The merge-time deciders (RFC 0025 §2): the arm and variant folds over raw supplies, survivorship, and the §6 test seams.

use std::borrow::Cow;

use crate::ast::{ArmTarget, Body, BodyEntry, BodyEntryKind};
use crate::model::{FieldType, ModelDef, OneOfDef};
use crate::schema_index::{BodyShape, FieldTarget, SchemaIndex};
use crate::types::Value;

use super::entries::*;
use super::instances::*;
use super::normalize::*;
use super::seal::*;

/// The arms a group of bodies at one oneof position could have made
/// effective: the schema default plus every discriminator value any body
/// states, in encounter order, deduplicated.
pub(in crate::layers) fn candidate_arms(
    oneof: &OneOfDef,
    bodies: &[(InstanceId<'_>, &Body)],
) -> Vec<String> {
    let mut arms: Vec<String> = Vec::new();
    if let Some(d) = &oneof.default_discriminator {
        arms.push(d.clone());
    }
    for (_, b) in bodies {
        for e in &b.entries {
            if let BodyEntryKind::Property(p) = &e.kind {
                if p.name.name == oneof.discriminator {
                    if let Value::String(s) = &p.value.value {
                        if !arms.contains(s) {
                            arms.push(s.clone());
                        }
                    }
                }
            }
        }
    }
    arms
}

/// The variant a body's SHAPE selects, by declared type name — the one
/// [`SchemaIndex::resolve_type_in_body`] reading every union consumer
/// shares (the fold's classifier, the candidate scan, the spelling
/// normalizer, the discard hint). `None` for structural shapes.
fn inferred_variant(index: &SchemaIndex, union_ty: &FieldType, body: &Body) -> Option<String> {
    match index.resolve_type_in_body(union_ty, body) {
        FieldTarget::Model(m) => Some(m.name.clone()),
        FieldTarget::OneOf(o) => Some(o.name.clone()),
        _ => None,
    }
}

/// Every union variant a group of bodies could have made effective —
/// each body's authored `as` (valid names only), else its shape — the
/// union analogue of [`candidate_arms`], consumed by the fail-closed
/// seal scan.
pub(in crate::layers) fn candidate_variants(
    index: &SchemaIndex,
    union_ty: &FieldType,
    bodies: &[(InstanceId<'_>, &Body)],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |n: String| {
        if !out.contains(&n) {
            out.push(n);
        }
    };
    for (_, b) in bodies {
        // ONE classification: an oracle-ambiguous body could have made
        // EVERY candidate effective — all of them are candidates (the
        // resolver's first-wins pick would make the scan variant-order
        // dependent).
        match UnionSupply::classify(index, union_ty, Cow::Borrowed(b)) {
            UnionSupply::Authored { variant, .. } | UnionSupply::Inferred { variant, .. } => {
                push(variant)
            }
            UnionSupply::Ambiguous { candidates, .. } => {
                for c in candidates {
                    push(c);
                }
            }
            UnionSupply::Items { .. } | UnionSupply::Empty | UnionSupply::Value => {}
        }
    }
    out
}

/// The seal scan's arm-set interior walk: each INLINE arm body of an
/// arms-typed field scans under the arm target's vocabulary — a model
/// target directly, a oneof target under the arm each body's own stated
/// discriminator (else the schema default) selects. Reference/literal
/// arms own no inline body: the referenced instance survives the
/// replacement, nothing sealed is discarded.
pub(in crate::layers) fn scan_arm_bodies<'a>(
    index: &SchemaIndex,
    at: &FieldIdentity,
    target: &FieldType,
    body: &Body,
    layer: InstanceId<'a>,
    out: &mut SealSink<'a>,
) {
    let FieldType::ModelRef(n) = target else {
        return;
    };
    let target = index.nameable(n);
    for e in &body.entries {
        let BodyEntryKind::Arm(arm) = &e.kind else {
            continue;
        };
        let ArmTarget::Inline { body: ab, .. } = &arm.target else {
            continue;
        };
        let vocab = arm_body_vocab(index, target, ab);
        if !vocab.is_empty() {
            let sibs = [(layer, ab)];
            // Each inline arm body is its own scope (`Seg::Arm`): two
            // arms' seals of one field name are two FIELDS.
            let aat = at.child(Seg::Arm(arm.selector.clone()));
            seal_scan_body(index, &aat, &vocab, ab, &sibs, layer, out);
        }
    }
}

/// One layer's decision at a position.
pub(in crate::layers) type Decision<'a> = (InstanceId<'a>, ArmDecision<'a>);

/// A position's per-layer decision trace — a fold's output, the merge's
/// replay input.
pub(in crate::layers) type DecisionTrace<'a> = Vec<Decision<'a>>;

/// One layer's fate at one variant-typed position, decided by a fold.
#[derive(Debug, PartialEq)]
pub(in crate::layers) enum ArmDecision<'a> {
    /// Omitted or restated-at-effective: the layer joins the group and
    /// deep-merges (a structural union supply joins the structural
    /// overlay).
    Join,
    /// Accepted switch: the group restarts at this layer.
    Switch,
    /// Backstop-rejected switch: the layer contributes nothing; the
    /// merge emits NML2060 from these recorded seals (position-relative
    /// paths; lowest-then-document order, `.len()` is the count).
    Rejected { seals: SealHits<'a> },
    /// Union shape conflict (RFC 0015): the contribution can neither
    /// merge into the establishment in force nor switch it (only an
    /// authored `as` switches) — discarded, loudly: the merge emits
    /// NML2085 from the context recorded HERE (the establishment it
    /// lost to and what it was), never re-deriving a verdict.
    Discarded {
        over: Establishment,
        lost: Establishment,
    },
    /// An authored `as` resolved an ambiguous group (RFC 0015 D2 meets
    /// RFC 0019): the layer joins, and its identifier becomes the
    /// group's annotation — never a switch, nothing was displaced.
    Pinned,
}

/// The two faces of variant survivorship (RFC 0025 §6): `Pinned` joins
/// the union face's group (an authored `as` resolving an ambiguous
/// group), while at a oneof position it is a union-only verdict — the
/// merge diagnoses it Dropped (NML2086) and the oneof face excludes it,
/// so a tampered layer diagnosed "not composed" is not composed.
#[derive(Clone, Copy)]
pub(in crate::layers) enum Face {
    OneOf,
    Union,
}

/// The survivorship rule, once, with the face as a parameter rather
/// than a second function (four prose-tied copies of this rule were the
/// disease it replaced): which positions of a trace SURVIVE its
/// decisions — a join keeps, a pin keeps on the union face only, a
/// switch restarts the group with itself, a rejection or a discard
/// contributes nothing.
pub(in crate::layers) fn surviving_indexes(trace: &[Decision<'_>], face: Face) -> Vec<usize> {
    let mut group: Vec<usize> = Vec::new();
    for (i, (_, d)) in trace.iter().enumerate() {
        match d {
            ArmDecision::Join => group.push(i),
            ArmDecision::Pinned => {
                if matches!(face, Face::Union) {
                    group.push(i);
                }
            }
            ArmDecision::Switch => {
                group.clear();
                group.push(i);
            }
            ArmDecision::Rejected { .. } | ArmDecision::Discarded { .. } => {}
        }
    }
    group
}

/// THE arm-decision authority: the accumulator fold, seal backstop
/// included, producing both the final effective arm and the full
/// per-layer decision trace the merge replays. Seal judgment runs over
/// the displaced group normalized UNDER THE DISPLACED ARM
/// ([`normalize_for_scan`]) — the exact question is "would the displaced
/// composed value carry an assigned seal?", and any other representation
/// answers a different question: raw bodies miss `.shared`-distributed
/// and positional writes; bodies normalized under the SURVIVING arm
/// carry that arm's machinery injections as false positives.
pub(in crate::layers) fn fold_arm_checked<'a>(
    index: &SchemaIndex,
    oneof: &OneOfDef,
    bodies: &[(InstanceId<'a>, &Body)],
) -> (Option<String>, DecisionTrace<'a>) {
    let mut effective = oneof.default_discriminator.clone();
    let mut group: Vec<(InstanceId<'a>, &Body)> = Vec::new();
    let mut trace: DecisionTrace<'a> = Vec::new();
    for (id, b) in bodies {
        let stated = stated_discriminator(b, &oneof.discriminator);
        match stated {
            Some(v) if Some(&v) != effective.as_ref() => {
                let displaced = effective
                    .as_ref()
                    .and_then(|d| variant_model_of(index, oneof, d));
                let seals = match displaced {
                    Some(dm) => displaced_group_seals(index, &[dm], &group),
                    None => Vec::new(),
                };
                if !seals.is_empty() {
                    trace.push((*id, ArmDecision::Rejected { seals }));
                    continue;
                }
                effective = Some(v);
                group.clear();
                group.push((*id, b));
                trace.push((*id, ArmDecision::Switch));
            }
            _ => {
                group.push((*id, b));
                trace.push((*id, ArmDecision::Join));
            }
        }
    }
    (effective, trace)
}

/// The stated (authored, VALID) `as` variant of a union body — an
/// annotation naming no variant reads as un-annotated (a bogus name must
/// not switch anything; `merge_union` reports it as NML2051, since the
/// composed view replaces the annotation before the validator can see
/// the authored one).
pub(in crate::layers) fn stated_variant(
    index: &SchemaIndex,
    union_ty: &FieldType,
    body: &Body,
) -> Option<String> {
    let variants = union_ty.union_variants()?;
    let name = body.type_annotation.as_ref()?.name.as_str();
    index
        .select_variant_by_type_name(variants, name)
        .map(|_| name.to_string())
}

/// A union position's establishment: what the lowest supplying layer
/// made the position (RFC 0019: "the lowest layer that supplies the
/// value establishes the variant — its `as` annotation, else its
/// shape"). Structural variants (scalar/list — deliberately not
/// nameable, RFC 0015) establish too, PER SHAPE: a valid whole-value
/// spelling IS a supply, and one collapsed "structural" bucket let a
/// scalar↔list cross replace values by spelling instead of layer order.
/// Ambiguity is fail-closed end to end: where the D2 oracle refuses to
/// pick a variant, compose refuses too — it never guesses one and never
/// synthesizes an annotation that would blind the validator's NML2052.
/// Doubles as "what a supply WOULD have established" — the second half
/// of a recorded discard.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::layers) enum Establishment {
    /// A nameable (model/oneof) variant; `synthesized` marks
    /// shape-inferred establishment — the output annotation is
    /// synthesized rather than cloned from the author (a pin is
    /// authored: the pinning layer's identifier is carried).
    Named { variant: String, synthesized: bool },
    /// An un-annotated keyed/bare body the D2 oracle calls ambiguous
    /// (`candidates` = the oracle's set, source order): the group
    /// deep-merges MODEL-LESS and the output carries NO annotation, so
    /// the validator's fail-closed NML2052 fires on the composed view
    /// exactly as it would on the raw one. An authored `as` above it
    /// PINS the group — it resolves the ambiguity rather than switching
    /// away from a variant that was never chosen.
    Ambiguous { candidates: Vec<String> },
    /// The scalar structural variant (a whole-value spelling).
    Value,
    /// The list structural variant (items, in any spelling). Its body
    /// IS scannable — items carry sealed assignments at any depth,
    /// through positional tokens and list-level `.shared` writes too —
    /// so a switch away from this establishment is judged like every
    /// other displacement, over the displaced LIST body (the bare-list
    /// winner: the list the displaced compose would actually carry).
    Items,
}

impl Establishment {
    /// The predicate diagnostics complete: "'{path}' is established
    /// {clause} {relation}".
    pub(in crate::layers) fn clause(&self) -> String {
        match self {
            Establishment::Named { variant, .. } => format!("`as {variant}`"),
            Establishment::Ambiguous { candidates } => format!(
                "as an un-annotated body (ambiguous between {})",
                candidates.join(" | ")
            ),
            Establishment::Value => "as a scalar value".to_string(),
            Establishment::Items => "as a list value".to_string(),
        }
    }

    /// The seal judgment a switch away from this establishment must
    /// pass — "what would the displaced compose carry?" under the
    /// DISPLACED vocabulary: a named group under its variant, a list
    /// group over its list body under the list variant's element type,
    /// a scalar trivially (nothing to scan). An ambiguous group is never
    /// switched away from (an authored `as` pins it — see
    /// [`union_verdict`]); the arm is kept fail-closed over every oracle
    /// candidate should that ever change.
    fn displaced_seals<'a>(
        &self,
        index: &SchemaIndex,
        union_ty: &FieldType,
        group: &[(InstanceId<'a>, &Body)],
    ) -> SealHits<'a> {
        match self {
            Establishment::Named { variant, .. } => {
                let vocab = union_variant_vocab(index, variant, group);
                displaced_group_seals(index, &vocab, group)
            }
            // Unreachable by the rule table ((Ambiguous, Authored) is a
            // Pin, never a switch — pinned by
            // `union_verdict_table_enumerates_every_cell`); kept
            // fail-CLOSED over every candidate rather than panicking.
            Establishment::Ambiguous { candidates } => {
                let mut vocab: Vec<&ModelDef> = Vec::new();
                for name in candidates {
                    for m in union_variant_vocab(index, name, group) {
                        push_model(&mut vocab, m);
                    }
                }
                displaced_group_seals(index, &vocab, group)
            }
            Establishment::Items => displaced_list_seals(index, union_ty, group),
            Establishment::Value => Vec::new(),
        }
    }
}

/// What one layer supplies at a union-typed position, classified by
/// [`UnionSupply::classify`]: a nameable variant (authored `as`, or
/// unambiguously shape-inferred), an oracle-ambiguous body, a list value
/// (items in any spelling — the body is the LIST body, synthesized for
/// non-block spellings so it is scannable), a zero-item entry (never
/// supplies, never establishes: NML2079's contract), or a scalar whole
/// value. ONE constructor ([`union_supplies`]) serves every fold site,
/// so a trace is always folded over exactly the supply set the merge
/// composes — folding one set and replaying over another is how a
/// switch silently sticks or a refusal gets fabricated.
pub(in crate::layers) enum UnionSupply<'b> {
    Authored {
        variant: String,
        body: Cow<'b, Body>,
    },
    Inferred {
        variant: String,
        body: Cow<'b, Body>,
    },
    Ambiguous {
        candidates: Vec<String>,
        body: Cow<'b, Body>,
    },
    Items {
        body: Cow<'b, Body>,
    },
    Empty,
    Value,
}

impl<'b> UnionSupply<'b> {
    /// Classify a nested (or synthesized list) body: authored `as`
    /// (valid names only — a bogus name never switches; the merge
    /// reports it as NML2051); the zero-item entry (no entries of its
    /// OWN — `.shared` lines are distributed, not owned — under a union
    /// that admits items: a warned no-op, not an empty object, and the
    /// same verdict raw or normalized); an oracle-ambiguous body
    /// (compose refuses to guess exactly where D2 does); a
    /// shape-selected nameable variant via the ONE resolver; else the
    /// structural shape it spells.
    pub(in crate::layers) fn classify(
        index: &SchemaIndex,
        union_ty: &FieldType,
        body: Cow<'b, Body>,
    ) -> Self {
        if let Some(variant) = stated_variant(index, union_ty, &body) {
            return UnionSupply::Authored { variant, body };
        }
        if zero_item_body_at(union_ty, &body) {
            return UnionSupply::Empty;
        }
        if let Some(vs) = union_ty.union_variants() {
            if let Some(cands) = index.ambiguous_union_variants(vs, &body) {
                return UnionSupply::Ambiguous {
                    candidates: cands.iter().map(|c| c.name().to_string()).collect(),
                    body,
                };
            }
        }
        if let Some(variant) = inferred_variant(index, union_ty, &body) {
            return UnionSupply::Inferred { variant, body };
        }
        if BodyShape::of(&body).has_list_items {
            UnionSupply::Items { body }
        } else {
            UnionSupply::Value
        }
    }

    /// Classify one ENTRY of any spelling: a nested block by its body; an
    /// item-bearing spelling (inline array, modifier block or array) by a
    /// synthesized list body — only where the union admits items (else
    /// an array is just an invalid whole value, never a phantom empty
    /// object); anything else a scalar whole value.
    fn classify_entry(index: &SchemaIndex, union_ty: &FieldType, kind: &'b BodyEntryKind) -> Self {
        match kind {
            BodyEntryKind::NestedBlock(nb) => {
                UnionSupply::classify(index, union_ty, Cow::Borrowed(&nb.body))
            }
            kind => match items_of(kind) {
                Some(items) if admits_items(union_ty) => {
                    let list = Body::fresh(
                        items
                            .into_iter()
                            .map(|item| BodyEntry {
                                span: item.span,
                                kind: BodyEntryKind::ListItem(item),
                            })
                            .collect(),
                    );
                    UnionSupply::classify(index, union_ty, Cow::Owned(list))
                }
                _ => UnionSupply::Value,
            },
        }
    }

    pub(in crate::layers) fn body(&self) -> Option<&Body> {
        match self {
            UnionSupply::Authored { body, .. }
            | UnionSupply::Inferred { body, .. }
            | UnionSupply::Ambiguous { body, .. }
            | UnionSupply::Items { body } => Some(body.as_ref()),
            UnionSupply::Empty | UnionSupply::Value => None,
        }
    }

    /// The establishment this supply makes when it is the lowest one
    /// (a zero-item entry makes none) — and, recorded on a discard,
    /// what the losing supply WOULD have established.
    fn establishes(&self) -> Option<Establishment> {
        match self {
            UnionSupply::Authored { variant, .. } => Some(Establishment::Named {
                variant: variant.clone(),
                synthesized: false,
            }),
            UnionSupply::Inferred { variant, .. } => Some(Establishment::Named {
                variant: variant.clone(),
                synthesized: true,
            }),
            UnionSupply::Ambiguous { candidates, .. } => Some(Establishment::Ambiguous {
                candidates: candidates.clone(),
            }),
            UnionSupply::Items { .. } => Some(Establishment::Items),
            UnionSupply::Value => Some(Establishment::Value),
            UnionSupply::Empty => None,
        }
    }

    /// The variant this supply names or unambiguously selects — the one
    /// reading every normalization consumer takes (`None` for ambiguous
    /// and structural supplies: nothing to normalize under, no guess).
    pub(in crate::layers) fn nameable_variant(&self) -> Option<&str> {
        match self {
            UnionSupply::Authored { variant, .. } | UnionSupply::Inferred { variant, .. } => {
                Some(variant)
            }
            _ => None,
        }
    }
}

/// The one supply constructor: every entry a group of sibling bodies
/// holds under `name`, in group order — the exact set the merge folds
/// and composes.
pub(in crate::layers) fn union_supplies<'a, 'b>(
    index: &SchemaIndex,
    union_ty: &FieldType,
    entries: &[(InstanceId<'a>, &'b BodyEntry)],
) -> Vec<(InstanceId<'a>, UnionSupply<'b>)> {
    entries
        .iter()
        .map(|(l, e)| (*l, UnionSupply::classify_entry(index, union_ty, &e.kind)))
        .collect()
}

/// The fold's policy verdict for one supply over the establishment in
/// force — RFC 0019's rules as a table, separated from the group/trace
/// bookkeeping that carries them out.
#[derive(Debug, PartialEq)]
pub(in crate::layers) enum Verdict {
    /// The lowest supplying layer establishes.
    Establish,
    /// Deep-merge / overlay into the establishment (a zero-item entry
    /// always joins as a no-op).
    Join,
    /// An authored `as` over an ambiguous group resolves it to this
    /// variant — never a switch: nothing was chosen to switch from.
    Pin(String),
    /// An authored `as` naming a different variant: a switch, subject
    /// to the seal backstop.
    JudgeSwitch(String),
    /// Can neither merge nor switch: discarded, loudly (NML2085).
    Discard,
}

/// The rule table. Every (establishment, supply) pair is named — no
/// wildcard hides a cell — and the only guard is the restatement check.
pub(in crate::layers) fn union_verdict(
    est: Option<&Establishment>,
    supply: &UnionSupply<'_>,
) -> Verdict {
    use Establishment as E;
    use UnionSupply as S;
    match (est, supply) {
        // A zero-item entry never supplies: it joins as a no-op.
        (_, S::Empty) => Verdict::Join,
        (None, _) => Verdict::Establish,
        // Authored `as`: restatement joins; a different name switches
        // (judged); over an ambiguous group it pins.
        (Some(E::Named { variant, .. }), S::Authored { variant: v, .. }) => {
            if variant == v {
                Verdict::Join
            } else {
                Verdict::JudgeSwitch(v.clone())
            }
        }
        (Some(E::Ambiguous { .. }), S::Authored { variant, .. }) => Verdict::Pin(variant.clone()),
        (Some(E::Value | E::Items), S::Authored { variant, .. }) => {
            Verdict::JudgeSwitch(variant.clone())
        }
        // An un-annotated body never switches: over a named or ambiguous
        // establishment it deep-merges (mis-shape is the validator's
        // business); over a structural value it cannot merge.
        (Some(E::Named { .. } | E::Ambiguous { .. }), S::Inferred { .. } | S::Ambiguous { .. }) => {
            Verdict::Join
        }
        (Some(E::Value | E::Items), S::Inferred { .. } | S::Ambiguous { .. }) => Verdict::Discard,
        // Whole values: same-shape structural supplies join their own
        // overlay; every cross — over a body establishment, or across
        // the scalar/list boundary — is discarded.
        (Some(E::Value), S::Value) | (Some(E::Items), S::Items { .. }) => Verdict::Join,
        (Some(E::Named { .. } | E::Ambiguous { .. } | E::Items), S::Value)
        | (Some(E::Named { .. } | E::Ambiguous { .. } | E::Value), S::Items { .. }) => {
            Verdict::Discard
        }
    }
}

/// RFC 0025 §6 — the `FOLD_TAMPER` hook's shape: the folded path in,
/// the fold's trace to corrupt.
#[cfg(test)]
type TamperFn = fn(&str, &mut DecisionTrace<'_>);

#[cfg(test)]
thread_local! {
    /// Test-only observability for the judgment memo (a DoS defence has
    /// no behavioral signature): the number of judgments actually
    /// computed rather than reused.
    pub(in crate::layers) static JUDGMENT_MISSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// RFC 0025 §6 — every path a merge-level fold folded, dotted and
    /// bracketed, in fold order: the observable behind "an item group
    /// folds under its own bracketed scope, never the container's".
    pub(in crate::layers) static FOLD_LOG: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    /// RFC 0025 §6 — a take-once corruption hook over a merge-level
    /// fold's trace, path-aware (the first fold executed consumes it):
    /// proves the NML2086 boundary assertion live.
    pub(in crate::layers) static FOLD_TAMPER: std::cell::Cell<Option<TamperFn>> = const { std::cell::Cell::new(None) };
    /// RFC 0025 §6 — every (path, layer name) the loser subtraction
    /// diagnosed under its own readings.
    pub(in crate::layers) static DISCARDS: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// RFC 0025 §6 — the fold seams, applied after every merge-level fold:
/// log the folded path; run the take-once trace corruption when armed.
#[cfg(test)]
pub(in crate::layers) fn fold_seams(path: &str, trace: &mut DecisionTrace<'_>) {
    FOLD_LOG.with(|l| l.borrow_mut().push(path.to_string()));
    if let Some(tamper) = FOLD_TAMPER.take() {
        tamper(path, trace);
    }
}

/// THE variant-decision authority for union-typed positions (RFC 0015),
/// the exact sibling of [`fold_arm_checked`]: the rule table is
/// [`union_verdict`]; this loop only carries verdicts out — group and
/// trace bookkeeping, plus the seal judgment a switch must pass
/// ([`Establishment::displaced_seals`], judged over the displaced group
/// normalized under the DISPLACED vocabulary). Supplies come from the
/// one constructor ([`union_supplies`]); returns the establishment and
/// the per-layer trace the merge replays.
pub(in crate::layers) fn fold_variant_checked<'a, 'b>(
    index: &SchemaIndex,
    union_ty: &FieldType,
    supplies: &[(InstanceId<'a>, UnionSupply<'b>)],
) -> (Option<Establishment>, DecisionTrace<'a>) {
    let mut established: Option<Establishment> = None;
    // The effective group's SCANNABLE bodies, for the displaced-seal
    // judgment: named and ambiguous groups accumulate (they
    // deep-merge); a list group holds ONLY the bare-list winner (a
    // replaced list's seals never engaged — judging them would refuse a
    // switch over items the engine itself discarded); scalar whole
    // values have nothing to scan.
    let mut group: Vec<(InstanceId<'a>, &Body)> = Vec::new();
    // A rejected switch leaves the group untouched, so consecutive
    // judgments over one group are identical — memoized by group
    // version (N rejected switches over M sealed items were N full
    // scans: a quadratic DoS axis).
    let mut group_version: u32 = 0;
    let mut judged: Option<(u32, SealHits<'a>)> = None;
    let mut trace: DecisionTrace<'a> = Vec::new();
    for (id, supply) in supplies {
        match union_verdict(established.as_ref(), supply) {
            Verdict::Establish => {
                established = supply.establishes();
                if let Some(b) = supply.body() {
                    group.push((*id, b));
                    group_version += 1;
                }
                trace.push((*id, ArmDecision::Join));
            }
            Verdict::Join => {
                match (&established, supply.body()) {
                    (
                        Some(Establishment::Named { .. } | Establishment::Ambiguous { .. }),
                        Some(b),
                    ) => {
                        group.push((*id, b));
                        group_version += 1;
                    }
                    (Some(Establishment::Items), Some(b)) => {
                        group.clear();
                        group.push((*id, b));
                        group_version += 1;
                    }
                    _ => {}
                }
                trace.push((*id, ArmDecision::Join));
            }
            Verdict::Pin(variant) => {
                // Authored by the pinning layer: not synthesized.
                established = Some(Establishment::Named {
                    variant,
                    synthesized: false,
                });
                group.push((*id, supply.body().expect("an authored supply is a body")));
                group_version += 1;
                trace.push((*id, ArmDecision::Pinned));
            }
            Verdict::JudgeSwitch(variant) => {
                let est = established
                    .as_ref()
                    .expect("a switch is judged against an establishment");
                let seals = match &judged {
                    Some((version, seals)) if *version == group_version => seals.clone(),
                    _ => {
                        #[cfg(test)]
                        JUDGMENT_MISSES.with(|c| c.set(c.get() + 1));
                        let seals = est.displaced_seals(index, union_ty, &group);
                        judged = Some((group_version, seals.clone()));
                        seals
                    }
                };
                if !seals.is_empty() {
                    trace.push((*id, ArmDecision::Rejected { seals }));
                    continue;
                }
                established = Some(Establishment::Named {
                    variant,
                    synthesized: false,
                });
                group.clear();
                group.push((*id, supply.body().expect("an authored supply is a body")));
                group_version += 1;
                trace.push((*id, ArmDecision::Switch));
            }
            Verdict::Discard => {
                trace.push((
                    *id,
                    ArmDecision::Discarded {
                        over: established
                            .clone()
                            .expect("a discard is judged against an establishment"),
                        lost: supply
                            .establishes()
                            .expect("a zero-item entry always joins"),
                    },
                ));
            }
        }
    }
    (established, trace)
}

/// The plain bottom-up discriminator fold (RFC 0019 step 3): the last
/// layer to state the discriminator wins, else the schema default. The
/// merge-time accumulator (with its seal backstop) remains the authority
/// on the composed VALUE — the fold only picks the normalization
/// vocabulary, and a backstop-rejected switch's layers are discarded at
/// merge time regardless of how they normalized.
pub(in crate::layers) fn fold_arm(oneof: &OneOfDef, bodies: &[&Body]) -> Option<String> {
    bodies
        .iter()
        .rev()
        .find_map(|b| stated_discriminator(b, &oneof.discriminator))
        .or_else(|| oneof.default_discriminator.clone())
}
