//! RFC 0019 — instance layer composition (`uses`) and sealed fields.
//!
//! One merge engine for embedders and `nml check` alike: [`resolve_layers`]
//! linearizes a `uses` stack (C3, in NML's reversed orientation — precedence
//! increases left to right, the mirror of Python's MRO), inlines each
//! layer's array references against its own document, and composes
//! field-by-field under the schema's merge-policy directives (`#sealed` /
//! `#identity` / `#append` / `#overlay`), returning a best-effort
//! [`ResolvedInstance`] plus every diagnostic in one pass. The merge
//! DECIDES at each level over the raw supplies (RFC 0025): survivors
//! normalize under the decided variant through the one normalizer
//! (`normalize_level`), discarded bodies are diagnosed by subtraction
//! under their own readings, and every finding is emitted through a sink
//! ordered by a total key.
//!
//! Authorization is two-grant (RFC 0019 §Authorization): the authoring
//! site's grant governs each clause's own listed refs; the root clause's
//! grant bounds every composed layer. The engine asks a
//! [`LayerGrantProvider`] — grant *matching* (globs, the P1–P4 path
//! pipeline) lives with the provider (nml-validate), keeping the dependency
//! direction clean; this module owns the *decisions*.

use std::collections::{HashMap, HashSet};

use crate::ast::{Body, DeclarationKind, File};
use crate::diagnostic::{Diagnostic, codes};
use crate::diff::Origin;
use crate::schema_index::SchemaIndex;

mod decide;
mod entries;
mod grants;
mod instances;
mod linearize;
mod merge;
mod normalize;
mod policy;
mod seal;

pub use grants::{GrantLookup, LayerGrant, LayerGrantProvider, OpenContext, RefDecision};
pub use instances::{InstanceId, InstanceIndex};
pub use policy::{MergePolicy, policy_of, validate_merge_policies, validate_merge_policies_over};

use grants::*;
use linearize::*;
use merge::*;

/// RFC 0019: the language-level hard cap on distinct instances in one
/// linearized stack (the declaring instance included). Bounds merge work in
/// every context, grants included — the same defensive stance as the
/// parser's `MAX_DEPTH` and the glob matcher's segment cap.
pub const MAX_STACK_DEPTH: u32 = 16;

// ─────────────────────────────────────────────────────────────── grants ──

/// Discovery-only `uses` ref resolution for consumers that do NOT compose
/// (`nml validate`'s "unresolved references" contract): every clause ref
/// must resolve in-file, with the same NML2059 wording (and did-you-mean)
/// the composing path emits — the two verbs can never describe the same
/// defect differently. No linearization, no grants, no merge. Schema
/// definitions are skipped: a `uses` clause there is itself the defect
/// (NML2062, owned by the composing path).
pub fn check_uses_refs(source_path: &str, file: &File) -> Vec<Diagnostic> {
    let instances = InstanceIndex::from_file(source_path, file);
    let mut out = Vec::new();
    for decl in &file.declarations {
        let DeclarationKind::Block(block) = &decl.kind else {
            continue;
        };
        if crate::symbols::is_schema_keyword(&block.keyword.name) {
            // A schema definition's clause is itself the defect —
            // definition-intrinsic, so `validate` owns it too (same
            // NML2062 wording owner as the composing path).
            if !block.uses.is_empty() {
                out.push(schema_def_uses_denial(block, source_path));
            }
            continue;
        }
        for r in &block.uses {
            if instances.resolve_ref(&r.name).is_none() {
                out.push(
                    unresolved_ref(&instances, &block.name.name, &block.keyword.name, &r.name)
                        .with_span(r.span)
                        .with_source(source_path.to_string()),
                );
            }
        }
    }
    out
}

/// Field path → origin: which layer's assignment produced each effective
/// entry. List items key by the identity pair (kind, token). Consumed
/// today by the oracle dump (`nml check --dump-compose`, via
/// `ComposedFile::origins`); the RFC 0019 verbs (`nml resolve
/// --provenance`) are its next consumers.
pub type ProvenanceTable = Vec<(String, Origin)>;

#[derive(Debug)]
pub struct ResolvedInstance {
    /// Invariant: exactly one VALUE entry per field name — replace in
    /// place at the base slot (the entry itself carries the head's span
    /// and name, RFC 0019 E15), never append-and-shadow. Validator-
    /// facing passthroughs sit beside it: a declaration (a type-
    /// annotation modifier) ahead of its value, and non-string
    /// discriminator entries after the canonical one (first, when none
    /// exists) — every passthrough of the second kind accompanied by an
    /// error-severity finding, so no artifact of record may be derived
    /// from this body while an error-severity finding exists (E16). One
    /// carve-out: validator DEPTH TRUNCATION (NML2044, a warning) stops
    /// checking before the discriminator does — unreachable through the
    /// parser (its nesting fence errors first) but reachable from a
    /// constructed AST, so an artifact-of-record gate must treat
    /// NML2044 as blocking too.
    pub body: Body,
    pub origins: ProvenanceTable,
}

/// Compose a stack (RFC 0019 §Resolution pipeline). `refs` are the
/// declaring clause's LISTED refs in authored order — linearization is this
/// function's job, not the caller's. Returns the pair: best-effort resolved
/// instance plus every diagnostic in one pass. Step 1–2 failures yield
/// `None` (fail closed); step 3–4 policy violations yield diagnostics PLUS
/// a best-effort instance with the offending contribution skipped.
pub fn resolve_layers(
    index: &SchemaIndex,
    instances: &InstanceIndex<'_>,
    declaring: InstanceId<'_>,
    root: &str,
    refs: &[InstanceId<'_>],
    local: &Body,
    grants: &dyn LayerGrantProvider,
) -> (Option<ResolvedInstance>, Vec<Diagnostic>) {
    let mut diags = Vec::new();
    let Some(declaring_block) = instances.get(declaring) else {
        diags.push(
            Diagnostic::error(format!(
                "declaring instance '{}' is not in the index",
                declaring.name
            ))
            .with_code(codes::UNRESOLVED_LAYER_REF)
            .with_source(declaring.source_path.to_string()),
        );
        return (None, diags);
    };

    // Steps 1–2: authorize, load, linearize, authorize the stack.
    let mut lin = Linearizer {
        instances,
        grants,
        declaring_keyword: &declaring_block.keyword.name,
        diags: Vec::new(),
        memo: HashMap::new(),
        in_progress: Vec::new(),
        depth_reported: false,
    };
    // The declaring clause's own site check + listed-ref resolution.
    let site = grants.grant_for(declaring.source_path);
    if let Some(d) = deny_diagnostic(&site, declaring, declaring_block) {
        diags.push(d);
        return (None, diags);
    }
    // Site-check the declaring clause's listed refs against the root grant.
    let mut site_ok = true;
    if let GrantLookup::Granted {
        grant,
        binding,
        manifest,
    } = &site
    {
        let gref = GrantRef {
            binding,
            manifest,
            file: declaring.source_path,
        };
        for r in refs {
            let decision = grants.ref_decision(grant, r.source_path);
            if let Some(d) = ref_denial(decision, r.name, &gref, Denial::Site) {
                // Anchor at the denied ref's own token — the same anchor
                // transitive-clause denials use — not the block name.
                let span = declaring_block
                    .uses
                    .iter()
                    .find(|u| u.name == r.name)
                    .map(|u| u.span)
                    .unwrap_or(declaring_block.name.span);
                diags.push(
                    d.with_span(span)
                        .with_source(declaring.source_path.to_string()),
                );
                site_ok = false;
            }
        }
    }
    // Pre-warm every ref's linearization so ONE pass reports every
    // discovery failure (memoized — the merge below re-uses the results),
    // then run the shared listed-refs merge core: the declaring clause and
    // every transitive clause linearize through the same implementation.
    for r in refs {
        lin.linearize(*r);
    }
    let merged = lin.merge_listed(declaring, declaring_block.name.span, refs);
    diags.append(&mut lin.diags);
    if !site_ok {
        return (None, diags);
    }
    let Some(merged) = merged else {
        return (None, diags);
    };
    // Bottom-up compose order: reversed precedence order, declaring last.
    let mut stack: Vec<InstanceId<'_>> = merged.into_iter().rev().collect();
    stack.push(declaring);

    // Depth: distinct instances in the linearized stack, declaring included.
    let depth = stack.len() as u32;
    let grant_cap = match &site {
        GrantLookup::Granted { grant, .. } => grant.max_stack_depth,
        _ => None,
    };
    if let Some(cap) = grant_cap {
        if depth > cap {
            diags.push(
                layer_bound_exceeded(LayerBound::Grant { depth, cap })
                    .with_span(declaring_block.name.span)
                    .with_source(declaring.source_path.to_string()),
            );
            return (None, diags);
        }
    }
    if depth > MAX_STACK_DEPTH {
        diags.push(
            layer_bound_exceeded(LayerBound::Language { depth })
                .with_span(declaring_block.name.span)
                .with_source(declaring.source_path.to_string()),
        );
        return (None, diags);
    }
    // Stack-level authorization: the root grant bounds every composed
    // layer (the declaring instance excepted).
    if let GrantLookup::Granted {
        grant,
        binding,
        manifest,
    } = &site
    {
        let gref = GrantRef {
            binding,
            manifest,
            file: declaring.source_path,
        };
        let mut stack_ok = true;
        for layer in stack.iter().filter(|l| **l != declaring) {
            let decision = grants.ref_decision(grant, layer.source_path);
            // The entering ref: the root clause's listed ref whose own
            // stack pulls this layer in — named in the message, and its
            // span (in the checked file) anchors the diagnostic.
            let entering = refs.iter().find(|r| {
                **r == *layer
                    || lin
                        .memo
                        .get(*r)
                        .and_then(|o| o.as_ref())
                        .is_some_and(|v| v.contains(layer))
            });
            let span = entering
                .and_then(|e| {
                    declaring_block
                        .uses
                        .iter()
                        .find(|u| u.name == e.name)
                        .map(|u| u.span)
                })
                .unwrap_or(declaring_block.name.span);
            if let Some(d) = ref_denial(
                decision,
                layer.name,
                &gref,
                Denial::Stack {
                    entering: entering.map(|e| e.name),
                },
            ) {
                diags.push(
                    d.with_span(span)
                        .with_source(declaring.source_path.to_string()),
                );
                stack_ok = false;
            }
        }
        if !stack_ok {
            return (None, diags);
        }
    }

    // Step 3: array-reference inlining, per layer, against that layer's
    // own document (inlining is arm-independent and can itself introduce
    // a discriminator). Everything after is merge-time (RFC 0025;
    // RFC 0019 erratum E18): each position decides by folding its raw
    // inlined supplies, survivors normalize under the decided variant,
    // and discarded bodies are diagnosed under their own readings.
    let doc = instances.document();
    let inlined: Vec<(InstanceId<'_>, Body)> = stack
        .iter()
        .map(|id| {
            let body = if *id == declaring {
                local
            } else {
                &instances
                    .get(*id)
                    .expect("linearized layer is indexed")
                    .body
            };
            (
                *id,
                crate::defaults::inline_layer_array_references(&doc, body),
            )
        })
        .collect();
    // The order contract (RFC 0025 §5): every compose-time finding —
    // per-level normalization and merge alike — is stamped with its
    // offending layer's stack position and sorted on a total key, so
    // emission order is irrelevant. Pre-sink findings (linearization,
    // grants) failed closed above, before the sink exists.
    let mut sink = ComposeSink::new(stack.clone());
    // Step 4: the merge decides (RFC 0025 §2).
    let mut merger = Merger {
        index,
        sink: &mut sink,
        origins: Vec::new(),
    };
    let body = merger.compose_root(root, &inlined);
    let origins = merger.origins;
    diags.extend(sink.finish());
    // A reached internal invariant is a test failure in debug builds —
    // asserted at the boundary, not at the site, so the diagnostic and
    // the fail-safe composition it describes are observable in every
    // build (the editor's guard catches this unwind too).
    debug_assert!(
        !diags
            .iter()
            .any(|d| d.code == Some(codes::INTERNAL_COMPOSE_INVARIANT)),
        "internal composition invariant violated: {:?}",
        diags
            .iter()
            .filter(|d| d.code == Some(codes::INTERNAL_COMPOSE_INVARIANT))
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>()
    );
    (Some(ResolvedInstance { body, origins }), diags)
}

/// The one-home deduplication key: identical (code, span, message) triples
/// are the same finding wherever re-encountered — a base defect cloned into
/// every overlay's resolved body, a shared sub-stack's violation seen by
/// every descendant's compose. One vocabulary for every deduplicating
/// consumer (this module's orchestration, the CLI's validator loop, the
/// LSP's diagnostics pass to come).
pub type FindingKey = (
    Option<crate::diagnostic::Code>,
    Option<(usize, usize)>,
    String,
);

/// The [`FindingKey`] of one diagnostic.
pub fn finding_key(diag: &Diagnostic) -> FindingKey {
    (
        diag.code,
        diag.span.map(|sp| (sp.start, sp.end)),
        diag.message.clone(),
    )
}

/// A whole file composed: the validation view plus every composition
/// diagnostic, deduplicated to one home per finding.
pub struct ComposedFile {
    /// The file with each composing block's body replaced by its resolved
    /// body and failed composes removed — what schema validation should see
    /// (an overlay alone is deliberately partial; a failed compose already
    /// reported its own findings). `None` when no block carries `uses`.
    pub validation_file: Option<File>,
    pub diagnostics: Vec<Diagnostic>,
    /// Each successfully composed declaration's provenance table, keyed
    /// by its declaration index (RFC 0025 Phase 1 — the oracle dumps
    /// them; `resolve_layers` always computed them and this surface
    /// used to drop them).
    pub origins: Vec<(usize, ProvenanceTable)>,
}

/// Compose every `uses`-carrying block in a file — the one orchestration
/// every front-end shares (`nml check` today, the LSP's diagnostics pass
/// next), so the editor and the CI gate can never disagree about the same
/// buffer. Owns: the schema-definition gate (NML2062), declaring-clause ref
/// resolution with its did-you-mean (NML2059), per-block `resolve_layers`,
/// one-home deduplication (a shared sub-stack's violation is re-encountered
/// by every descendant's compose; identical findings report once), and the
/// validation-view substitution.
pub fn compose_file(
    index: &SchemaIndex,
    source_path: &str,
    file: &File,
    grants: &dyn LayerGrantProvider,
) -> ComposedFile {
    let instances = InstanceIndex::from_file(source_path, file);
    let mut diagnostics = Vec::new();
    let mut seen: HashSet<FindingKey> = HashSet::new();
    let mut composed: Vec<(usize, Body)> = Vec::new();
    let mut origins: Vec<(usize, ProvenanceTable)> = Vec::new();
    let mut failed: Vec<usize> = Vec::new();
    let mut any_uses = false;

    for (decl_idx, decl) in file.declarations.iter().enumerate() {
        let crate::ast::DeclarationKind::Block(block) = &decl.kind else {
            continue;
        };
        if block.uses.is_empty() {
            continue;
        }
        any_uses = true;
        if crate::symbols::is_schema_keyword(&block.keyword.name) {
            let d = schema_def_uses_denial(block, source_path);
            if seen.insert(finding_key(&d)) {
                diagnostics.push(d);
            }
            // NOT pushed to `failed`: a definition stays in the validation
            // view (its definition-side findings and `is`-target resolution
            // must survive); only failed INSTANCE composes are removed,
            // because their partial bodies would cascade.
            continue;
        }
        let declaring = InstanceId {
            source_path,
            name: &block.name.name,
        };
        let mut refs = Vec::new();
        let mut refs_ok = true;
        for r in &block.uses {
            match instances.resolve_ref(&r.name) {
                Some(id) => refs.push(id),
                None => {
                    // Through `seen` like every other finding — a direct
                    // push bypassing the one-home dedup re-reports the
                    // same defect when a DEPENDENT block's compose
                    // re-encounters this clause.
                    let d =
                        unresolved_ref(&instances, &block.name.name, &block.keyword.name, &r.name)
                            .with_span(r.span)
                            .with_source(source_path.to_string());
                    if seen.insert(finding_key(&d)) {
                        diagnostics.push(d);
                    }
                    refs_ok = false;
                }
            }
        }
        if !refs_ok {
            failed.push(decl_idx);
            continue;
        }
        let (resolved, diags) = resolve_layers(
            index,
            &instances,
            declaring,
            &block.keyword.name,
            &refs,
            &block.body,
            grants,
        );
        for diag in diags {
            if seen.insert(finding_key(&diag)) {
                diagnostics.push(diag);
            }
        }
        match resolved {
            Some(r) => {
                origins.push((decl_idx, r.origins));
                composed.push((decl_idx, r.body));
            }
            None => failed.push(decl_idx),
        }
    }

    let validation_file = any_uses.then(|| {
        let mut f = file.clone();
        for (idx, body) in composed {
            if let crate::ast::DeclarationKind::Block(b) = &mut f.declarations[idx].kind {
                b.body = body;
            }
        }
        failed.sort_unstable();
        for idx in failed.into_iter().rev() {
            f.declarations.remove(idx);
        }
        f
    });

    ComposedFile {
        validation_file,
        diagnostics,
        origins,
    }
}

// ───────────────────────────────────────────────────────────── merging ──

/// Compose-time findings, each stamped with the stack position of the
/// layer it was found in — a TOTAL key, so emission order is irrelevant
/// (RFC 0025 §5, the order contract). Emitters stamp the OFFENDING
/// contribution's layer — every site already holds it and already sets
/// `with_source`; `emit` never re-sources. Linearization and grant
/// findings never enter the sink: they fail closed and `resolve_layers`
/// returns before the merge — and so before the sink — exists.
pub(crate) struct ComposeSink<'a> {
    stack: Vec<InstanceId<'a>>,
    items: Vec<(usize, Diagnostic)>,
}

impl<'a> ComposeSink<'a> {
    fn new(stack: Vec<InstanceId<'a>>) -> Self {
        ComposeSink {
            stack,
            items: Vec::new(),
        }
    }

    /// Stamp `d` with the stack position of the offending `layer`. A
    /// layer outside the stack (tamper-only; no reachable emitter) sorts
    /// after every real position, fail-open for display, debug-asserted.
    fn emit(&mut self, layer: InstanceId<'a>, d: Diagnostic) {
        let position = self.stack.iter().position(|id| *id == layer);
        debug_assert!(
            position.is_some(),
            "compose finding stamped with a layer outside the stack"
        );
        self.items.push((position.unwrap_or(self.stack.len()), d));
    }

    /// The findings sorted by `(position, source, span, code, message)`
    /// — stack order first (the cross-layer contract the seven order
    /// pins assert), then a total key within a layer. The sort is
    /// stable, so residual full-key ties keep emission order.
    fn finish(self) -> Vec<Diagnostic> {
        let mut items = self.items;
        items.sort_by_cached_key(|(position, d)| {
            (
                *position,
                d.source.clone(),
                d.span.map(|sp| (sp.start, sp.end)),
                d.code.map(|c| c.to_string()),
                d.message.clone(),
            )
        });
        items.into_iter().map(|(_, d)| d).collect()
    }
}

#[cfg(test)]
mod tests;
