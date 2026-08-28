//! RFC 0019 — instance layer composition (`uses`) and sealed fields.
//!
//! One merge engine for embedders and `nml check` alike: [`resolve_layers`]
//! linearizes a `uses` stack (C3, in NML's reversed orientation — precedence
//! increases left to right, the mirror of Python's MRO), normalizes each
//! layer in the shipped pipeline order, composes field-by-field under the
//! schema's merge-policy directives (`#sealed` / `#identity` / `#append` /
//! `#overlay`), and returns a best-effort [`ResolvedInstance`] plus every
//! diagnostic in one pass.
//!
//! Authorization is two-grant (RFC 0019 §Authorization): the authoring
//! site's grant governs each clause's own listed refs; the root clause's
//! grant bounds every composed layer. The engine asks a
//! [`LayerGrantProvider`] — grant *matching* (globs, the P1–P4 path
//! pipeline) lives with the provider (nml-validate), keeping the dependency
//! direction clean; this module owns the *decisions*.

use std::collections::{HashMap, HashSet};

use crate::ast::{
    BlockDecl, Body, BodyEntry, BodyEntryKind, DeclarationKind, File, Identifier, ListItem,
    ListItemKind, Modifier, ModifierValue, NestedBlock,
};
use crate::diagnostic::{Diagnostic, codes};
use crate::diff::Origin;
use crate::model::{FieldDef, FieldType, ModelDef, OneOfDef};
use crate::query::Document;
use crate::schema_index::SchemaIndex;
use crate::span::Span;
use crate::types::{SpannedValue, Value};

/// RFC 0019: the language-level hard cap on distinct instances in one
/// linearized stack (the declaring instance included). Bounds merge work in
/// every context, grants included — the same defensive stance as the
/// parser's `MAX_DEPTH` and the glob matcher's segment cap.
pub const MAX_STACK_DEPTH: u32 = 16;

// ─────────────────────────────────────────────────────────────── grants ──

/// A binding's composition grant. Attached to a validator binding; a
/// binding without one denies composition ([`GrantLookup::NoGrant`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerGrant {
    /// Target allowlist: globs over the referenced instance's defining
    /// file path; empty = deny all.
    pub allow_refs: Vec<String>,
    /// Deny wins over allow (NML2065 deny-veto, named by index).
    pub deny_refs: Vec<String>,
    /// Maximum distinct instances in one linearized stack, the declaring
    /// instance included (NML2066). `None` = no grant-level cap; the
    /// language hard cap still applies.
    pub max_stack_depth: Option<u32>,
}

/// The result of looking up the grant governing a file. A state, not an
/// `Option`: NML2064's three message forms need the binding and manifest
/// names, the ambiguous claimants, or the unclaiming root.
#[derive(Debug, Clone)]
pub enum GrantLookup<'a> {
    /// A binding governs the file and carries a grant.
    Granted {
        grant: &'a LayerGrant,
        binding: &'a str,
        manifest: &'a str,
    },
    /// A binding governs the file and carries no `layers:` grant.
    NoGrant { binding: &'a str, manifest: &'a str },
    /// Two or more manifests claim the file — denied, naming all claimants.
    Ambiguous { manifests: Vec<&'a str> },
    /// No binding governs the file; the context default applies (closed
    /// universe → denied, open developer context → permissive).
    Unbound { open_context: bool },
}

/// How a grant judged one referenced path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefDecision {
    Allowed,
    /// A `denyRefs` entry vetoed the path; the index names the rule
    /// (grant rules are unnamed strings — the index is the only stable
    /// referent, and `nml binding` prints the same indices).
    DenyVeto(usize),
    /// No `allowRefs` entry admits the path — the dominant denial mode
    /// ("empty allowlist means deny all"). No rule index exists.
    AllowMiss,
}

/// Answers the engine's two authorization questions. Matching (globs, path
/// canonicalization) is the provider's concern — nml-validate implements it
/// over its glob matcher and P1–P4 pipeline; tests implement it literally.
pub trait LayerGrantProvider {
    /// The grant state governing `source_path` (canonical workspace-relative).
    fn grant_for(&self, source_path: &str) -> GrantLookup<'_>;
    /// Evaluate one referenced defining path against a grant's rules.
    fn ref_decision(&self, grant: &LayerGrant, target_path: &str) -> RefDecision;
}

/// The open developer context: no manifest governs anything, composition is
/// permitted everywhere (RFC 0019's context default for a repo with no
/// binding). Closed universes get their provider from binding resolution.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenContext;

impl LayerGrantProvider for OpenContext {
    fn grant_for(&self, _source_path: &str) -> GrantLookup<'_> {
        GrantLookup::Unbound { open_context: true }
    }
    fn ref_decision(&self, _grant: &LayerGrant, _target_path: &str) -> RefDecision {
        RefDecision::Allowed
    }
}

// ─────────────────────────────────────────────────────── instance index ──

/// Identity of a composed instance: defining file + declaration name.
/// Names are file-scoped (RFC 0020), so diamonds dedupe by this pair,
/// never by name alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstanceId<'a> {
    /// Canonical workspace-relative defining path.
    pub source_path: &'a str,
    pub name: &'a str,
}

/// Resolves an [`InstanceId`] to its declaring block and defining document.
/// This slice indexes one file (same-file composition — RFC 0020 imports
/// extend it across files); the API is already id-keyed so the extension is
/// additive.
pub struct InstanceIndex<'a> {
    source_path: &'a str,
    file: &'a File,
    by_name: HashMap<&'a str, &'a BlockDecl>,
}

impl<'a> InstanceIndex<'a> {
    pub fn from_file(source_path: &'a str, file: &'a File) -> Self {
        let mut by_name: HashMap<&str, &BlockDecl> = HashMap::new();
        for decl in &file.declarations {
            if let crate::ast::DeclarationKind::Block(b) = &decl.kind {
                if !crate::symbols::is_schema_keyword(&b.keyword.name) {
                    // First-wins on duplicate names — the documented
                    // convention everywhere (SchemaIndex, PolicyCtx), and
                    // the duplicate itself is NML2009's business. A plain
                    // `insert` would silently make the LAST duplicate the
                    // one every ref composes against.
                    by_name.entry(b.name.name.as_str()).or_insert(b);
                }
            }
        }
        Self {
            source_path,
            file,
            by_name,
        }
    }

    pub fn get(&self, id: InstanceId<'_>) -> Option<&'a BlockDecl> {
        (id.source_path == self.source_path)
            .then(|| self.by_name.get(id.name).copied())
            .flatten()
    }

    /// Resolve a bare `uses` ref through this file's scope (same-file names;
    /// RFC 0020 import bindings join here).
    pub fn resolve_ref(&self, name: &str) -> Option<InstanceId<'a>> {
        self.by_name.get(name).map(|b| InstanceId {
            source_path: self.source_path,
            name: &b.name.name,
        })
    }

    /// In-scope instance names (did-you-mean candidates for NML2059).
    pub fn names(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.by_name.keys().copied()
    }

    /// The layer's own document — RFC 0013 array refs are file-local, so
    /// step-3 inlining goes through this, never the composing file's.
    pub fn document(&self) -> Document<'a> {
        Document::new(self.file)
    }
}

// ─────────────────────────────────────────────────────── merge policies ──

/// The effective merge policy of one schema field (RFC 0019). Exactly one
/// per field, with the one sanctioned pair; NML2068 rejects everything else
/// at schema load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergePolicy {
    #[default]
    Overlay,
    Sealed,
    Identity,
    Append,
    IdentityAppend,
}

const POLICY_NAMES: [&str; 4] = ["sealed", "identity", "append", "overlay"];

/// Derive a field's merge policy from its trailing directives. Invalid
/// combinations are rejected at schema load (NML2068) — and the engine is
/// additionally fail-closed: `#sealed` in ANY combination composes as
/// `Sealed`, so an embedder that skips the lint pass can never lose a seal
/// to a schema typo (Overlay is the widest write grant; a broken schema
/// must narrow, never widen).
pub fn policy_of(field: &FieldDef) -> MergePolicy {
    let mut sealed = false;
    let mut identity = false;
    let mut append = false;
    for d in &field.directives {
        match d.name.as_str() {
            "sealed" => sealed = true,
            "identity" => identity = true,
            "append" => append = true,
            _ => {}
        }
    }
    match (sealed, identity, append) {
        (true, _, _) => MergePolicy::Sealed,
        (false, true, false) => MergePolicy::Identity,
        (false, false, true) => MergePolicy::Append,
        (false, true, true) => MergePolicy::IdentityAppend,
        (false, false, false) => MergePolicy::Overlay,
    }
}

fn is_list_like(ty: &FieldType) -> bool {
    matches!(ty, FieldType::List(_) | FieldType::Set(_))
}

fn is_set(ty: &FieldType) -> bool {
    matches!(ty, FieldType::Set(_))
}

/// Schema-load validation of merge-policy declarations: NML2068 (incoherent
/// combinations, list policies on non-collections, `#identity` with no
/// mergeable identity) and the three NML2076 seal-reachability lints.
pub fn validate_merge_policies(index: &SchemaIndex) -> Vec<Diagnostic> {
    validate_merge_policies_over(index.models(), index.oneofs())
}

/// Slice-borrowing variant for the schema LOADER — the single owner of
/// policy validation (every consumer of `load_schema`, CLI and LSP and
/// embedders alike, inherits NML2068/NML2076 with per-source attribution;
/// no verb can forget the call). Borrowed lookups only: builds no index
/// and clones no definitions — this runs on every load.
pub fn validate_merge_policies_over(models: &[ModelDef], oneofs: &[OneOfDef]) -> Vec<Diagnostic> {
    let ctx = PolicyCtx::new(models, oneofs);
    let mut diags = Vec::new();
    for model in models {
        for field in &model.fields {
            validate_field_policy(&ctx, model, field, &mut diags);
        }
    }
    for oneof in oneofs {
        // One schema defect, one warning: a oneof referenced by a field
        // already lints at each field site (the more precise span); the
        // declaration-site form covers only oneofs usable solely as
        // instance roots.
        let field_referenced = models.iter().any(|m| {
            m.fields.iter().any(|f| {
                matches!(effective_type(&f.field_type), FieldType::ModelRef(n) if *n == oneof.name)
            })
        });
        if !field_referenced {
            lint_oneof_seals(&ctx, oneof, None, &mut diags);
        }
    }
    diags
}

/// Borrowed name→definition lookups for policy validation. First-wins on
/// duplicate names, matching the schema index's documented convention.
struct PolicyCtx<'a> {
    models: HashMap<&'a str, &'a ModelDef>,
    oneofs: HashMap<&'a str, &'a OneOfDef>,
}

impl<'a> PolicyCtx<'a> {
    fn new(models: &'a [ModelDef], oneofs: &'a [OneOfDef]) -> Self {
        let mut model_map: HashMap<&str, &ModelDef> = HashMap::new();
        for m in models {
            model_map.entry(m.name.as_str()).or_insert(m);
        }
        let mut oneof_map: HashMap<&str, &OneOfDef> = HashMap::new();
        for o in oneofs {
            oneof_map.entry(o.name.as_str()).or_insert(o);
        }
        Self {
            models: model_map,
            oneofs: oneof_map,
        }
    }

    fn model(&self, name: &str) -> Option<&'a ModelDef> {
        self.models.get(name).copied()
    }

    fn oneof(&self, name: &str) -> Option<&'a OneOfDef> {
        self.oneofs.get(name).copied()
    }
}

fn declared_policy_names(field: &FieldDef) -> Vec<&str> {
    field
        .directives
        .iter()
        .filter(|d| POLICY_NAMES.contains(&d.name.as_str()))
        .map(|d| d.name.as_str())
        .collect()
}

fn validate_field_policy(
    ctx: &PolicyCtx<'_>,
    model: &ModelDef,
    field: &FieldDef,
    diags: &mut Vec<Diagnostic>,
) {
    let names = declared_policy_names(field);
    let span = field.span;
    let source = model.source.clone();
    let at = move |msg: String| {
        let d = Diagnostic::error(msg)
            .with_code(codes::INVALID_MERGE_POLICY)
            .with_span(span);
        match &source {
            Some(src) => d.with_source(src.clone()),
            None => d,
        }
    };
    let sealed = names.contains(&"sealed");
    let overlay = names.contains(&"overlay");
    let identity = names.contains(&"identity");
    let append = names.contains(&"append");

    if sealed && names.len() > 1 {
        diags.push(at(format!(
            "'{}.{}': `#sealed` composes with no other merge policy — \
             write-once contradicts every other grant; seal the item \
             fields, not the list, when that is the intent",
            model.name, field.name
        )));
        return;
    }
    if overlay && names.len() > 1 {
        diags.push(at(format!(
            "'{}.{}': `#overlay` is the explicit spelling of the default \
             and combines with nothing",
            model.name, field.name
        )));
        return;
    }
    let ty = &field.field_type;
    if (identity || append) && !is_list_like(effective_type(ty)) {
        diags.push(at(format!(
            "'{}.{}': `#identity`/`#append` are list and set policies — \
             this field is not a collection",
            model.name, field.name
        )));
        return;
    }
    if identity {
        let inner = list_inner(effective_type(ty));
        let mergeable = matches!(inner, Some(FieldType::ModelRef(_)));
        if is_set(effective_type(ty)) || !mergeable {
            diags.push(at(format!(
                "'{}.{}': `#identity` needs items with something to key and \
                 something to merge — plain scalar lists and `set<T>` have \
                 neither (`#append` and overlay are the policies that mean \
                 something there); seal the item fields, not the list, when \
                 that is the intent",
                model.name, field.name
            )));
            return;
        }
    }
    // NML2076 arm 3: a defaulted `#sealed` field never engages — a default
    // is not an assignment.
    if sealed && field.default_value.is_some() {
        let d = Diagnostic::warning(format!(
            "'{}.{}': `#sealed` with a schema default cannot engage — a \
             default is not an assignment; this field stays open until \
             some layer writes it",
            model.name, field.name
        ))
        .with_code(codes::UNREACHABLE_SEAL)
        .with_span(span);
        diags.push(match &model.source {
            Some(src) => d.with_source(src.clone()),
            None => d,
        });
    }
    // NML2076 arm 1: item seals under a bare-overlay list never engage.
    // A oneof ELEMENT's seals live in its arm models — look through it,
    // or a sealed-arm oneof list slips the lint entirely.
    if policy_of(field) == MergePolicy::Overlay {
        if let Some(FieldType::ModelRef(item)) = list_inner(effective_type(ty)) {
            let declares_seal = ctx
                .model(item)
                .is_some_and(|m| model_declares_seal(ctx, m, &mut HashSet::new()))
                || ctx.oneof(item).is_some_and(|o| {
                    o.variants.iter().any(|(_, v)| {
                        ctx.model(v)
                            .is_some_and(|m| model_declares_seal(ctx, m, &mut HashSet::new()))
                    })
                });
            if declares_seal {
                let d = Diagnostic::warning(format!(
                    "'{}.{}': item model '{}' declares `#sealed` fields, \
                     but a bare-overlay list is replaced wholesale — the \
                     seals never engage; grant the list `#identity` (and \
                     optionally `#append`) to make them reachable",
                    model.name, field.name, item
                ))
                .with_code(codes::UNREACHABLE_SEAL)
                .with_span(span);
                diags.push(match &model.source {
                    Some(src) => d.with_source(src.clone()),
                    None => d,
                });
            }
        }
    }
    // NML2076 arm 2: oneof-typed field with sealed arm fields and an
    // unsealed discriminator.
    if let FieldType::ModelRef(name) = effective_type(ty) {
        if let Some(oneof) = ctx.oneof(name) {
            lint_oneof_seals(ctx, oneof, Some((model, field)), diags);
        }
    }
}

/// `Modifier(inner)` fields carry their policy semantics on the inner type.
fn effective_type(ty: &FieldType) -> &FieldType {
    match ty {
        FieldType::Modifier(inner) => inner,
        other => other,
    }
}

fn list_inner(ty: &FieldType) -> Option<&FieldType> {
    match ty {
        FieldType::List(inner) | FieldType::Set(inner) => Some(inner),
        _ => None,
    }
}

fn model_declares_seal(ctx: &PolicyCtx<'_>, model: &ModelDef, seen: &mut HashSet<String>) -> bool {
    if !seen.insert(model.name.clone()) {
        return false; // cycle guard
    }
    model.fields.iter().any(|f| {
        policy_of(f) == MergePolicy::Sealed
            || match effective_type(&f.field_type) {
                FieldType::ModelRef(name) => ctx
                    .model(name)
                    .is_some_and(|m| model_declares_seal(ctx, m, seen)),
                FieldType::List(inner) | FieldType::Set(inner) => {
                    matches!(inner.as_ref(), FieldType::ModelRef(name)
                        if ctx.model(name)
                            .is_some_and(|m| model_declares_seal(ctx, m, seen)))
                }
                _ => false,
            }
    })
}

fn lint_oneof_seals(
    ctx: &PolicyCtx<'_>,
    oneof: &OneOfDef,
    at_field: Option<(&ModelDef, &FieldDef)>,
    diags: &mut Vec<Diagnostic>,
) {
    let sealed_arm = oneof.variants.iter().any(|(_, variant)| {
        ctx.model(variant)
            .is_some_and(|m| model_declares_seal(ctx, m, &mut HashSet::new()))
    });
    if !sealed_arm {
        return;
    }
    let discriminator_sealed = oneof.variants.iter().all(|(_, variant)| {
        ctx.model(variant).is_some_and(|m| {
            m.fields
                .iter()
                .any(|f| f.name == oneof.discriminator && policy_of(f) == MergePolicy::Sealed)
        })
    });
    if discriminator_sealed {
        return;
    }
    let (site, span) = match at_field {
        Some((model, field)) => (
            format!("'{}.{}' (oneof '{}')", model.name, field.name, oneof.name),
            field.span,
        ),
        None => (format!("oneof '{}'", oneof.name), oneof.span),
    };
    let source = match at_field {
        Some((model, _)) => model.source.clone(),
        None => oneof.source.clone(),
    };
    let d = Diagnostic::warning(format!(
        "{site}: an arm model declares `#sealed` fields but the \
         discriminator '{}' is not sealed — an arm switch discards the \
         sealed body (the seal backstop still rejects a switch that \
         would drop an assigned seal); seal the discriminator to forbid \
         switching outright",
        oneof.discriminator
    ))
    .with_code(codes::UNREACHABLE_SEAL)
    .with_span(span);
    diags.push(match source {
        Some(src) => d.with_source(src),
        None => d,
    });
}

// ───────────────────────────────────────────────────────── linearization ──

/// C3 merge over precedence-ordered lists (head = highest precedence).
/// Returns `None` when no consistent linearization exists (NML2077).
fn c3_merge<'a>(mut seqs: Vec<Vec<InstanceId<'a>>>) -> Option<Vec<InstanceId<'a>>> {
    let mut result = Vec::new();
    seqs.retain(|s| !s.is_empty());
    while !seqs.is_empty() {
        let head = seqs
            .iter()
            .map(|s| s[0])
            .find(|h| !seqs.iter().any(|s| s[1..].contains(h)))?;
        result.push(head);
        for s in &mut seqs {
            s.retain(|x| *x != head);
        }
        seqs.retain(|s| !s.is_empty());
    }
    Some(result)
}

/// Linearize the stack rooted at `declaring` with listed `refs`, in NML's
/// reversed orientation: precedence increases left to right, so the C3
/// merge runs over REVERSED ref lists and the result is read back-to-front
/// into bottom-up compose order. The declaring instance is the final (top)
/// element.
///
/// Returns bottom-up order (lowest first). Site authorization runs during
/// discovery (RFC 0019 step 1 is logical layering, not temporal phasing).
struct Linearizer<'a, 'p> {
    instances: &'a InstanceIndex<'a>,
    grants: &'p dyn LayerGrantProvider,
    declaring_keyword: &'a str,
    diags: Vec<Diagnostic>,
    /// Memoized precedence-ordered (head-first) linearizations.
    memo: HashMap<InstanceId<'a>, Option<Vec<InstanceId<'a>>>>,
    /// The live recursion path, in order — doubles as the cycle detector
    /// and lets NML2061 render the full cycle (`a -> b -> a`), matching
    /// the house cycle-diagnostic vocabulary.
    in_progress: Vec<InstanceId<'a>>,
}

impl<'a, 'p> Linearizer<'a, 'p> {
    /// Precedence-ordered (head-first) linearization of one instance.
    fn linearize(&mut self, id: InstanceId<'a>) -> Option<Vec<InstanceId<'a>>> {
        if let Some(done) = self.memo.get(&id) {
            return done.clone();
        }
        // Discovery-time recursion bound: the post-linearization depth check
        // cannot protect the linearizer itself — a generated 10,000-link
        // chain must fail HERE, at 16 frames, not after recursing its full
        // length (RFC 0019: "a runaway generator cannot stack-bomb the
        // checker"). `in_progress` is exactly the live recursion path.
        if self.in_progress.len() as u32 >= MAX_STACK_DEPTH {
            self.diags.push(
                layer_bound_exceeded(LayerBound::Discovery { instance: id.name })
                    .with_source(id.source_path.to_string()),
            );
            return None;
        }
        if self.in_progress.contains(&id) {
            let start = self
                .in_progress
                .iter()
                .position(|x| *x == id)
                .expect("contains");
            // Canonicalize the rotation (smallest member name first): the
            // same cycle discovered from different roots must render — and
            // anchor — identically, so multi-root composes report ONE
            // finding through FindingKey dedup, not one per entry point.
            let cycle: Vec<InstanceId<'a>> = self.in_progress[start..].to_vec();
            let pivot = cycle
                .iter()
                .enumerate()
                .min_by_key(|(_, x)| x.name)
                .map(|(i, _)| i)
                .expect("cycle is non-empty");
            let mut path: Vec<&str> = cycle[pivot..]
                .iter()
                .chain(cycle[..pivot].iter())
                .map(|x| x.name)
                .collect();
            path.push(cycle[pivot].name);
            let anchor = cycle[pivot];
            let mut d = Diagnostic::error(format!("`uses` reference cycle: {}", path.join(" -> ")))
                .with_code(codes::LAYER_CYCLE)
                .with_source(anchor.source_path.to_string());
            if let Some(block) = self.instances.get(anchor) {
                d = d.with_span(block.name.span);
            }
            self.diags.push(d);
            return None;
        }
        self.in_progress.push(id);
        let result = self.linearize_inner(id);
        self.in_progress.pop();
        self.memo.insert(id, result.clone());
        result
    }

    fn linearize_inner(&mut self, id: InstanceId<'a>) -> Option<Vec<InstanceId<'a>>> {
        let block = self.instances.get(id)?;
        // Every layer must declare the same model keyword (NML2062).
        if block.keyword.name != self.declaring_keyword {
            self.diags.push(
                Diagnostic::error(format!(
                    "`uses` target '{}' is a `{}`, not a `{}` — layers \
                     compose only within one model keyword{}",
                    id.name,
                    block.keyword.name,
                    self.declaring_keyword,
                    // Did-you-mean over in-scope SAME-keyword instances
                    // (RFC's NML2062 row) — the author probably reached
                    // for a near-named layer, not a cross-keyword one.
                    crate::suggest::suggest(
                        id.name,
                        self.instances.names().filter(|n| {
                            *n != id.name
                                && self
                                    .instances
                                    .resolve_ref(n)
                                    .and_then(|t| self.instances.get(t))
                                    .is_some_and(|b| b.keyword.name == self.declaring_keyword)
                        }),
                    )
                    .map(|h| format!(" — did you mean '{h}'?"))
                    .unwrap_or_default()
                ))
                .with_code(codes::LAYER_KEYWORD_MISMATCH)
                .with_span(block.name.span)
                .with_source(id.source_path.to_string()),
            );
            return None;
        }
        // Site authorization: this clause's listed refs against the
        // authoring site's own grant.
        let refs = self.site_checked_refs(id, block)?;
        let merged = self.merge_listed(id, block.name.span, &refs)?;
        let mut lin = vec![id];
        lin.extend(merged);
        Some(lin)
    }

    /// The ONE listed-refs → precedence-ordered-merge implementation:
    /// order-preserving dedupe (a literal duplicate ref is redundant but
    /// legal), reversed-orientation C3 over each ref's own linearization,
    /// and the NML2077 emission. Shared by every transitive clause (via
    /// [`Self::linearize_inner`]) and the declaring clause (via
    /// [`resolve_layers`]) — the two can never linearize differently.
    fn merge_listed(
        &mut self,
        id: InstanceId<'a>,
        span: Span,
        refs: &[InstanceId<'a>],
    ) -> Option<Vec<InstanceId<'a>>> {
        let mut deduped: Vec<InstanceId<'a>> = Vec::new();
        for r in refs {
            if !deduped.contains(r) {
                deduped.push(*r);
            }
        }
        let mut parent_lins = Vec::new();
        for r in deduped.iter().rev() {
            parent_lins.push(self.linearize(*r)?);
        }
        let order_constraint: Vec<InstanceId<'a>> = deduped.iter().rev().copied().collect();
        parent_lins.push(order_constraint);
        // Breadth guard, BEFORE the merge: the C3 loop is superlinear in
        // the number of sequences, and the post-linearization depth check
        // runs only after that work is spent — a single wide clause
        // (thousands of listed refs over shared sub-layers) would buy
        // minutes of CPU from kilobytes of input. The merged result
        // contains every distinct discovered instance exactly once, so a
        // distinct-count over the inputs (+1 for the declaring instance)
        // that already exceeds the cap is guaranteed NML2066 — reject in
        // linear time and never start the merge.
        let mut distinct: HashSet<InstanceId<'a>> = HashSet::new();
        for seq in &parent_lins {
            distinct.extend(seq.iter().copied());
        }
        if distinct.len() as u32 + 1 > MAX_STACK_DEPTH {
            self.diags.push(
                layer_bound_exceeded(LayerBound::Language {
                    depth: distinct.len() as u32 + 1,
                })
                .with_span(span)
                .with_source(id.source_path.to_string()),
            );
            return None;
        }
        match c3_merge(parent_lins) {
            Some(merged) => Some(merged),
            None => {
                self.diags.push(
                    self.explain_inconsistency(id, &deduped)
                        .with_span(span)
                        .with_source(id.source_path.to_string()),
                );
                None
            }
        }
    }

    /// NML2077 with the RFC's teaching shape: name the contradicting pair
    /// and the forcing clause, not just "something contradicts". Two
    /// causes exist (both detectable from the memoized per-ref
    /// linearizations, all ≤ the 16-layer cap, so this is trivial work on
    /// an already-failing path):
    ///
    /// 1. A listed ref is a transitive base of an EARLIER-listed ref —
    ///    listing it after its own dependent would order it above it.
    ///    Carries a machine-applicable remove-the-ref suggestion.
    /// 2. Two listed refs' own stacks order a shared pair oppositely —
    ///    no listing order can satisfy both; the stacks must be aligned.
    ///
    /// Falls back to the generic either/or wording only when neither
    /// pattern is found (a multi-clause interaction pinned mid-merge).
    fn explain_inconsistency(&self, id: InstanceId<'a>, deduped: &[InstanceId<'a>]) -> Diagnostic {
        let block = self.instances.get(id);
        let ref_span =
            |n: &str| block.and_then(|b| b.uses.iter().find(|u| u.name == n).map(|u| u.span));
        for (i, a) in deduped.iter().enumerate() {
            let Some(Some(a_lin)) = self.memo.get(a) else {
                continue;
            };
            for b in &deduped[i + 1..] {
                if a_lin.iter().skip(1).any(|x| x == b) {
                    let mut d = Diagnostic::error(format!(
                        "no consistent linearization for '{}' — '{}' is \
                         already a transitive base of '{}', so listing it \
                         after '{}' would order it above its own \
                         dependent; remove the redundant ref or list it \
                         before '{}'",
                        id.name, b.name, a.name, a.name, a.name
                    ))
                    .with_code(codes::INCONSISTENT_LINEARIZATION);
                    // The deletion span swallows the preceding separator
                    // (`, b` not `b`) so applying the fix never leaves a
                    // dangling comma; the contradicting ref is listed
                    // after its dependent, so a predecessor always exists.
                    let sugg = block.and_then(|blk| {
                        // A duplicated listed name would need every
                        // occurrence removed — one span cannot express
                        // that, and a machine fix that doesn't fix stalls
                        // `nml fix`'s strict-improvement loop. Suggest
                        // only when the name occurs once.
                        if blk.uses.iter().filter(|u| u.name == b.name).count() != 1 {
                            return None;
                        }
                        let idx = blk.uses.iter().position(|u| u.name == b.name)?;
                        let end = blk.uses[idx].span.end;
                        let start = if idx > 0 {
                            blk.uses[idx - 1].span.end
                        } else {
                            blk.uses[idx].span.start
                        };
                        Some(Span::new(start, end))
                    });
                    if let Some(s) = sugg {
                        d = d.with_suggestion("", s);
                    }
                    if let Some(ab) = self.instances.get(*a) {
                        d = d.with_related(
                            ab.name.span,
                            format!("'{}' already composes '{}' here", a.name, b.name),
                        );
                    }
                    return d;
                }
            }
        }
        for (i, a) in deduped.iter().enumerate() {
            let Some(Some(a_lin)) = self.memo.get(a) else {
                continue;
            };
            for b in &deduped[i + 1..] {
                let Some(Some(b_lin)) = self.memo.get(b) else {
                    continue;
                };
                for (xi, x) in a_lin.iter().enumerate() {
                    for y in &a_lin[xi + 1..] {
                        let (Some(bx), Some(by)) = (
                            b_lin.iter().position(|e| e == x),
                            b_lin.iter().position(|e| e == y),
                        ) else {
                            continue;
                        };
                        if bx > by {
                            let mut d = Diagnostic::error(format!(
                                "no consistent linearization for '{}' — \
                                 '{}' and '{}' order the shared pair '{}', \
                                 '{}' oppositely in their own stacks; \
                                 align them: order '{}' above '{}' in \
                                 '{}', or '{}' above '{}' in '{}' — or \
                                 drop one ref",
                                id.name,
                                a.name,
                                b.name,
                                x.name,
                                y.name,
                                x.name,
                                y.name,
                                b.name,
                                y.name,
                                x.name,
                                a.name,
                            ))
                            .with_code(codes::INCONSISTENT_LINEARIZATION);
                            if let Some(s) = ref_span(b.name) {
                                d = d.with_related(
                                    s,
                                    format!("'{}' orders '{}' below '{}'", b.name, y.name, x.name),
                                );
                            }
                            return d;
                        }
                    }
                }
            }
        }
        let listed: Vec<&str> = deduped.iter().map(|r| r.name).collect();
        inconsistent_linearization(id.name, &listed)
    }

    /// Resolve and site-authorize one clause's listed refs.
    fn site_checked_refs(
        &mut self,
        id: InstanceId<'a>,
        block: &'a BlockDecl,
    ) -> Option<Vec<InstanceId<'a>>> {
        if block.uses.is_empty() {
            return Some(Vec::new());
        }
        let site = self.grants.grant_for(id.source_path);
        if let Some(diag) = deny_diagnostic(&site, id, block) {
            self.diags.push(diag);
            return None;
        }
        let mut refs = Vec::new();
        let mut ok = true;
        for r in &block.uses {
            let Some(target) = self.instances.resolve_ref(&r.name) else {
                self.diags.push(
                    unresolved_ref(self.instances, id.name, self.declaring_keyword, &r.name)
                        .with_span(r.span)
                        .with_source(id.source_path.to_string()),
                );
                ok = false;
                continue;
            };
            if let GrantLookup::Granted { grant, binding, .. } = &site {
                let decision = self.grants.ref_decision(grant, target.source_path);
                if let Some(d) = ref_denial(decision, &r.name, binding, Denial::Site) {
                    self.diags
                        .push(d.with_span(r.span).with_source(id.source_path.to_string()));
                    ok = false;
                    continue;
                }
            }
            refs.push(target);
        }
        ok.then_some(refs)
    }
}

/// The one NML2059 wording owner: unresolved `uses` ref with a
/// did-you-mean over in-scope same-keyword instances (the declaring
/// instance itself excluded — suggesting a self-cycle helps no one).
fn unresolved_ref(
    instances: &InstanceIndex<'_>,
    declaring_name: &str,
    keyword: &str,
    ref_name: &str,
) -> Diagnostic {
    let hint = crate::suggest::suggest(
        ref_name,
        instances.names().filter(|n| {
            *n != declaring_name
                && instances
                    .resolve_ref(n)
                    .and_then(|t| instances.get(t))
                    .is_some_and(|b| b.keyword.name == keyword)
        }),
    );
    let mut msg = format!("`uses` ref '{ref_name}' does not resolve");
    if let Some(h) = hint {
        msg.push_str(&format!(" — did you mean '{h}'?"));
    }
    Diagnostic::error(msg).with_code(codes::UNRESOLVED_LAYER_REF)
}

/// A list-shaped entry that normalized to zero items: it does not supply
/// the list (NML2079's contract) — it neither replaces, empties, nor
/// seals. One predicate, used by every path that must honor that rule.
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

/// One name→field lookup, FIRST-wins on duplicate names — the module's
/// one convention (SchemaIndex, PolicyCtx, the old linear `find`s), and
/// the fail-closed direction: the first duplicate's policy governs, so a
/// broken schema can never silently swap a `#sealed` for an open field.
fn first_wins_field_map(model: Option<&ModelDef>) -> HashMap<&str, &FieldDef> {
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
fn items_of(kind: &BodyEntryKind) -> Option<Vec<ListItem>> {
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

/// Whether an entry on a `#sealed` field counts as a WRITE under the
/// engine's own semantics: for a list-shaped field a zero-item entry
/// (empty array spelling included) neither supplies nor seals (NML2079's
/// contract); for every other shape — scalars and object-typed fields —
/// every entry is a write. One predicate shared by `merge_sealed` and the
/// backstop scan so "assigned" can never mean two different things.
fn seal_write(f: &FieldDef, kind: &BodyEntryKind) -> bool {
    if !is_list_like(effective_type(&f.field_type)) {
        return true;
    }
    match kind {
        BodyEntryKind::Property(p) => !matches!(&p.value.value, Value::Array(vs) if vs.is_empty()),
        _ => !is_zero_item_entry(kind),
    }
}

/// The nested-block bodies named `name` across a group of sibling bodies,
/// in group order — the sub-bodies a nested field's own compose would run
/// over, used to fold candidate arms at oneof-typed positions.
fn sub_bodies_at<'b, 'a>(
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

/// The arms a group of bodies at one oneof position could have made
/// effective: the schema default plus every discriminator value any body
/// states, in encounter order, deduplicated.
fn candidate_arms(oneof: &OneOfDef, bodies: &[(InstanceId<'_>, &Body)]) -> Vec<String> {
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

/// Dotted field-path join — one spelling owner for provenance keys and
/// diagnostic paths.
pub(crate) fn join_path(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

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

/// NML2062's schema-definition form — one wording owner for the
/// composing path (`compose_file`) and the definition verb
/// (`check_uses_refs`), so the two can never describe the defect
/// differently.
fn schema_def_uses_denial(block: &BlockDecl, source_path: &str) -> Diagnostic {
    Diagnostic::error(format!(
        "`uses` is an instance clause — a `{}` definition cannot \
         compose layers; delete the clause",
        block.keyword.name
    ))
    .with_code(codes::LAYER_KEYWORD_MISMATCH)
    .with_span(block.name.span)
    .with_source(source_path.to_string())
}

/// NML2077's generic fallback wording — reached only when
/// `explain_inconsistency` (the one emission site, shared by the
/// declaring clause and every transitive clause via `merge_listed`)
/// cannot pin the contradiction to a named pair.
fn inconsistent_linearization(name: &str, listed: &[&str]) -> Diagnostic {
    Diagnostic::error(format!(
        "no consistent linearization for '{name}' — the `uses` orders of \
         [{}] contradict: a listed layer is a transitive base of an \
         earlier-listed layer, or two listed layers' own stacks order a \
         shared pair oppositely; reorder or drop the contradicting ref",
        listed.join(", ")
    ))
    .with_code(codes::INCONSISTENT_LINEARIZATION)
}

/// One NML2065 emission for both denial modes, shared by every check site
/// (site-level, declaring-clause, stack-level) so the wording — and the
/// disclosure rules riding on it — cannot drift per site.
/// Where a `uses` denial was raised — a named scope beats the earlier
/// `Option<Option<&str>>`, which a reader had to decode as
/// site / stack-anonymous / stack-with-entering-ref.
enum Denial<'a> {
    /// A declaring or transitive clause's own listed ref — the ref name
    /// IS the author's token, so it may be disclosed.
    Site,
    /// The root grant bounding a transitively-pulled layer the author
    /// never named. `entering` is the root clause's own listed ref that
    /// pulls it in (the author's token), when known.
    Stack { entering: Option<&'a str> },
}

fn ref_denial(
    decision: RefDecision,
    ref_name: &str,
    binding: &str,
    scope: Denial<'_>,
) -> Option<Diagnostic> {
    // Stack-level denials name the ENTERING ref — the root clause's
    // listed ref whose stack pulls the denied layer in — so the author
    // knows which ref to remove (the denied layer itself may sit several
    // clauses away).
    let suffix = match scope {
        Denial::Stack {
            entering: Some(entering),
        } => format!(
            " (stack-level: the root grant bounds every composed layer; \
             '{entering}' in this clause pulls it in)"
        ),
        Denial::Stack { entering: None } => {
            " (stack-level: the root grant bounds every composed layer)".to_string()
        }
        Denial::Site => String::new(),
    };
    let msg = match decision {
        RefDecision::Allowed => return None,
        RefDecision::DenyVeto(i) => format!(
            "`uses` ref '{ref_name}' denied by denyRefs[{i}] of binding \
             '{binding}'{suffix}"
        ),
        // Allow-miss discloses the BINDING, never the missed target: at
        // the site level the ref name is the author's own token, but a
        // stack-level denial reaches layers the author never named —
        // echoing a transitively-pulled layer's (author-chosen) instance
        // name would leak through the denial. The entering ref in the
        // suffix is the author's own clause token.
        RefDecision::AllowMiss => match scope {
            Denial::Site => format!(
                "`uses` ref '{ref_name}' denied: no allowRefs entry of \
                 binding '{binding}' admits this layer"
            ),
            Denial::Stack { .. } => format!(
                "`uses` stack denied: no allowRefs entry of binding \
                 '{binding}' admits a composed layer{suffix}"
            ),
        },
    };
    Some(Diagnostic::error(msg).with_code(codes::LAYER_REF_DENIED))
}

/// NML2064's three message forms, from the grant state. `None` = permitted.
fn deny_diagnostic(
    lookup: &GrantLookup<'_>,
    id: InstanceId<'_>,
    block: &BlockDecl,
) -> Option<Diagnostic> {
    let base = |msg: String| {
        Diagnostic::error(msg)
            .with_code(codes::COMPOSITION_DENIED)
            .with_span(block.name.span)
            .with_source(id.source_path.to_string())
    };
    match lookup {
        GrantLookup::Granted { .. } | GrantLookup::Unbound { open_context: true } => None,
        GrantLookup::NoGrant { binding, manifest } => Some(base(format!(
            "composition not permitted: binding '{binding}' ({manifest}) \
             carries no `layers:` grant — an operator change, not fixable \
             from a content file; run `nml binding <file>` to see the \
             effective grant"
        ))),
        GrantLookup::Ambiguous { manifests } => Some(base(format!(
            "composition not permitted: {} manifests claim this file ({}) — \
             an ambiguously-claimed file is denied; remove or narrow one \
             claim, then run `nml binding <file>`",
            manifests.len(),
            manifests.join(", ")
        ))),
        GrantLookup::Unbound {
            open_context: false,
        } => Some(base(
            "composition not permitted: no binding governs this file in a \
             closed universe — add a `files` glob that claims it (an \
             operator change), then run `nml binding <file>`"
                .to_string(),
        )),
    }
}

// ───────────────────────────────────────────────────────── normalization ──

/// Step 3, per layer, in the shipped pipeline's order: array-reference
/// inlining against the layer's own document → positional/identity
/// materialization → `.shared:` merge — then spelling normalization
/// (array-literal properties and inline modifiers rewrite to the block
/// spelling so list policies bind regardless of authored form).
fn normalize_inlined(
    index: &SchemaIndex,
    root: &str,
    inlined: &Body,
    plan: &ArmPlan,
    source_path: &str,
    diags: &mut Vec<Diagnostic>,
) -> Body {
    let positional = crate::identity::apply_positional_planned(index, root, inlined, &plan.arms);
    let shared = crate::resolve::apply_shared_properties(&positional);
    let vocab = index
        .model(root)
        .or_else(|| oneof_vocab(index, root, &shared, "", plan));
    normalize_spellings(index, vocab, &shared, "", plan, source_path, diags)
}

/// RFC 0019 step 3's discriminator pre-pass result: the stack's effective
/// arm at each oneof position, folded bottom-up from the schema default
/// over the array-ref-inlined layer bodies — computed BEFORE per-layer
/// materialization so every layer normalizes against the arm the stack
/// actually composes, not its own default-filled guess. Keyed by dotted
/// field path from the instance root; `""` is a oneof root. List items
/// are their own single-item-group scopes and are deliberately not
/// planned (their vocabularies resolve per item).
///
/// `decisions` is the fold's full per-layer trace at each planned
/// position — the ONE arm-decision authority. The merge REPLAYS it
/// rather than re-deriving decisions from its own (differently
/// normalized) view of the bodies: two independent accumulators over two
/// representations is exactly how positional machinery injected under
/// one arm fabricated seal refusals against another.
#[derive(Default)]
struct ArmPlan<'a> {
    arms: HashMap<String, String>,
    decisions: HashMap<String, Vec<(InstanceId<'a>, ArmDecision<'a>)>>,
}

/// One layer's fate at one oneof position, decided by the fold.
enum ArmDecision<'a> {
    /// Omitted or restated-at-effective: the layer joins the group and
    /// deep-merges.
    Join,
    /// Accepted switch: the group restarts at this layer.
    Switch,
    /// Backstop-rejected switch: the layer contributes nothing; the
    /// merge emits NML2060 from these recorded seals (position-relative
    /// paths; lowest-then-document order, `.len()` is the count).
    Rejected {
        seals: Vec<(String, Span, InstanceId<'a>)>,
    },
}

/// Normalize one authored (inlined) body under a CANDIDATE arm model for
/// seal judgment: positional materialization, `.shared` distribution,
/// spelling normalization — the scan must judge the value the displaced
/// compose WOULD CARRY. Raw-body scanning misses `.shared`-distributed
/// and positionally-materialized sealed writes; scanning bodies
/// normalized under a DIFFERENT arm counts that arm's machinery
/// injections as authored writes. Probe-only: diagnostics are discarded
/// (the real normalization emits them once, against the real plan).
fn normalize_for_scan(index: &SchemaIndex, arm: &ModelDef, body: &Body) -> Body {
    let no_plan = ArmPlan::default();
    let positional =
        crate::identity::apply_positional_planned(index, &arm.name, body, &no_plan.arms);
    let shared = crate::resolve::apply_shared_properties(&positional);
    normalize_spellings(index, Some(arm), &shared, "", &no_plan, "", &mut Vec::new())
}

fn build_arm_plan<'a>(
    index: &SchemaIndex,
    root: &str,
    layers: &[(InstanceId<'a>, Body)],
) -> ArmPlan<'a> {
    let mut plan = ArmPlan::default();
    let bodies: Vec<(InstanceId<'a>, &Body)> = layers.iter().map(|(l, b)| (*l, b)).collect();
    if let Some(oneof) = index.oneof(root) {
        let (arm, trace) = fold_arm_checked(index, oneof, &bodies);
        let survivors = surviving_entries(&trace, &bodies);
        plan.decisions.insert(String::new(), trace);
        if let Some(arm) = arm {
            let vocab = variant_model_of(index, oneof, &arm);
            plan.arms.insert(String::new(), arm);
            if let Some(m) = vocab {
                arm_plan_walk(index, m, &survivors, "", &mut plan);
            }
        }
    } else if let Some(m) = index.model(root) {
        arm_plan_walk(index, m, &bodies, "", &mut plan);
    }
    plan
}

/// The entries that SURVIVE a fold's decisions — everything after the
/// last accepted switch, rejected switches excluded. Children of a oneof
/// position must be planned over exactly this membership: the merge
/// reaches nested positions only through the surviving group, and a
/// trace folded over layers the parent discarded replays against a
/// different entry list than it was computed over — the membership
/// half of the divergence class the decision trace exists to kill.
fn surviving_entries<'a, 'b>(
    trace: &[(InstanceId<'a>, ArmDecision<'a>)],
    bodies: &[(InstanceId<'a>, &'b Body)],
) -> Vec<(InstanceId<'a>, &'b Body)> {
    let mut group: Vec<usize> = Vec::new();
    for (i, (_, d)) in trace.iter().enumerate() {
        match d {
            ArmDecision::Join => group.push(i),
            ArmDecision::Switch => {
                group.clear();
                group.push(i);
            }
            ArmDecision::Rejected { .. } => {}
        }
    }
    group.into_iter().map(|i| bodies[i]).collect()
}

fn arm_plan_walk<'a>(
    index: &SchemaIndex,
    model: &ModelDef,
    bodies: &[(InstanceId<'a>, &Body)],
    path: &str,
    plan: &mut ArmPlan<'a>,
) {
    // FIRST-wins on a duplicate field name — the plan MUST key each path
    // by the same field the merge's `first_wins_field_map` resolves, or a
    // trace folded under the wrong duplicate replays against a body the
    // merge composed under the other (the alignment guard sees matching
    // ids and cannot catch it). `seen` makes the walk first-wins too.
    let mut seen: HashSet<&str> = HashSet::new();
    for f in &model.fields {
        if !seen.insert(f.name.as_str()) {
            continue;
        }
        let FieldType::ModelRef(n) = effective_type(&f.field_type) else {
            continue;
        };
        let subs = sub_bodies_at(bodies, &f.name);
        if subs.is_empty() {
            continue;
        }
        let fpath = join_path(path, &f.name);
        if let Some(nested) = index.model(n) {
            arm_plan_walk(index, nested, &subs, &fpath, plan);
        } else if let Some(oneof) = index.oneof(n) {
            let (arm, trace) = fold_arm_checked(index, oneof, &subs);
            let survivors = surviving_entries(&trace, &subs);
            plan.decisions.insert(fpath.clone(), trace);
            if let Some(arm) = arm {
                let vocab = variant_model_of(index, oneof, &arm);
                plan.arms.insert(fpath.clone(), arm);
                if let Some(nested) = vocab {
                    arm_plan_walk(index, nested, &survivors, &fpath, plan);
                }
            }
        }
    }
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
fn fold_arm_checked<'a>(
    index: &SchemaIndex,
    oneof: &OneOfDef,
    bodies: &[(InstanceId<'a>, &Body)],
) -> (Option<String>, Vec<(InstanceId<'a>, ArmDecision<'a>)>) {
    let mut effective = oneof.default_discriminator.clone();
    let mut group: Vec<(InstanceId<'a>, &Body)> = Vec::new();
    let mut trace: Vec<(InstanceId<'a>, ArmDecision<'a>)> = Vec::new();
    for (id, b) in bodies {
        let stated = stated_discriminator(b, &oneof.discriminator);
        match stated {
            Some(v) if Some(&v) != effective.as_ref() => {
                let displaced = effective
                    .as_ref()
                    .and_then(|d| variant_model_of(index, oneof, d));
                let seals = match displaced {
                    Some(dm) => {
                        let normd: Vec<(InstanceId<'a>, Body)> = group
                            .iter()
                            .map(|(gid, gb)| (*gid, normalize_for_scan(index, dm, gb)))
                            .collect();
                        let refs: Vec<(InstanceId<'a>, &Body)> =
                            normd.iter().map(|(gid, gb)| (*gid, gb)).collect();
                        assigned_seals_over(index, "", &[dm], &refs)
                    }
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

/// The plain bottom-up discriminator fold (RFC 0019 step 3): the last
/// layer to state the discriminator wins, else the schema default. The
/// merge-time accumulator (with its seal backstop) remains the authority
/// on the composed VALUE — the fold only picks the normalization
/// vocabulary, and a backstop-rejected switch's layers are discarded at
/// merge time regardless of how they normalized.
fn fold_arm(oneof: &OneOfDef, bodies: &[&Body]) -> Option<String> {
    bodies
        .iter()
        .rev()
        .find_map(|b| stated_discriminator(b, &oneof.discriminator))
        .or_else(|| oneof.default_discriminator.clone())
}

/// Which composition bound NML2066 fires for. One wording owner for the
/// code, like every other diagnostic in this module — its three forms
/// were previously open-coded at four sites with drifting phrasings.
enum LayerBound<'n> {
    /// Discovery-time: the recursion frame count hit the language cap
    /// before the full stack depth is known, so the instance is named
    /// instead of a number.
    Discovery { instance: &'n str },
    /// Composition-time: the distinct-instance count exceeds the cap.
    Language { depth: u32 },
    /// The grant's operator-set `maxStackDepth`.
    Grant { depth: u32, cap: u32 },
}

fn layer_bound_exceeded(bound: LayerBound<'_>) -> Diagnostic {
    let msg = match bound {
        LayerBound::Discovery { instance } => format!(
            "layer stack exceeds the language cap ({MAX_STACK_DEPTH}) at \
             '{instance}' — restructure the stack"
        ),
        LayerBound::Language { depth } => format!(
            "layer stack depth {depth} exceeds the language cap \
             ({MAX_STACK_DEPTH}) — restructure the stack"
        ),
        LayerBound::Grant { depth, cap } => format!(
            "layer stack depth {depth} exceeds the grant's maxStackDepth \
             = {cap} (an operator change)"
        ),
    };
    Diagnostic::error(msg).with_code(codes::LAYER_BOUND_EXCEEDED)
}

/// The string discriminator a body states, if any — the one reader for
/// "which arm did this layer name?", shared by the fold, the merge
/// accumulator, the vocab pickers, and the positionalizer (via
/// `crate::layers`). A non-string discriminator reads as unstated (it is
/// a type error the validator owns, never an arm selection).
pub(crate) fn stated_discriminator(body: &Body, disc: &str) -> Option<String> {
    body.entries.iter().find_map(|e| match &e.kind {
        BodyEntryKind::Property(p) if p.name.name == disc => match &p.value.value {
            Value::String(s) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    })
}

/// The discriminator entry a body states (the [`BodyEntry`] behind
/// [`stated_discriminator`]) — for callers that need its span.
fn stated_discriminator_entry<'b>(body: &'b Body, disc: &str) -> Option<&'b BodyEntry> {
    body.entries.iter().find(|e| {
        matches!(&e.kind, BodyEntryKind::Property(p)
            if p.name.name == disc && matches!(p.value.value, Value::String(_)))
    })
}

/// One variant-name → arm-model lookup for every consumer.
fn variant_model_of<'i>(
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

/// The normalization vocabulary of a oneof position: the plan's arm at
/// `path` when one exists, else the body's own stated discriminator, else
/// the schema default.
fn oneof_vocab<'i>(
    index: &'i SchemaIndex,
    name: &str,
    body: &Body,
    path: &str,
    plan: &ArmPlan,
) -> Option<&'i ModelDef> {
    let oneof = index.oneof(name)?;
    let disc = plan
        .arms
        .get(path)
        .cloned()
        .or_else(|| fold_arm(oneof, &[body]))?;
    variant_model_of(index, oneof, &disc)
}

/// `Value::Role`/`Value::Reference` map to their matching `ListItemKind`;
/// anything else is a bodiless scalar-keyed `Shorthand`. Mapping roles or
/// references to `Shorthand` would make `|deny = [@ops]` and a block-form
/// `- @ops` a cross-kind pair at an equal token — the exact inverse of the
/// spelling invariance normalization exists to provide.
fn items_from_array(values: &[SpannedValue]) -> Vec<ListItem> {
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

fn normalize_spellings(
    index: &SchemaIndex,
    model: Option<&ModelDef>,
    body: &Body,
    path: &str,
    plan: &ArmPlan,
    source_path: &str,
    diags: &mut Vec<Diagnostic>,
) -> Body {
    // One name→field map per body level (wide bodies × wide models made
    // the per-entry linear scan quadratic — a compose-path DoS axis).
    // FIRST-wins on a duplicate field name, exactly like the linear
    // `find` this replaced: a `collect()` would be last-wins, silently
    // swapping which duplicate's policy (e.g. `#sealed`) governs.
    let field_map = first_wins_field_map(model);
    let entries = body
        .entries
        .iter()
        .map(|entry| {
            let kind = match &entry.kind {
                BodyEntryKind::Property(p) => {
                    let field = field_map.get(p.name.name.as_str()).copied();
                    match (&p.value.value, field) {
                        (Value::Array(values), Some(f))
                            if is_list_like(effective_type(&f.field_type)) =>
                        {
                            if values.is_empty() {
                                diags.push(zero_item_warning(
                                    &p.name.name,
                                    entry.span,
                                    source_path,
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
                            // NML2079 is list-scoped: a declared NON-list
                            // modifier restated as `[]` is a type error's
                            // business, not a zero-item list entry. An
                            // undeclared name stays warned (fail-closed:
                            // modifiers are list-carriers by convention).
                            let list_like = field_map
                                .get(m.name.name.as_str())
                                .is_none_or(|f| is_list_like(effective_type(&f.field_type)));
                            if values.is_empty() && list_like {
                                diags.push(zero_item_warning(
                                    &m.name.name,
                                    entry.span,
                                    source_path,
                                ));
                            }
                            BodyEntryKind::Modifier(Modifier {
                                name: m.name.clone(),
                                value: ModifierValue::Block(items_from_array(values)),
                            })
                        }
                        _ => entry.kind.clone(),
                    },
                    _ => entry.kind.clone(),
                },
                BodyEntryKind::NestedBlock(nb) => {
                    let child_path = join_path(path, &nb.name.name);
                    let nb_field = field_map.get(nb.name.name.as_str()).copied();
                    let inner_model = nb_field.and_then(|f| match effective_type(&f.field_type) {
                        FieldType::ModelRef(name) => index.model(name).or_else(|| {
                            // Oneof-typed position: normalize against
                            // the pre-pass's effective arm (else the
                            // body's own stated-or-default arm), or
                            // its list fields silently keep their
                            // Property spelling and every policy
                            // misses them.
                            oneof_vocab(index, name, &nb.body, &child_path, plan)
                        }),
                        FieldType::List(inner) | FieldType::Set(inner) => match inner.as_ref() {
                            FieldType::ModelRef(name) => index.model(name),
                            _ => None,
                        },
                        _ => None,
                    });
                    let is_list =
                        nb_field.is_some_and(|f| is_list_like(effective_type(&f.field_type)));
                    if is_list
                        && !nb
                            .body
                            .entries
                            .iter()
                            .any(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
                    {
                        diags.push(zero_item_warning(&nb.name.name, entry.span, source_path));
                    }
                    BodyEntryKind::NestedBlock(NestedBlock {
                        name: nb.name.clone(),
                        body: normalize_spellings(
                            index,
                            inner_model,
                            &nb.body,
                            &child_path,
                            plan,
                            source_path,
                            diags,
                        ),
                    })
                }
                BodyEntryKind::ListItem(item) => {
                    // Item bodies are model bodies too (`model` here is the
                    // element model whenever the caller recursed through a
                    // list-typed block): their array-spelled list fields
                    // must normalize like any other, or list policies
                    // inside identity-merged items miss their targets.
                    let kind = match &item.kind {
                        ListItemKind::Named { name, body } => ListItemKind::Named {
                            name: name.clone(),
                            body: normalize_spellings(
                                index,
                                model,
                                body,
                                path,
                                plan,
                                source_path,
                                diags,
                            ),
                        },
                        ListItemKind::Shorthand {
                            value,
                            body: Some(b),
                        } => ListItemKind::Shorthand {
                            value: value.clone(),
                            body: Some(normalize_spellings(
                                index,
                                model,
                                b,
                                path,
                                plan,
                                source_path,
                                diags,
                            )),
                        },
                        other => other.clone(),
                    };
                    BodyEntryKind::ListItem(ListItem {
                        kind,
                        span: item.span,
                    })
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

fn zero_item_warning(field: &str, span: Span, source_path: &str) -> Diagnostic {
    Diagnostic::warning(format!(
        "'{field}' normalizes to zero items in a composing layer — it does \
         not supply the list, and \"empty the base list\" has no merge \
         spelling"
    ))
    .with_code(codes::ZERO_ITEM_LAYER_ENTRY)
    .with_span(span)
    .with_source(source_path.to_string())
}

// ───────────────────────────────────────────────────────────── resolve ──

/// Field path → origin: which layer's assignment produced each effective
/// entry. List items key by the identity pair (kind, token). Load-bearing
/// for NML2060's related spans, `--resolve-layers` attribution, the LSP's
/// resolved peek, and the embedder audit hash.
pub type ProvenanceTable = Vec<(String, Origin)>;

#[derive(Debug)]
pub struct ResolvedInstance {
    /// Invariant: exactly one entry per field name — replace in place at
    /// the base entry's position, never append-and-shadow.
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
    };
    // The declaring clause's own site check + listed-ref resolution.
    let site = grants.grant_for(declaring.source_path);
    if let Some(d) = deny_diagnostic(&site, declaring, declaring_block) {
        diags.push(d);
        return (None, diags);
    }
    // Site-check the declaring clause's listed refs against the root grant.
    let mut site_ok = true;
    if let GrantLookup::Granted { grant, binding, .. } = &site {
        for r in refs {
            let decision = grants.ref_decision(grant, r.source_path);
            if let Some(d) = ref_denial(decision, r.name, binding, Denial::Site) {
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
    if let GrantLookup::Granted { grant, binding, .. } = &site {
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
                binding,
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

    // Step 3: normalize each layer against its own document — array-ref
    // inlining first (it can itself introduce a discriminator), then the
    // discriminator pre-pass over the inlined bodies (the stack's
    // effective arm at each oneof position is computed BEFORE per-layer
    // materialization — RFC 0019 step 3), then per-layer normalization
    // against the plan.
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
    let plan = build_arm_plan(index, root, &inlined);
    let layers: Vec<(InstanceId<'_>, Body)> = inlined
        .iter()
        .map(|(id, b)| {
            (
                *id,
                normalize_inlined(index, root, b, &plan, id.source_path, &mut diags),
            )
        })
        .collect();

    // Step 4: compose, replaying the pre-pass's arm decisions.
    let mut merger = Merger {
        index,
        diags: &mut diags,
        origins: Vec::new(),
        plan: &plan,
    };
    let body = merger.merge_root(root, &layers);
    let origins = merger.origins;
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
            diagnostics.push(schema_def_uses_denial(block, source_path));
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
                    diagnostics.push(
                        unresolved_ref(&instances, &block.name.name, &block.keyword.name, &r.name)
                            .with_span(r.span)
                            .with_source(source_path.to_string()),
                    );
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
            Some(r) => composed.push((decl_idx, r.body)),
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
    }
}

// ───────────────────────────────────────────────────────────── merging ──

/// One layer's contribution to one field: the entry plus where it came from.
struct Contribution<'a> {
    layer: InstanceId<'a>,
    entry: BodyEntry,
}

struct Merger<'a, 'd> {
    index: &'a SchemaIndex,
    diags: &'d mut Vec<Diagnostic>,
    origins: ProvenanceTable,
    /// The pre-pass's arm decisions — replayed, never re-derived, at
    /// every planned oneof position.
    plan: &'d ArmPlan<'a>,
}

impl<'a, 'd> Merger<'a, 'd> {
    fn record(&mut self, path: &str, layer: InstanceId<'_>, span: Span) {
        self.origins.push((
            path.to_string(),
            Origin::File {
                file: layer.source_path.into(),
                span,
            },
        ));
    }

    fn merge_root(&mut self, root: &str, layers: &[(InstanceId<'a>, Body)]) -> Body {
        if let Some(oneof) = self.index.oneof(root) {
            let oneof = oneof.clone();
            return self.merge_oneof_bodies("", &oneof, layers);
        }
        let model = self.index.model(root).cloned();
        self.merge_model_bodies("", model.as_ref(), layers)
    }

    /// Merge bodies against a model: one entry per field name, replace in
    /// place at the base entry's position (`Body::with_entries` on the
    /// establishing layer's body — the receiver carries the annotation).
    fn merge_model_bodies(
        &mut self,
        path: &str,
        model: Option<&ModelDef>,
        layers: &[(InstanceId<'a>, Body)],
    ) -> Body {
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
        // pass-through kinds keep the lowest layer's copy.
        let mut order: Vec<String> = Vec::new();
        let mut by_name: HashMap<String, Vec<Contribution<'a>>> = HashMap::new();
        let mut passthrough: Vec<(InstanceId<'a>, BodyEntry)> = Vec::new();
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
                    if !by_name.contains_key(&name) {
                        order.push(name.clone());
                    }
                    by_name.entry(name).or_default().push(Contribution {
                        layer: *layer,
                        entry: entry.clone(),
                    });
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
            let contributions = &by_name[name];
            // Undeclared modifier groups keep their `|` key, which can
            // never match a field name (`|` is not an identifier
            // character) — so a plain map lookup is total; the old
            // `"|" + f.name == name` disjunct was unreachable once the
            // gather canonicalized declared modifiers.
            let field = field_map.get(name.as_str()).copied();
            if let Some(entry) = self.merge_field(path, name, field, contributions) {
                entries.push(entry);
            }
        }
        // Arm-set fields compose by overlay: the highest layer that states
        // arms replaces the whole set. The seal backstop for displaced arm
        // bodies binds through the arm target's typed body, which arrives
        // with union compose (arm targets are consumer-typed references in
        // this slice, so there is no schema-named sealed surface to scan
        // yet — the replacement itself is loud in `nml resolve`).
        if let Some((_, arms)) = arm_sets.last() {
            for entry in arms {
                entries.push(entry.clone());
            }
        }
        for (_, entry) in passthrough {
            entries.push(entry);
        }
        establishing.with_entries(entries)
    }

    /// Merge one named field across layers per its policy.
    fn merge_field(
        &mut self,
        path: &str,
        name: &str,
        field: Option<&FieldDef>,
        contributions: &[Contribution<'a>],
    ) -> Option<BodyEntry> {
        let policy = field.map(policy_of).unwrap_or_default();
        let field_path = join_path(path, name);
        // All-modifier groups route by KIND first (modifier output shape,
        // TypeAnnotation passthrough); a group MIXING spellings of one
        // declared field routes by policy below — `items_of` gives every
        // merge path one view of the items regardless of spelling.
        if contributions
            .iter()
            .all(|c| matches!(c.entry.kind, BodyEntryKind::Modifier(_)))
        {
            return match policy {
                MergePolicy::Sealed => self.merge_sealed(&field_path, field, contributions),
                _ => self.merge_modifier(&field_path, policy, field, contributions),
            };
        }
        match policy {
            MergePolicy::Sealed => self.merge_sealed(&field_path, field, contributions),
            MergePolicy::Identity | MergePolicy::Append | MergePolicy::IdentityAppend => {
                self.merge_list(&field_path, policy, field, contributions)
            }
            MergePolicy::Overlay => self.merge_overlay(&field_path, field, contributions),
        }
    }

    /// `#sealed`: write-once from the bottom. Any higher assignment is
    /// NML2060 — even at the identical value (with a machine-applicable
    /// deletion suggestion; on a sealed field this takes precedence over
    /// NML2084).
    fn merge_sealed(
        &mut self,
        path: &str,
        field: Option<&FieldDef>,
        contributions: &[Contribution<'a>],
    ) -> Option<BodyEntry> {
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
            let last = contributions.last()?;
            self.record(path, last.layer, last.entry.span);
            return Some(last.entry.clone());
        };
        let (first, rest) = contributions[pos..]
            .split_first()
            .expect("position yields a non-empty tail");
        for c in rest {
            if !is_write(&c.entry.kind) {
                continue;
            }
            let equal = match (&first.entry.kind, &c.entry.kind) {
                (BodyEntryKind::Property(a), BodyEntryKind::Property(b)) => {
                    a.value.value.semantic_eq(&b.value.value)
                }
                _ => false,
            };
            let mut d = if equal {
                Diagnostic::error(format!(
                    "'{path}' is already sealed to this same value by a \
                     lower layer; restating it would silently decouple if \
                     the base changes — delete this assignment"
                ))
                .with_suggestion("", c.entry.span)
            } else {
                Diagnostic::error(format!(
                    "assignment to `#sealed` field '{path}' — a lower layer \
                     already fixed it"
                ))
            };
            d = d
                .with_code(codes::SEALED_FIELD_VIOLATION)
                .with_span(c.entry.span)
                .with_source(c.layer.source_path.to_string())
                .with_related(first.entry.span, "sealed here".to_string());
            self.diags.push(d);
        }
        self.record(path, first.layer, first.entry.span);
        Some(first.entry.clone())
    }

    /// Overlay: scalars replace (later wins, NML2084 on a dead delta);
    /// nested blocks deep-merge recursively — with variant identity
    /// composing before fields for oneof-typed positions.
    fn merge_overlay(
        &mut self,
        path: &str,
        field: Option<&FieldDef>,
        contributions: &[Contribution<'a>],
    ) -> Option<BodyEntry> {
        let target = field.map(|f| effective_type(&f.field_type));
        if let Some(FieldType::List(_) | FieldType::Set(_)) = target {
            // Bare-overlay list, across EVERY spelling (`items_of` sees
            // block, array-property, and modifier forms as one): the
            // highest layer that SUPPLIES items (≥1 after normalization)
            // replaces wholesale; zero-item entries were already warned
            // and are no-ops — an escaped-normalization `xs = []` must
            // never EMPTY the base list through the scalar-overlay path.
            // When NO layer supplies items, the field survives
            // authored-empty rather than dropping (a valid inherited
            // `xs = []` must not turn into a missing-required error).
            let winner = contributions
                .iter()
                .rev()
                .find(|c| items_of(&c.entry.kind).is_some_and(|v| !v.is_empty()))
                .or_else(|| {
                    contributions
                        .iter()
                        .rev()
                        .find(|c| items_of(&c.entry.kind).is_some())
                })
                .or_else(|| contributions.last())?;
            self.record(path, winner.layer, winner.entry.span);
            return Some(winner.entry.clone());
        }
        // An object-typed field (model- or oneof-ModelRef) ALWAYS
        // deep-merges its nested contributions — the RFC's "nested blocks
        // always deep-merge, there is no whole-object replacement form,
        // which is what closes object-level seal laundering". Routing by
        // whether EVERY contribution is nested let a scalar/modifier
        // spelling (`cfg = "gone"`, invalid for an object field) drop the
        // group into scalar-overlay and discard a sealed nested body.
        // Route by TARGET instead: gather the nested bodies, deep-merge
        // them, and let any non-object spelling surface as an ordinary
        // type error on the resolved body — it can never win the field.
        if let Some(FieldType::ModelRef(type_name)) = target {
            let nested: Vec<(InstanceId<'a>, &NestedBlock)> = contributions
                .iter()
                .filter_map(|c| match &c.entry.kind {
                    BodyEntryKind::NestedBlock(nb) => Some((c.layer, nb)),
                    _ => None,
                })
                .collect();
            let is_object =
                self.index.model(type_name).is_some() || self.index.oneof(type_name).is_some();
            if is_object && !nested.is_empty() {
                let sub: Vec<(InstanceId<'a>, Body)> =
                    nested.iter().map(|(l, nb)| (*l, nb.body.clone())).collect();
                let (base_layer, base_nb) = nested[0];
                let merged = if let Some(oneof) = self.index.oneof(type_name) {
                    let oneof = oneof.clone();
                    self.merge_oneof_bodies(path, &oneof, &sub)
                } else {
                    let model = self.index.model(type_name).cloned();
                    self.merge_model_bodies(path, model.as_ref(), &sub)
                };
                self.record(path, base_layer, base_nb.name.span);
                return Some(BodyEntry {
                    span: base_nb.name.span,
                    kind: BodyEntryKind::NestedBlock(NestedBlock {
                        name: base_nb.name.clone(),
                        body: merged,
                    }),
                });
            }
        }
        // Scalar overlay: later wins; a dead delta warns (NML2084).
        let winner = contributions.last()?;
        for pair in contributions.windows(2) {
            if let (BodyEntryKind::Property(lo), BodyEntryKind::Property(hi)) =
                (&pair[0].entry.kind, &pair[1].entry.kind)
            {
                if lo.value.value.semantic_eq(&hi.value.value) {
                    self.diags.push(
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
        Some(winner.entry.clone())
    }

    /// The element targets of a list-typed field: model elements
    /// deep-merge item bodies; a oneof ELEMENT makes each identity-matched
    /// item group its own variant scope — merging those bodies model-less
    /// would skip the arm accumulator entirely (no seal enforcement, no
    /// backstop, silent cross-arm merges). One resolver for every list
    /// spelling: the modifier route dodging it was a seal escape.
    fn item_targets(&self, field: Option<&FieldDef>) -> (Option<ModelDef>, Option<OneOfDef>) {
        let inner_name = field
            .and_then(|f| list_inner(effective_type(&f.field_type)))
            .and_then(|inner| match inner {
                FieldType::ModelRef(n) => Some(n.as_str()),
                _ => None,
            });
        (
            inner_name.and_then(|n| self.index.model(n)).cloned(),
            inner_name.and_then(|n| self.index.oneof(n)).cloned(),
        )
    }

    fn merge_modifier(
        &mut self,
        path: &str,
        policy: MergePolicy,
        field: Option<&FieldDef>,
        contributions: &[Contribution<'a>],
    ) -> Option<BodyEntry> {
        // A modifier without a list-policy grant composes like a bare list:
        // the highest layer that supplies items replaces wholesale (its
        // items are authored, not overridden). Only granted policies merge.
        if policy == MergePolicy::Overlay {
            // Same zero-item contract as bare lists: an empty overlay
            // modifier must never EMPTY the base's list (a security-shaped
            // allow-by-emptying); it is a warned no-op, and only an
            // item-bearing layer replaces.
            let winner = contributions
                .iter()
                .rev()
                .find(|c| {
                    matches!(&c.entry.kind, BodyEntryKind::Modifier(m)
                        if !matches!(m.value, ModifierValue::TypeAnnotation { .. }))
                        && !is_zero_item_entry(&c.entry.kind)
                })
                .or_else(|| {
                    contributions.iter().rev().find(|c| {
                        matches!(&c.entry.kind, BodyEntryKind::Modifier(m)
                            if !matches!(m.value, ModifierValue::TypeAnnotation { .. }))
                    })
                })?;
            self.record(path, winner.layer, winner.entry.span);
            return Some(winner.entry.clone());
        }
        // One tuple per LAYER, its entries' items concatenated in document
        // order — per-contribution tuples would let a layer stating the
        // field twice dodge the within-layer duplicate-identity check and
        // misread its own later entries as another layer's overlay.
        let mut items_per_layer: Vec<(InstanceId<'a>, Span, &Identifier, Vec<ListItem>)> =
            Vec::new();
        for c in contributions {
            let BodyEntryKind::Modifier(m) = &c.entry.kind else {
                continue;
            };
            let Some(items) = items_of(&c.entry.kind) else {
                continue;
            };
            match items_per_layer.last_mut() {
                Some((l, _, _, acc)) if *l == c.layer => acc.extend(items),
                _ => items_per_layer.push((c.layer, c.entry.span, &m.name, items)),
            }
        }
        let (base_layer, base_span, name, _) = items_per_layer.first()?;
        let (item_model, item_oneof) = self.item_targets(field);
        let merged = self.merge_items(
            path,
            policy,
            item_model.as_ref(),
            item_oneof.as_ref(),
            &items_per_layer
                .iter()
                .map(|(l, s, _, items)| (*l, *s, items.clone()))
                .collect::<Vec<_>>(),
        );
        self.record(path, *base_layer, *base_span);
        Some(BodyEntry {
            span: *base_span,
            kind: BodyEntryKind::Modifier(Modifier {
                name: (*name).clone(),
                value: ModifierValue::Block(merged),
            }),
        })
    }

    /// List-policy merge over per-layer item vectors.
    fn merge_list(
        &mut self,
        path: &str,
        policy: MergePolicy,
        field: Option<&FieldDef>,
        contributions: &[Contribution<'a>],
    ) -> Option<BodyEntry> {
        let (item_model, item_oneof) = self.item_targets(field);
        // One tuple per LAYER (same-layer entries concatenate in document
        // order — the within-layer duplicate check must see the whole
        // layer), extracting items across EVERY spelling via `items_of`:
        // block, escaped-normalization array property (a spelling gap must
        // merge, never vanish), and modifier entries mixed with the
        // others of one declared field.
        let mut per_layer: Vec<(InstanceId<'a>, Span, Vec<ListItem>)> = Vec::new();
        for c in contributions {
            let Some(items) = items_of(&c.entry.kind) else {
                continue;
            };
            match per_layer.last_mut() {
                Some((l, _, acc)) if *l == c.layer => acc.extend(items),
                _ => per_layer.push((c.layer, c.entry.span, items)),
            }
        }
        let base = contributions.first()?;
        let name = match &base.entry.kind {
            BodyEntryKind::NestedBlock(nb) => nb.name.clone(),
            BodyEntryKind::Property(p) => p.name.clone(),
            BodyEntryKind::Modifier(m) => m.name.clone(),
            _ => return None,
        };
        let merged = self.merge_items(
            path,
            policy,
            item_model.as_ref(),
            item_oneof.as_ref(),
            &per_layer,
        );
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
        Some(BodyEntry {
            span: base.entry.span,
            kind,
        })
    }

    fn merge_items(
        &mut self,
        path: &str,
        policy: MergePolicy,
        item_model: Option<&ModelDef>,
        item_oneof: Option<&OneOfDef>,
        per_layer: &[(InstanceId<'a>, Span, Vec<ListItem>)],
    ) -> Vec<ListItem> {
        // Identity-matched item bodies merge per the element's kind: a
        // oneof element routes through the arm accumulator (seal
        // enforcement, backstop), a model element deep-merges.
        let merge_item_bodies =
            |me: &mut Self, item_path: &str, sub: &[(InstanceId<'a>, Body)]| match item_oneof {
                Some(oneof) => me.merge_oneof_bodies(item_path, oneof, sub),
                None => me.merge_model_bodies(item_path, item_model, sub),
            };
        // Set dedupe rides resolved-instance validation (NML2030); the
        // merge itself is shape-agnostic.
        let mut resolved: Vec<(ItemKey, ListItem, InstanceId<'a>)> = Vec::new();
        // Token-prehash index over `resolved` (kind-blind, so same-kind
        // and cross-kind candidates share a bucket; exactness re-verified
        // inside) — the per-item linear scans were O(items²), an editor
        // and CLI DoS axis on large lists. Replacements keep their key,
        // so only pushes index.
        let mut resolved_index: HashMap<u64, Vec<usize>> = HashMap::new();
        let push_resolved = |resolved: &mut Vec<(ItemKey, ListItem, InstanceId<'a>)>,
                             index: &mut HashMap<u64, Vec<usize>>,
                             key: ItemKey,
                             item: ListItem,
                             layer: InstanceId<'a>| {
            index
                .entry(token_prehash(&key))
                .or_default()
                .push(resolved.len());
            resolved.push((key, item, layer));
        };
        // NML2067 fires only once some lower layer actually SUPPLIED items
        // (≥1) — a zero-item base entry neither supplies nor establishes
        // the list (NML2079's contract), so it must not turn every higher
        // tier's first real items into "unmatched overlays".
        let mut base_established = false;
        for (layer, _span, items) in per_layer.iter() {
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
                    self.diags.push(
                        Diagnostic::error(format!(
                            "duplicate identity in one layer's '{path}' list — \
                             the merge key must be unique before it can be \
                             merged on; delete the duplicate"
                        ))
                        .with_code(codes::IDENTITY_REDEFINITION)
                        .with_span(item.span)
                        .with_source(layer.source_path.to_string()),
                    );
                    continue;
                }
                seen_this_layer
                    .entry(prehash)
                    .or_default()
                    .push(key.clone());
                // Same-kind identity first: a base may legally hold a
                // shorthand "a" AND a named a: (the within-layer duplicate
                // check keys on kind+token), so a first-token-equal lookup
                // would bind an overlay's named a to the SCALAR entry and
                // misfire NML2063 while its true merge partner sits right
                // there. Cross-kind token collision is only the verdict
                // when no same-kind partner exists. Bucketed lookup: the
                // kind-blind prehash keeps both passes in one bucket.
                let bucket: &[usize] = resolved_index
                    .get(&prehash)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                let existing = bucket
                    .iter()
                    .copied()
                    .find(|&i| resolved[i].0.same(&key))
                    .or_else(|| {
                        bucket
                            .iter()
                            .copied()
                            .find(|&i| resolved[i].0.token_eq(&key))
                    });
                match existing {
                    None => {
                        if base_established && policy == MergePolicy::Identity {
                            // Unmatched overlay item without #append.
                            let mut msg = format!(
                                "item matches no base identity in '{path}' \
                                 (the schema grants overriding, not adding)"
                            );
                            if let Some(hint) = self.named_hint(&key, &resolved) {
                                msg.push_str(&format!(" — did you mean '{hint}'?"));
                            }
                            self.diags.push(
                                Diagnostic::error(msg)
                                    .with_code(codes::UNMATCHED_OVERLAY_ITEM)
                                    .with_span(item.span)
                                    .with_source(layer.source_path.to_string()),
                            );
                            continue;
                        }
                        push_resolved(
                            &mut resolved,
                            &mut resolved_index,
                            key,
                            item.clone(),
                            *layer,
                        );
                    }
                    Some(pos) => {
                        let (existing_key, existing_item, existing_layer) = &resolved[pos];
                        if !base_established {
                            // Base-internal duplicates are the base's own
                            // business at merge time (identity-keyed dupes
                            // were already diagnosed above; scalar dupes are
                            // ordinary list semantics).
                            push_resolved(
                                &mut resolved,
                                &mut resolved_index,
                                key,
                                item.clone(),
                                *layer,
                            );
                            continue;
                        }
                        // Cross-kind match at an equal token: NML2063 under
                        // every identity-keyed policy.
                        if !existing_key.same_kind(&key) {
                            self.diags.push(
                                Diagnostic::error(format!(
                                    "item matches an existing identity in \
                                     '{path}' across item kinds — match the \
                                     base's spelling"
                                ))
                                .with_code(codes::IDENTITY_REDEFINITION)
                                .with_span(item.span)
                                .with_source(layer.source_path.to_string()),
                            );
                            continue;
                        }
                        match policy {
                            MergePolicy::Append => {
                                if existing_key.is_scalar() {
                                    // Scalar concatenation: duplicates legal.
                                    push_resolved(
                                        &mut resolved,
                                        &mut resolved_index,
                                        key,
                                        item.clone(),
                                        *layer,
                                    );
                                } else {
                                    self.diags.push(
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
                                }
                            }
                            MergePolicy::Identity | MergePolicy::IdentityAppend => {
                                match (&existing_item.kind, &item.kind) {
                                    (
                                        ListItemKind::Named { name, body: lo },
                                        ListItemKind::Named { body: hi, .. },
                                    ) => {
                                        let sub = vec![
                                            (*existing_layer, lo.clone()),
                                            (*layer, hi.clone()),
                                        ];
                                        let item_path = format!("{path}[{}]", name.name);
                                        let merged_body = merge_item_bodies(self, &item_path, &sub);
                                        let span = item.span;
                                        resolved[pos] = (
                                            existing_key.clone(),
                                            ListItem {
                                                kind: ListItemKind::Named {
                                                    name: name.clone(),
                                                    body: merged_body,
                                                },
                                                span,
                                            },
                                            *existing_layer,
                                        );
                                    }
                                    (
                                        ListItemKind::Shorthand { value, body: lo },
                                        ListItemKind::Shorthand { body: hi, .. },
                                    ) => {
                                        // Bodiless base merges as an empty
                                        // body; bodiless upper is a no-op
                                        // restatement.
                                        let lo_body =
                                            lo.clone().unwrap_or_else(|| Body::fresh(vec![]));
                                        let mut hi_body =
                                            hi.clone().unwrap_or_else(|| Body::fresh(vec![]));
                                        // The positionalizer materializes
                                        // the identity token into BOTH
                                        // layers' bodies — that token IS
                                        // the merge key, so the upper
                                        // copy is pairing machinery, not
                                        // an authored restatement: strip
                                        // it, or every scalar-keyed
                                        // identity merge draws a spurious
                                        // NML2084 dead-delta.
                                        let shorthand_field = item_model
                                            .and_then(|m| m.fields.iter().find(|f| f.shorthand))
                                            .or_else(|| {
                                                // A oneof element's `+`
                                                // field lives on its
                                                // effective arm — the
                                                // bodies' stated disc,
                                                // else the default,
                                                // mirroring the
                                                // materialization that
                                                // injected the token.
                                                item_oneof.and_then(|o| {
                                                    let stated = [&lo_body, &hi_body]
                                                        .into_iter()
                                                        .find_map(|b| {
                                                            b.entries.iter().find_map(|e| match &e
                                                                .kind
                                                            {
                                                                BodyEntryKind::Property(p)
                                                                    if p.name.name
                                                                        == o.discriminator =>
                                                                {
                                                                    p.value.value.as_str()
                                                                }
                                                                _ => None,
                                                            })
                                                        });
                                                    let disc = stated
                                                        .or(o.default_discriminator.as_deref())?;
                                                    variant_model_of(self.index, o, disc).and_then(
                                                        |m| m.fields.iter().find(|f| f.shorthand),
                                                    )
                                                })
                                            });
                                        if let Some(tok) = shorthand_field {
                                            let entries = hi_body
                                                .entries
                                                .iter()
                                                .filter(|e| {
                                                    !matches!(&e.kind,
                                                        BodyEntryKind::Property(p)
                                                            if p.name.name == tok.name
                                                                && p.value.value
                                                                    .semantic_eq(&value.value))
                                                })
                                                .cloned()
                                                .collect();
                                            hi_body = hi_body.with_entries(entries);
                                        }
                                        let sub =
                                            vec![(*existing_layer, lo_body), (*layer, hi_body)];
                                        let item_path =
                                            format!("{path}[{}]", value.value.type_name());
                                        let merged_body = merge_item_bodies(self, &item_path, &sub);
                                        resolved[pos] = (
                                            existing_key.clone(),
                                            ListItem {
                                                kind: ListItemKind::Shorthand {
                                                    value: value.clone(),
                                                    body: Some(merged_body),
                                                },
                                                span: item.span,
                                            },
                                            *existing_layer,
                                        );
                                    }
                                    // Reference / role items are immutable:
                                    // identical restatement is a no-op.
                                    (ListItemKind::Reference(_), ListItemKind::Reference(_))
                                    | (ListItemKind::Role(_), ListItemKind::Role(_)) => {}
                                    _ => {
                                        self.diags.push(
                                            Diagnostic::error(format!(
                                                "item redefines a bodiless \
                                                 (reference/role) identity \
                                                 in '{path}' — those items \
                                                 are immutable under \
                                                 `#identity`"
                                            ))
                                            .with_code(codes::IDENTITY_REDEFINITION)
                                            .with_span(item.span)
                                            .with_source(layer.source_path.to_string()),
                                        );
                                    }
                                }
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
        for (key, item, layer) in &resolved {
            if let ItemKey::Named(n) = key {
                self.record(&format!("{path}[{n}]"), *layer, item.span);
            }
        }
        resolved.into_iter().map(|(_, item, _)| item).collect()
    }

    fn named_hint(
        &self,
        key: &ItemKey,
        resolved: &[(ItemKey, ListItem, InstanceId<'a>)],
    ) -> Option<String> {
        // NML2067's did-you-mean discloses NAMED identities only —
        // scalar-keyed tokens are values and are never echoed.
        let ItemKey::Named(input) = key else {
            return None;
        };
        let names: Vec<&str> = resolved
            .iter()
            .filter_map(|(k, _, _)| match k {
                ItemKey::Named(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        crate::suggest::suggest(input, names.iter().copied()).map(|s| s.to_string())
    }

    /// Oneof compose: the effective arm accumulates bottom-up from the
    /// schema default; omission inherits; a stated equal value deep-merges;
    /// a stated different value switches — wholesale, subject to the seal
    /// backstop (a discarded body with an assigned `#sealed` field, at any
    /// depth, is NML2060). The DECISIONS come from the pre-pass's fold —
    /// replayed at planned positions, derived locally through the SAME
    /// fold at unplanned ones (item scopes) — so this function never
    /// re-judges an arm the plan already judged: two accumulators over
    /// two body representations is how machinery injected under one arm
    /// fabricated refusals against another.
    fn merge_oneof_bodies(
        &mut self,
        path: &str,
        oneof: &OneOfDef,
        layers: &[(InstanceId<'a>, Body)],
    ) -> Body {
        // A planned trace is replayed POSITIONALLY, and only when it
        // aligns entry-for-entry with the contributions being merged (one
        // layer may state a oneof field twice — two contributions, two
        // decisions, ONE id — so an id-keyed lookup would collapse them
        // and misapply the first decision to both). Any structural
        // mismatch (membership drift, count drift) falls back to a local
        // fold over these bodies: recomputing is always safe; misapplying
        // a stale decision is how a switch silently sticks or a sealed
        // write silently vanishes.
        let owned_trace;
        let planned = self.plan.decisions.get(path).filter(|t| {
            t.len() == layers.len() && t.iter().zip(layers).all(|((tid, _), (lid, _))| tid == lid)
        });
        let trace: &[(InstanceId<'a>, ArmDecision<'a>)] = match planned {
            Some(t) => t,
            None => {
                let refs: Vec<(InstanceId<'a>, &Body)> =
                    layers.iter().map(|(l, b)| (*l, b)).collect();
                owned_trace = fold_arm_checked(self.index, oneof, &refs).1;
                &owned_trace
            }
        };
        let mut effective: Option<String> = oneof.default_discriminator.clone();
        let mut group: Vec<(InstanceId<'a>, Body)> = Vec::new();
        let mut disc_entry: Option<(InstanceId<'a>, BodyEntry)> = None;
        for (idx, (layer, body)) in layers.iter().enumerate() {
            let stated_entry = stated_discriminator_entry(body, &oneof.discriminator);
            // Positional: the trace was folded over these exact entries in
            // this exact order (alignment was checked above; the local
            // fold produces one decision per entry by construction).
            let decision = trace.get(idx).map(|(_, d)| d).unwrap_or(&ArmDecision::Join);
            match decision {
                ArmDecision::Rejected { seals } => {
                    let (seal_path, seal_span, seal_layer) = seals
                        .first()
                        .cloned()
                        .expect("a rejection records at least one seal");
                    let mut msg = format!(
                        "arm switch on '{}' would discard the assigned \
                         `#sealed` field '{}' — replacement cannot launder \
                         a seal",
                        oneof.discriminator,
                        join_path(path, &seal_path)
                    );
                    if seals.len() > 1 {
                        msg.push_str(&format!(
                            " ({} sealed fields would be discarded)",
                            seals.len()
                        ));
                    }
                    let span = stated_entry.map(|e| e.span).unwrap_or(seal_span);
                    self.diags.push(
                        Diagnostic::error(msg)
                            .with_code(codes::SEALED_FIELD_VIOLATION)
                            .with_span(span)
                            .with_source(layer.source_path.to_string())
                            .with_related(
                                seal_span,
                                format!("sealed here (in {})", seal_layer.source_path),
                            ),
                    );
                    // Switch rejected: this layer contributes nothing.
                }
                ArmDecision::Switch => {
                    if let Some(entry) = stated_entry {
                        if let BodyEntryKind::Property(p) = &entry.kind {
                            if let Value::String(s) = &p.value.value {
                                effective = Some(s.clone());
                            }
                        }
                        disc_entry = Some((*layer, entry.clone()));
                    }
                    group.clear();
                    group.push((*layer, body.clone()));
                }
                ArmDecision::Join => {
                    // Omitted or restated-at-effective: deep-merge into
                    // the effective arm.
                    if let Some(entry) = stated_entry {
                        if disc_entry.is_none() {
                            disc_entry = Some((*layer, entry.clone()));
                        }
                    }
                    group.push((*layer, body.clone()));
                }
            }
        }
        let arm_model = effective
            .as_ref()
            .and_then(|d| self.variant_model(oneof, d));
        // Strip discriminator entries from the group (the accumulator owns
        // the discriminator; one canonical entry is re-added below).
        let stripped: Vec<(InstanceId<'a>, Body)> = group
            .iter()
            .map(|(l, b)| {
                (
                    *l,
                    b.with_entries(
                        b.entries
                            .iter()
                            .filter(|e| {
                                !matches!(&e.kind, BodyEntryKind::Property(p)
                                    if p.name.name == oneof.discriminator
                                        && matches!(p.value.value, Value::String(_)))
                            })
                            .cloned()
                            .collect(),
                    ),
                )
            })
            .collect();
        let mut merged = self.merge_model_bodies(path, arm_model.as_ref(), &stripped);
        if let Some((layer, entry)) = disc_entry {
            let disc_path = join_path(path, &oneof.discriminator);
            self.record(&disc_path, layer, entry.span);
            let mut entries = vec![entry];
            entries.extend(merged.entries.iter().cloned());
            merged = merged.with_entries(entries);
        }
        merged
    }

    fn variant_model(&self, oneof: &OneOfDef, discriminator: &str) -> Option<ModelDef> {
        variant_model_of(self.index, oneof, discriminator).cloned()
    }
}

/// Every assigned `#sealed` field a displaced group of bodies carries,
/// validated against the `vocab` candidate models, at any depth —
/// deduplicated by field path, in lowest-layer-then-first-in-document
/// order (so `.first()` is the RFC's related span and `.len()` is its
/// count). "Assigned" carries the engine's own write semantics
/// ([`seal_write`]): a zero-item entry on a LIST-shaped sealed field is
/// not a write and must not block a legal arm switch. Oneof-typed nested
/// positions widen the vocabulary to every arm the group could have made
/// effective (the schema default plus each discriminator value the group
/// states) — fail-closed: a seal the nested accumulator would have
/// preserved (including via its own backstop rejecting an inner switch)
/// is never missed, at the price of a rare cross-arm name-collision
/// over-report, pre-warned at schema load by NML2076. Each body node is
/// scanned exactly ONCE against the union vocabulary — recursing once
/// per candidate arm instead would be exponential in nesting depth over
/// a recursive oneof, a DoS from kilobytes of hostile input.
///
/// A free function (not a `Merger` method) because the discriminator
/// pre-pass consults the same scan to mirror the backstop's decisions.
fn assigned_seals_over<'a>(
    index: &SchemaIndex,
    path: &str,
    vocab: &[&ModelDef],
    group: &[(InstanceId<'a>, &Body)],
) -> Vec<(String, Span, InstanceId<'a>)> {
    let mut out = Vec::new();
    for (layer, body) in group {
        seal_scan_body(index, path, vocab, body, group, *layer, &mut out);
    }
    out
}

/// Add `m` to a candidate vocabulary unless a same-named model is present.
fn push_model<'i>(vocab: &mut Vec<&'i ModelDef>, m: &'i ModelDef) {
    if !vocab.iter().any(|x| x.name == m.name) {
        vocab.push(m);
    }
}

fn seal_scan_body<'a>(
    index: &SchemaIndex,
    path: &str,
    vocab: &[&ModelDef],
    body: &Body,
    siblings: &[(InstanceId<'a>, &Body)],
    layer: InstanceId<'a>,
    out: &mut Vec<(String, Span, InstanceId<'a>)>,
) {
    // Dedup by (path, span): the same assignment re-encountered is one
    // finding, but two DISTINCT assignments sharing a non-disclosing path
    // (two scalar-keyed items both render `xs[string]`) are two discarded
    // seals — collapsing them under-reports the RFC's count.
    let hit = |out: &mut Vec<(String, Span, InstanceId<'a>)>, p: String, span: Span| {
        if !out.iter().any(|(q, s, _)| *q == p && *s == span) {
            out.push((p, span, layer));
        }
    };
    // One name→fields map per scan level (the per-entry vocab scan was
    // the same quadratic width axis as the merge's field lookup).
    let mut field_map: HashMap<&str, Vec<&FieldDef>> = HashMap::new();
    for m in vocab {
        for f in &m.fields {
            field_map.entry(f.name.as_str()).or_default().push(f);
        }
    }
    let lookup =
        |name: &str| -> &[&FieldDef] { field_map.get(name).map(|v| v.as_slice()).unwrap_or(&[]) };
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::Property(p) => {
                for f in lookup(&p.name.name) {
                    if policy_of(f) == MergePolicy::Sealed && seal_write(f, &entry.kind) {
                        hit(out, join_path(path, &f.name), entry.span);
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
                    hit(out, join_path(path, &m.name.name), entry.span);
                    continue;
                }
                // A modifier's ITEMS are list items like any other
                // spelling's — their sealed fields must reach the scan, or
                // `|steps:` launders what `steps:` cannot.
                scan_list_items(
                    index,
                    &join_path(path, &m.name.name),
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
                    hit(out, join_path(path, &nb.name.name), entry.span);
                    continue;
                }
                let fpath = join_path(path, &nb.name.name);
                let sibs = sub_bodies_at(siblings, &nb.name.name);
                // Non-list targets: ONE recursion over the union of every
                // candidate field's target vocabulary.
                let mut child: Vec<&ModelDef> = Vec::new();
                for f in fields {
                    if let FieldType::ModelRef(n) = effective_type(&f.field_type) {
                        if let Some(m) = index.model(n) {
                            push_model(&mut child, m);
                        } else if let Some(oneof) = index.oneof(n) {
                            for arm in candidate_arms(oneof, &sibs) {
                                if let Some(am) = variant_model_of(index, oneof, &arm) {
                                    push_model(&mut child, am);
                                }
                            }
                        }
                    }
                }
                if !child.is_empty() {
                    seal_scan_body(index, &fpath, &child, &nb.body, &sibs, layer, out);
                }
                // List targets: "at any depth" includes list items — the
                // laundering vector reopens through a sealed field on an
                // item model otherwise (shared with the modifier spelling
                // above).
                scan_list_items(
                    index,
                    &fpath,
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

/// The item references an entry carries, across the two body-bearing list
/// spellings (block form and block-form modifiers). Inline-array items
/// are bodiless — nothing inside them to scan.
fn item_refs(kind: &BodyEntryKind) -> Vec<&ListItem> {
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

/// All sibling items at field `name`, across BOTH list spellings, in
/// layer-then-document order — the identity-group pool for item scans.
fn sibling_items_at<'a, 'b>(
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
fn token_prehash(key: &ItemKey) -> u64 {
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
fn scan_list_items<'a, 'b>(
    index: &SchemaIndex,
    fpath: &str,
    fields: &[&FieldDef],
    own: &[&'b ListItem],
    pool: &[(InstanceId<'a>, &'b ListItem)],
    layer: InstanceId<'a>,
    out: &mut Vec<(String, Span, InstanceId<'a>)>,
) {
    let list_targets: Vec<&str> = fields
        .iter()
        .filter_map(|f| match list_inner(effective_type(&f.field_type)) {
            Some(FieldType::ModelRef(n)) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    if list_targets.is_empty() || own.is_empty() {
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
        let (seg, item_body) = match &item.kind {
            ListItemKind::Named { name, body } => (name.name.clone(), Some(body)),
            ListItemKind::Shorthand { value, body } => {
                (value.value.type_name().to_string(), body.as_ref())
            }
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
        for n in &list_targets {
            let mut item_vocab: Vec<&ModelDef> = Vec::new();
            if let Some(m) = index.model(n) {
                push_model(&mut item_vocab, m);
            } else if let Some(oneof) = index.oneof(n) {
                for arm in candidate_arms(oneof, &group) {
                    if let Some(am) = variant_model_of(index, oneof, &arm) {
                        push_model(&mut item_vocab, am);
                    }
                }
            }
            if !item_vocab.is_empty() {
                let ipath = format!("{fpath}[{seg}]");
                seal_scan_body(index, &ipath, &item_vocab, b, &group, layer, out);
            }
        }
    }
}

/// List-item identity: the pair (item kind, token). Kinds are part of the
/// key — a cross-kind match at an equal token is NML2063, never a merge.
#[derive(Debug, Clone)]
enum ItemKey {
    Named(String),
    Scalar(Value),
    Reference(String),
    Role(String),
}

impl ItemKey {
    fn of(kind: &ListItemKind) -> Self {
        match kind {
            ListItemKind::Named { name, .. } => ItemKey::Named(name.name.clone()),
            ListItemKind::Shorthand { value, .. } => ItemKey::Scalar(value.value.clone()),
            ListItemKind::Reference(id) => ItemKey::Reference(id.name.clone()),
            ListItemKind::Role(r) => ItemKey::Role(r.clone()),
        }
    }

    /// Same (kind, token) — the full identity.
    fn same(&self, other: &Self) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::Code;

    fn index_from(schema: &str) -> SchemaIndex {
        let mut ex = crate::cst::extract_schema(schema).0;
        crate::schema::resolve_model_inheritance(&mut ex);
        SchemaIndex::build(ex.models, ex.enums, ex.oneofs)
    }

    fn file_of(src: &str) -> File {
        let (file, diags) = crate::cst::parse_to_ast_all(src);
        assert!(diags.is_empty(), "parse diags: {diags:?}");
        file
    }

    /// Compose the named block in `src` under `schema`, open context.
    fn compose(
        schema: &str,
        src: &str,
        root: &str,
        name: &str,
    ) -> (Option<ResolvedInstance>, Vec<Diagnostic>) {
        compose_with(schema, src, root, name, &OpenContext)
    }

    fn compose_with(
        schema: &str,
        src: &str,
        root: &str,
        name: &str,
        grants: &dyn LayerGrantProvider,
    ) -> (Option<ResolvedInstance>, Vec<Diagnostic>) {
        let index = index_from(schema);
        let file = file_of(src);
        let instances = InstanceIndex::from_file("main.nml", &file);
        let declaring = instances.resolve_ref(name).expect("declaring indexed");
        let block = instances.get(declaring).unwrap();
        let refs: Vec<InstanceId> = block
            .uses
            .iter()
            .map(|r| instances.resolve_ref(&r.name).expect("ref resolves"))
            .collect();
        let local = block.body.clone();
        resolve_layers(&index, &instances, declaring, root, &refs, &local, grants)
    }

    fn codes_of(diags: &[Diagnostic]) -> Vec<Code> {
        diags.iter().filter_map(|d| d.code).collect()
    }

    fn scalar<'r>(body: &'r Body, name: &str) -> Option<&'r Value> {
        body.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::Property(p) if p.name.name == name => Some(&p.value.value),
            _ => None,
        })
    }

    fn list_names(body: &Body, field: &str) -> Vec<String> {
        body.entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == field => Some(
                    nb.body
                        .entries
                        .iter()
                        .filter_map(|e| match &e.kind {
                            BodyEntryKind::ListItem(item) => Some(match &item.kind {
                                ListItemKind::Named { name, .. } => name.name.clone(),
                                ListItemKind::Shorthand { value, .. } => {
                                    format!("{:?}", value.value)
                                }
                                ListItemKind::Reference(id) => id.name.clone(),
                                ListItemKind::Role(r) => format!("@{r}"),
                            }),
                            _ => None,
                        })
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default()
    }

    const FLOW_SCHEMA: &str = "\
model step:
    name string+
    action string #sealed
    locator string

model flow:
    entrypoint string #sealed
    steps []step #identity
";

    const SUMMARY: &str = "\
flow memberLookup:
    entrypoint = \"search\"
    steps:
        - search:
            action = \"type\"
            locator = \"#q\"
        - submitSearch:
            action = \"click\"
            locator = \"#submit\"

flow cuXyz uses memberLookup:
    steps:
        - submitSearch:
            locator = \"#search-button\"
";

    // ── the RFC Summary example, end to end ──────────────────────────────

    #[test]
    fn summary_example_composes() {
        let (resolved, diags) = compose(FLOW_SCHEMA, SUMMARY, "flow", "cuXyz");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        assert_eq!(
            scalar(&body, "entrypoint"),
            Some(&Value::String("search".into()))
        );
        assert_eq!(list_names(&body, "steps"), ["search", "submitSearch"]);
        // The overlay re-targeted submitSearch's locator; action stayed.
        let steps = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "steps" => Some(&nb.body),
                _ => None,
            })
            .unwrap();
        let submit = steps
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::ListItem(ListItem {
                    kind: ListItemKind::Named { name, body },
                    ..
                }) if name.name == "submitSearch" => Some(body),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            scalar(submit, "locator"),
            Some(&Value::String("#search-button".into()))
        );
        assert_eq!(
            scalar(submit, "action"),
            Some(&Value::String("click".into()))
        );
    }

    // ── sealed ───────────────────────────────────────────────────────────

    #[test]
    fn sealed_field_violation_differing_value() {
        let src = "\
flow base:
    entrypoint = \"search\"

flow hijacked uses base:
    entrypoint = \"adminPanel\"
";
        let (resolved, diags) = compose(FLOW_SCHEMA, src, "flow", "hijacked");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        // Best-effort body keeps the sealed base value.
        let body = resolved.unwrap().body;
        assert_eq!(
            scalar(&body, "entrypoint"),
            Some(&Value::String("search".into()))
        );
    }

    #[test]
    fn sealed_equal_value_restatement_is_2060_with_deletion_fix() {
        let src = "\
flow base:
    entrypoint = \"search\"

flow copy uses base:
    entrypoint = \"search\"
";
        let (_, diags) = compose(FLOW_SCHEMA, src, "flow", "copy");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        assert!(diags[0].message.contains("same value"));
        let sug = diags[0].suggestions.first().expect("deletion suggestion");
        assert!(sug.replacement.is_empty(), "sole-candidate deletion");
    }

    #[test]
    fn sealed_inside_identity_item_stays_sealed() {
        let src = "\
flow base:
    entrypoint = \"search\"
    steps:
        - search:
            action = \"type\"

flow evil uses base:
    steps:
        - search:
            action = \"transfer\"
";
        let (_, diags) = compose(FLOW_SCHEMA, src, "flow", "evil");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
    }

    // ── identity / append ────────────────────────────────────────────────

    #[test]
    fn unmatched_overlay_item_is_2067_with_named_hint() {
        let src = "\
flow base:
    entrypoint = \"a\"
    steps:
        - submitSearch:
            action = \"click\"

flow t uses base:
    steps:
        - submitSaerch:
            locator = \"#x\"
";
        let (resolved, diags) = compose(FLOW_SCHEMA, src, "flow", "t");
        assert_eq!(codes_of(&diags), [codes::UNMATCHED_OVERLAY_ITEM]);
        assert!(
            diags[0].message.contains("submitSearch"),
            "{}",
            diags[0].message
        );
        // Best-effort: the item was skipped, base list survives.
        assert_eq!(
            list_names(&resolved.unwrap().body, "steps"),
            ["submitSearch"]
        );
    }

    const APPEND_SCHEMA: &str = "\
model step:
    name string+
    action string

model flow:
    steps []step #append
";

    #[test]
    fn append_alone_rejects_redefinition_and_adds_new() {
        let src = "\
flow base:
    steps:
        - audit:
            action = \"log\"

flow t uses base:
    steps:
        - audit:
            action = \"noop\"
        - extra:
            action = \"click\"
";
        let (resolved, diags) = compose(APPEND_SCHEMA, src, "flow", "t");
        assert_eq!(codes_of(&diags), [codes::IDENTITY_REDEFINITION]);
        assert_eq!(
            list_names(&resolved.unwrap().body, "steps"),
            ["audit", "extra"],
            "additions at the back; base immutable"
        );
    }

    const PAIR_SCHEMA: &str = "\
model step:
    name string+
    action string #sealed
    locator string

model flow:
    steps []step #identity #append
";

    #[test]
    fn identity_append_pair_merges_matches_and_appends_rest() {
        let src = "\
flow base:
    steps:
        - search:
            action = \"type\"
            locator = \"#q\"

flow t uses base:
    steps:
        - search:
            locator = \"#q2\"
        - confirm:
            action = \"click\"
            locator = \"#ok\"
";
        let (resolved, diags) = compose(PAIR_SCHEMA, src, "flow", "t");
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(
            list_names(&resolved.unwrap().body, "steps"),
            ["search", "confirm"]
        );
    }

    #[test]
    fn duplicate_identity_within_one_layer_is_2063() {
        let src = "\
flow base:
    steps:
        - search:
            action = \"a\"
        - search:
            action = \"b\"

flow t uses base:
    steps:
        - search:
            locator = \"#x\"
";
        let (_, diags) = compose(PAIR_SCHEMA, src, "flow", "t");
        assert!(codes_of(&diags).contains(&codes::IDENTITY_REDEFINITION));
    }

    // ── zero-item + dead delta ───────────────────────────────────────────

    const DENY_SCHEMA: &str = "\
model policy:
    denyHosts []string #append
    label string
";

    #[test]
    fn spelling_invariance_and_zero_item_warning() {
        let src = "\
policy base:
    denyHosts = [\"a\", \"b\"]
    label = \"x\"

policy mid uses base:
    denyHosts = [\"c\"]

policy top uses mid:
    denyHosts = []
";
        let (resolved, diags) = compose(DENY_SCHEMA, src, "policy", "top");
        assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
        let body = resolved.unwrap().body;
        let items = list_names(&body, "denyHosts");
        assert_eq!(items.len(), 3, "a, b appended with c: {items:?}");
    }

    #[test]
    fn dead_delta_warns_on_overlay_restatement() {
        let src = "\
policy base:
    label = \"x\"

policy t uses base:
    label = \"x\"
";
        let (_, diags) = compose(DENY_SCHEMA, src, "policy", "t");
        assert_eq!(codes_of(&diags), [codes::DEAD_DELTA]);
    }

    // ── linearization ────────────────────────────────────────────────────

    const LIN_SCHEMA: &str = "\
model thing:
    v string
";

    #[test]
    fn c3_redundant_but_legal_order_composes() {
        let src = "\
thing base:
    v = \"b\"

thing mid uses base:
    v = \"m\"

thing top uses base, mid:
    v = \"t\"
";
        let (resolved, diags) = compose(LIN_SCHEMA, src, "thing", "top");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        assert_eq!(scalar(&body, "v"), Some(&Value::String("t".into())));
    }

    #[test]
    fn c3_mirror_order_is_2077() {
        let src = "\
thing base:
    v = \"b\"

thing mid uses base:
    v = \"m\"

thing top uses mid, base:
    v = \"t\"
";
        let (resolved, diags) = compose(LIN_SCHEMA, src, "thing", "top");
        assert!(resolved.is_none());
        assert_eq!(codes_of(&diags), [codes::INCONSISTENT_LINEARIZATION]);
    }

    #[test]
    fn sibling_subtree_contradiction_is_2077() {
        let src = "\
thing slow:
    v = \"s\"

thing fast:
    v = \"f\"

thing vendorX uses slow, fast:
    v = \"x\"

thing productY uses fast, slow:
    v = \"y\"

thing tenant uses vendorX, productY:
    v = \"t\"
";
        let (resolved, diags) = compose(LIN_SCHEMA, src, "thing", "tenant");
        assert!(resolved.is_none());
        assert!(codes_of(&diags).contains(&codes::INCONSISTENT_LINEARIZATION));
    }

    #[test]
    fn diamond_composes_shared_base_once() {
        let src = "\
thing a:
    v = \"a\"

thing b uses a:
    v = \"b\"

thing d uses a:
    v = \"d\"

thing c uses b, d:
    v = \"c\"
";
        let (resolved, diags) = compose(LIN_SCHEMA, src, "thing", "c");
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(
            scalar(&resolved.unwrap().body, "v"),
            Some(&Value::String("c".into()))
        );
    }

    #[test]
    fn cycle_is_2061() {
        let src = "\
thing a uses b:
    v = \"a\"

thing b uses a:
    v = \"b\"
";
        let (resolved, diags) = compose(LIN_SCHEMA, src, "thing", "a");
        assert!(resolved.is_none());
        assert!(codes_of(&diags).contains(&codes::LAYER_CYCLE));
    }

    #[test]
    fn unresolved_ref_is_2059_with_hint() {
        let src = "\
thing base:
    v = \"b\"

thing t uses bsae:
    v = \"t\"
";
        let _ = src;
        let index = index_from(LIN_SCHEMA);
        // A transitive layer's unresolved ref reports NML2059 inside the
        // engine (the declaring clause's own unresolved refs are the
        // caller's to report before calling in):
        let src2 = "\
thing base:
    v = \"b\"

thing mid uses bsae:
    v = \"m\"

thing t uses mid:
    v = \"t\"
";
        let file2 = file_of(src2);
        let instances2 = InstanceIndex::from_file("main.nml", &file2);
        let declaring = instances2.resolve_ref("t").unwrap();
        let block = instances2.get(declaring).unwrap();
        let refs: Vec<InstanceId> = block
            .uses
            .iter()
            .filter_map(|r| instances2.resolve_ref(&r.name))
            .collect();
        let (resolved, diags) = resolve_layers(
            &index,
            &instances2,
            declaring,
            "thing",
            &refs,
            &block.body,
            &OpenContext,
        );
        assert!(resolved.is_none());
        assert!(codes_of(&diags).contains(&codes::UNRESOLVED_LAYER_REF));
        assert!(
            diags.iter().any(|d| d.message.contains("base")),
            "did-you-mean"
        );
    }

    #[test]
    fn keyword_mismatch_is_2062() {
        let schema = "\
model thing:
    v string

model other:
    w string
";
        let src = "\
other base:
    w = \"b\"

thing t uses base:
    v = \"t\"
";
        let (resolved, diags) = compose(schema, src, "thing", "t");
        assert!(resolved.is_none());
        assert!(codes_of(&diags).contains(&codes::LAYER_KEYWORD_MISMATCH));
    }

    #[test]
    fn deep_chain_fails_at_discovery_without_deep_recursion() {
        // A generated 40-link chain must fail at the 16-frame discovery
        // bound — NML2066 from the linearizer, never a stack overflow and
        // never 40 frames of recursion.
        let mut src = String::from("thing l0:\n    v = \"0\"\n");
        for i in 1..=40 {
            src.push_str(&format!("\nthing l{i} uses l{}:\n    v = \"{i}\"\n", i - 1));
        }
        let (resolved, diags) = compose(LIN_SCHEMA, &src, "thing", "l40");
        assert!(resolved.is_none());
        assert!(codes_of(&diags).contains(&codes::LAYER_BOUND_EXCEEDED));
    }

    #[test]
    fn depth_cap_16_is_2066() {
        let mut src = String::from("thing l0:\n    v = \"0\"\n");
        for i in 1..=16 {
            src.push_str(&format!("\nthing l{i} uses l{}:\n    v = \"{i}\"\n", i - 1));
        }
        let (resolved, diags) = compose(LIN_SCHEMA, &src, "thing", "l16");
        assert!(resolved.is_none());
        assert!(codes_of(&diags).contains(&codes::LAYER_BOUND_EXCEEDED));
    }

    // ── grants ───────────────────────────────────────────────────────────

    struct TestGrants {
        lookup: fn(&str) -> GrantLookup<'static>,
    }
    impl LayerGrantProvider for TestGrants {
        fn grant_for(&self, source_path: &str) -> GrantLookup<'_> {
            (self.lookup)(source_path)
        }
        fn ref_decision(&self, grant: &LayerGrant, target_path: &str) -> RefDecision {
            if let Some(i) = grant
                .deny_refs
                .iter()
                .position(|d| target_path.starts_with(d.as_str()))
            {
                return RefDecision::DenyVeto(i);
            }
            if grant
                .allow_refs
                .iter()
                .any(|a| target_path.starts_with(a.as_str()))
            {
                RefDecision::Allowed
            } else {
                RefDecision::AllowMiss
            }
        }
    }

    const BASE_AND_T: &str = "\
thing base:
    v = \"b\"

thing t uses base:
    v = \"t\"
";

    #[test]
    fn no_grant_is_2064_naming_binding_and_manifest() {
        fn lookup(_: &str) -> GrantLookup<'static> {
            GrantLookup::NoGrant {
                binding: "tenantFlows",
                manifest: "nml-package.nml",
            }
        }
        let (resolved, diags) =
            compose_with(LIN_SCHEMA, BASE_AND_T, "thing", "t", &TestGrants { lookup });
        assert!(resolved.is_none());
        assert_eq!(codes_of(&diags), [codes::COMPOSITION_DENIED]);
        assert!(diags[0].message.contains("tenantFlows"));
        assert!(diags[0].message.contains("nml-package.nml"));
        assert!(diags[0].message.contains("nml binding"));
    }

    #[test]
    fn ambiguous_claim_is_2064_naming_both() {
        fn lookup(_: &str) -> GrantLookup<'static> {
            GrantLookup::Ambiguous {
                manifests: vec!["a/nml-package.nml", "b/nml-package.nml"],
            }
        }
        let (resolved, diags) =
            compose_with(LIN_SCHEMA, BASE_AND_T, "thing", "t", &TestGrants { lookup });
        assert!(resolved.is_none());
        assert_eq!(codes_of(&diags), [codes::COMPOSITION_DENIED]);
        assert!(diags[0].message.contains("a/nml-package.nml"));
        assert!(diags[0].message.contains("b/nml-package.nml"));
    }

    #[test]
    fn unbound_closed_is_2064_and_unbound_open_composes() {
        fn closed(_: &str) -> GrantLookup<'static> {
            GrantLookup::Unbound {
                open_context: false,
            }
        }
        let (resolved, diags) = compose_with(
            LIN_SCHEMA,
            BASE_AND_T,
            "thing",
            "t",
            &TestGrants { lookup: closed },
        );
        assert!(resolved.is_none());
        assert_eq!(codes_of(&diags), [codes::COMPOSITION_DENIED]);

        let (resolved, diags) = compose(LIN_SCHEMA, BASE_AND_T, "thing", "t");
        assert!(diags.is_empty(), "{diags:?}");
        assert!(resolved.is_some());
    }

    #[test]
    fn allow_miss_denies_without_naming_path() {
        static GRANT: LayerGrant = LayerGrant {
            allow_refs: Vec::new(),
            deny_refs: Vec::new(),
            max_stack_depth: None,
        };
        fn lookup(_: &str) -> GrantLookup<'static> {
            GrantLookup::Granted {
                grant: &GRANT,
                binding: "tenantFlows",
                manifest: "nml-package.nml",
            }
        }
        let (resolved, diags) =
            compose_with(LIN_SCHEMA, BASE_AND_T, "thing", "t", &TestGrants { lookup });
        assert!(resolved.is_none());
        assert!(codes_of(&diags).contains(&codes::LAYER_REF_DENIED));
        assert!(diags[0].message.contains("no allowRefs entry"));
        assert!(
            !diags[0].message.contains("main.nml"),
            "allow-miss never names the path"
        );
    }

    #[test]
    fn grant_depth_cap_is_2066_naming_operator_change() {
        static GRANT: LayerGrant = LayerGrant {
            allow_refs: Vec::new(),
            deny_refs: Vec::new(),
            max_stack_depth: Some(1),
        };
        fn lookup(_: &str) -> GrantLookup<'static> {
            GrantLookup::Granted {
                grant: &GRANT,
                binding: "b",
                manifest: "m",
            }
        }
        struct AllowAll;
        impl LayerGrantProvider for AllowAll {
            fn grant_for(&self, _: &str) -> GrantLookup<'_> {
                lookup("")
            }
            fn ref_decision(&self, _: &LayerGrant, _: &str) -> RefDecision {
                RefDecision::Allowed
            }
        }
        let (resolved, diags) = compose_with(LIN_SCHEMA, BASE_AND_T, "thing", "t", &AllowAll);
        assert!(resolved.is_none());
        assert!(codes_of(&diags).contains(&codes::LAYER_BOUND_EXCEEDED));
        assert!(diags.iter().any(|d| d.message.contains("operator change")));
    }

    #[test]
    fn deny_veto_names_rule_index() {
        static GRANT: LayerGrant = LayerGrant {
            allow_refs: Vec::new(),
            deny_refs: Vec::new(),
            max_stack_depth: None,
        };
        struct VetoAll;
        impl LayerGrantProvider for VetoAll {
            fn grant_for(&self, _: &str) -> GrantLookup<'_> {
                GrantLookup::Granted {
                    grant: &GRANT,
                    binding: "tenantFlows",
                    manifest: "m",
                }
            }
            fn ref_decision(&self, _: &LayerGrant, _: &str) -> RefDecision {
                RefDecision::DenyVeto(2)
            }
        }
        let (resolved, diags) = compose_with(LIN_SCHEMA, BASE_AND_T, "thing", "t", &VetoAll);
        assert!(resolved.is_none());
        assert!(codes_of(&diags).contains(&codes::LAYER_REF_DENIED));
        assert!(
            diags.iter().any(|d| d.message.contains("denyRefs[2]")),
            "deny-veto names the rule by index: {diags:?}"
        );
    }

    #[test]
    fn provenance_origin_points_at_winning_layer_span() {
        let (resolved, _) = compose(FLOW_SCHEMA, SUMMARY, "flow", "cuXyz");
        let r = resolved.unwrap();
        let (_, origin) = r
            .origins
            .iter()
            .find(|(p, _)| p == "entrypoint")
            .expect("entrypoint recorded");
        let Origin::File { file, span } = origin else {
            panic!("schema defaults don't run at compose");
        };
        assert_eq!(file.to_str().unwrap(), "main.nml");
        // The base's `entrypoint = "search"` assignment — its span must
        // enclose that text in the source.
        assert!(span.start < span.end);
    }

    const MODIFIER_SCHEMA: &str = "\
model policy:
    label string
    |deny []string #append
";

    #[test]
    fn modifier_append_merges_inline_and_block_spellings() {
        let src = "\
policy base:
    label = \"x\"
    |deny = [\"a\"]

policy t uses base:
    |deny:
        - \"b\"
";
        let (resolved, diags) = compose(MODIFIER_SCHEMA, src, "policy", "t");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        let items: Vec<String> = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::Modifier(m) => match &m.value {
                    ModifierValue::Block(items) => Some(
                        items
                            .iter()
                            .filter_map(|i| match &i.kind {
                                ListItemKind::Shorthand { value, .. } => {
                                    value.value.as_str().map(str::to_string)
                                }
                                _ => None,
                            })
                            .collect(),
                    ),
                    _ => None,
                },
                _ => None,
            })
            .expect("merged modifier present as Block");
        assert_eq!(items, ["a", "b"], "deny list grew upward across spellings");
    }

    #[test]
    fn overlay_modifier_collision_replaces_wholesale_no_panic() {
        // Regression: bare (overlay-policy) modifiers with a colliding item
        // used to hit merge_items' list-policies-only unreachable!.
        let schema = "\
model policy:
    label string
    |deny []string
";
        let src = "\
policy base:
    label = \"x\"
    |deny = [\"a\"]

policy t uses base:
    |deny = [\"b\"]
";
        let (resolved, diags) = compose(schema, src, "policy", "t");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        let items: Vec<String> = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::Modifier(m) => match &m.value {
                    ModifierValue::Block(items) => Some(
                        items
                            .iter()
                            .filter_map(|i| match &i.kind {
                                ListItemKind::Shorthand { value, .. } => {
                                    value.value.as_str().map(str::to_string)
                                }
                                _ => None,
                            })
                            .collect(),
                    ),
                    _ => None,
                },
                _ => None,
            })
            .expect("modifier present");
        assert_eq!(items, ["b"], "bare modifier replaces wholesale");
    }

    #[test]
    fn duplicate_listed_ref_is_redundant_but_legal() {
        let src = "\
thing base:
    v = \"b\"

thing t uses base, base:
    v = \"t\"
";
        let (resolved, diags) = compose(LIN_SCHEMA, src, "thing", "t");
        assert!(diags.is_empty(), "redundant duplicate is silent: {diags:?}");
        assert_eq!(
            scalar(&resolved.unwrap().body, "v"),
            Some(&Value::String("t".into()))
        );
    }

    #[test]
    fn sealed_in_invalid_combo_still_seals() {
        // Fail-closed policy_of: `#sealed #append` composes as Sealed even
        // if a caller skipped validate_merge_policies (which errors it).
        let schema = "\
model m:
    entrypoint string #sealed #append
";
        let src = "\
m base:
    entrypoint = \"search\"

m evil uses base:
    entrypoint = \"adminPanel\"
";
        let (_, diags) = compose(schema, src, "m", "evil");
        assert!(
            codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "a schema typo must never widen a seal: {diags:?}"
        );
    }

    #[test]
    fn non_string_discriminator_survives_to_validation() {
        let src = "\
notify base:
    kind = \"az\"
    azureUrl = \"https://a\"
    azureKey = \"k\"

notify top uses base:
    kind = sns
";
        let (resolved, diags) = compose(ONEOF_SCHEMA, src, "notify", "top");
        // Not recognized as a switch (no backstop), and NOT silently
        // stripped: the bad assignment must reach the resolved body so
        // validation can flag it at its authored span.
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        let kinds: Vec<&Value> = body
            .entries
            .iter()
            .filter_map(|e| match &e.kind {
                BodyEntryKind::Property(p) if p.name.name == "kind" => Some(&p.value.value),
                _ => None,
            })
            .collect();
        assert!(
            kinds
                .iter()
                .any(|v| matches!(v, Value::Reference(r) if r == "sns")),
            "authored non-string discriminator preserved: {kinds:?}"
        );
    }

    // ── oneof accumulator + backstop ─────────────────────────────────────

    const ONEOF_SCHEMA: &str = "\
model gcp:
    kind string
    path string

model az:
    kind string
    azureUrl string
    azureKey string #sealed

model sns:
    kind string
    topicArn string

oneof notify by kind = \"gcp\":
    \"gcp\" -> gcp
    \"az\" -> az
    \"sns\" -> sns
";

    #[test]
    fn oneof_switch_discarding_seal_is_backstopped() {
        let src = "\
notify base:
    kind = \"az\"
    azureUrl = \"https://a\"
    azureKey = \"k\"

notify mid uses base:
    azureUrl = \"https://b\"

notify top uses mid:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
        let (resolved, diags) = compose(ONEOF_SCHEMA, src, "notify", "top");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        assert!(diags[0].message.contains("azureKey"));
        let body = resolved.unwrap().body;
        assert_eq!(scalar(&body, "kind"), Some(&Value::String("az".into())));
        assert_eq!(
            scalar(&body, "azureUrl"),
            Some(&Value::String("https://b".into())),
            "middle layer's deep-merge landed"
        );
        assert_eq!(scalar(&body, "azureKey"), Some(&Value::String("k".into())));
    }

    #[test]
    fn oneof_default_arm_switch_drops_base_fields_cleanly() {
        let src = "\
notify base:
    path = \"/var/log/x\"

notify top uses base:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
        let (resolved, diags) = compose(ONEOF_SCHEMA, src, "notify", "top");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        assert_eq!(scalar(&body, "kind"), Some(&Value::String("sns".into())));
        assert!(scalar(&body, "path").is_none(), "base arm fields dropped");
        assert_eq!(
            scalar(&body, "topicArn"),
            Some(&Value::String("arn:x".into()))
        );
    }

    // ── schema-load policy validation + lints ────────────────────────────

    #[test]
    fn sealed_composes_with_nothing_2068() {
        let diags =
            validate_merge_policies(&index_from("model m:\n    xs []string #sealed #append\n"));
        assert_eq!(codes_of(&diags), [codes::INVALID_MERGE_POLICY]);
    }

    #[test]
    fn identity_on_scalar_list_and_set_2068() {
        let diags = validate_merge_policies(&index_from(
            "model m:\n    xs []string #identity\n    ys set<string> #identity\n",
        ));
        assert_eq!(
            codes_of(&diags),
            [codes::INVALID_MERGE_POLICY, codes::INVALID_MERGE_POLICY]
        );
    }

    #[test]
    fn list_policy_on_scalar_field_2068() {
        let diags = validate_merge_policies(&index_from("model m:\n    x string #append\n"));
        assert_eq!(codes_of(&diags), [codes::INVALID_MERGE_POLICY]);
    }

    #[test]
    fn sealed_with_default_lints_2076() {
        let diags =
            validate_merge_policies(&index_from("model m:\n    order number = 100 #sealed\n"));
        assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL]);
    }

    #[test]
    fn bare_overlay_list_with_sealed_items_lints_2076() {
        let diags = validate_merge_policies(&index_from(
            "model step:\n    name string+\n    action string #sealed\n\nmodel flow:\n    steps []step\n",
        ));
        assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL]);
    }

    #[test]
    fn oneof_with_sealed_arm_and_unsealed_discriminator_lints_2076() {
        let diags = validate_merge_policies(&index_from(ONEOF_SCHEMA));
        assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL]);
    }

    #[test]
    fn identity_list_of_models_is_clean() {
        let diags = validate_merge_policies(&index_from(PAIR_SCHEMA));
        // PAIR_SCHEMA's `action #sealed` under an identity-granted list is
        // exactly the reachable-seal shape — no lint.
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn authored_empty_list_survives_compose() {
        // Regression: `xs = []` on a bare-overlay list vanished from the
        // composed body, cascading a spurious missing-required error.
        let schema = "model m:\n    xs []string\n    label string\n";
        let src = "\
m base:
    label = \"b\"
    xs = []

m top uses base:
    label = \"t\"
";
        let (resolved, diags) = compose(schema, src, "m", "top");
        assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
        let body = resolved.unwrap().body;
        assert!(
            body.entries.iter().any(|e| matches!(&e.kind,
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "xs")),
            "authored-empty field survives as present-but-empty"
        );
    }

    #[test]
    fn zero_item_entry_cannot_seal() {
        // Regression: a base `xs = []` on a `#sealed` list counted as the
        // sealing write, rejecting the next tier's legitimate first
        // assignment — contradicting both NML2079's no-op contract and
        // sealed's stays-open rule.
        let schema = "model m:\n    xs []string #sealed\n";
        let src = "\
m base:
    xs = []

m t uses base:
    xs = [\"a\"]
";
        let (resolved, diags) = compose(schema, src, "m", "t");
        assert!(
            !codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "a zero-item entry is not a write: {diags:?}"
        );
        let body = resolved.unwrap().body;
        let items = list_names(&body, "xs");
        assert_eq!(items.len(), 1, "t's first real assignment seals: {items:?}");
    }

    #[test]
    fn empty_overlay_modifier_cannot_empty_base() {
        // Regression: `|deny = []` under bare overlay silently EMPTIED the
        // base's deny list — a security-shaped allow-by-emptying.
        let schema = "model policy:\n    label string\n    |deny []string\n";
        let src = "\
policy base:
    label = \"x\"
    |deny = [\"a\"]

policy t uses base:
    |deny = []
";
        let (resolved, diags) = compose(schema, src, "policy", "t");
        assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
        let body = resolved.unwrap().body;
        let kept = body.entries.iter().any(|e| {
            matches!(&e.kind,
            BodyEntryKind::Modifier(Modifier { value: ModifierValue::Block(items), .. })
                if items.len() == 1)
        });
        assert!(kept, "base deny entry survives: {body:?}");
    }

    #[test]
    fn seal_backstop_reaches_list_item_seals() {
        // Regression: an arm switch silently discarded a sealed field
        // assigned INSIDE a list item ("at any depth" was ModelRef-only).
        let schema = "\
model step:
    name string+
    action string #sealed

model az:
    kind string
    steps []step #identity

model sns:
    kind string
    topicArn string

oneof notify by kind:
    \"az\" -> az
    \"sns\" -> sns
";
        let src = "\
notify base:
    kind = \"az\"
    steps:
        - search:
            action = \"type\"

notify top uses base:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
        let (_, diags) = compose(schema, src, "notify", "top");
        assert!(
            codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "backstop must see item-level seals: {diags:?}"
        );
    }

    #[test]
    fn lint_2076_reaches_list_item_seals() {
        let schema = "\
model step:
    name string+
    action string #sealed

model az:
    kind string
    steps []step #identity

oneof notify by kind:
    \"az\" -> az
";
        let diags = validate_merge_policies(&index_from(schema));
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::UNREACHABLE_SEAL)
                    && d.message.contains("discriminator")),
            "oneof lint must see seals on item models: {diags:?}"
        );
    }

    #[test]
    fn zero_item_base_does_not_establish_identity_list() {
        // Regression: `steps = []` in the base made the next tier's first
        // real items draw spurious NML2067 and vanish — a zero-item entry
        // neither supplies nor establishes (NML2079's contract).
        let schema = "\
model step:
    name string+
    locator string

model flow:
    steps []step #identity
";
        let src = "\
flow base:
    steps = []

flow t uses base:
    steps:
        - search:
            locator = \"#q\"
";
        let (resolved, diags) = compose(schema, src, "flow", "t");
        assert!(
            !codes_of(&diags).contains(&codes::UNMATCHED_OVERLAY_ITEM),
            "first real items are authored, not unmatched: {diags:?}"
        );
        assert_eq!(
            list_names(&resolved.unwrap().body, "steps"),
            ["search"],
            "the first item-supplying tier establishes the list"
        );
    }

    #[test]
    fn cycle_message_renders_the_full_path() {
        let src = "\
thing a uses b:
    v = \"a\"

thing b uses a:
    v = \"b\"
";
        let (_, diags) = compose(LIN_SCHEMA, src, "thing", "a");
        assert!(
            diags.iter().any(|d| d.message.contains("->")),
            "cycle path rendered: {diags:?}"
        );
    }

    // ── compose_file: the shared orchestration contract ──────────────────

    #[test]
    fn compose_file_substitutes_resolved_and_removes_failed() {
        let index = index_from(LIN_SCHEMA);
        let src = "\
thing base:
    v = \"b\"

thing good uses base:
    v = \"g\"

thing bad uses missing:
    v = \"x\"
";
        let file = file_of(src);
        let out = compose_file(&index, "main.nml", &file, &OpenContext);
        assert!(codes_of(&out.diagnostics).contains(&codes::UNRESOLVED_LAYER_REF));
        let vf = out.validation_file.expect("uses present");
        // `bad` removed (failed compose must not cascade); `good` and
        // `base` remain, `good` carrying its RESOLVED body.
        assert_eq!(vf.declarations.len(), 2);
        let good = vf
            .declarations
            .iter()
            .find_map(|d| match &d.kind {
                crate::ast::DeclarationKind::Block(b) if b.name.name == "good" => Some(b),
                _ => None,
            })
            .expect("good survives");
        assert_eq!(
            scalar(&good.body, "v"),
            Some(&Value::String("g".into())),
            "resolved body substituted"
        );
    }

    #[test]
    fn compose_file_none_when_no_uses() {
        let index = index_from(LIN_SCHEMA);
        let file = file_of("thing base:\n    v = \"b\"\n");
        let out = compose_file(&index, "main.nml", &file, &OpenContext);
        assert!(out.validation_file.is_none());
        assert!(out.diagnostics.is_empty());
    }

    #[test]
    fn compose_file_dedups_shared_substack_findings() {
        let index = index_from(DENY_SCHEMA);
        let src = "\
policy base:
    label = \"x\"
    denyHosts = [\"a\"]

policy mid uses base:
    denyHosts = []

policy top uses mid:
    label = \"y\"
";
        let file = file_of(src);
        let out = compose_file(&index, "main.nml", &file, &OpenContext);
        let zero_items: Vec<_> = out
            .diagnostics
            .iter()
            .filter(|d| d.code == Some(codes::ZERO_ITEM_LAYER_ENTRY))
            .collect();
        assert_eq!(
            zero_items.len(),
            1,
            "mid's zero-item entry reports once, not per composing root: {:?}",
            out.diagnostics
        );
    }

    // ── provenance ───────────────────────────────────────────────────────

    #[test]
    fn provenance_records_winning_layers() {
        let (resolved, _) = compose(FLOW_SCHEMA, SUMMARY, "flow", "cuXyz");
        let origins = resolved.unwrap().origins;
        assert!(
            origins.iter().any(|(p, _)| p == "entrypoint"),
            "sealed base assignment recorded: {origins:?}"
        );
        assert!(
            origins.iter().any(|(p, _)| p.starts_with("steps[")),
            "item identities recorded: {origins:?}"
        );
    }

    // ── round-4 review pins ──────────────────────────────────────────────

    fn nested_scalar<'r>(body: &'r Body, block: &str, name: &str) -> Option<&'r Value> {
        body.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == block => scalar(&nb.body, name),
            _ => None,
        })
    }

    #[test]
    fn sealed_object_field_is_write_once() {
        let schema = "\
model inner:
    x string

model outer:
    label string
    cfg inner #sealed
";
        let src = "\
outer base:
    label = \"a\"
    cfg:
        x = \"secret\"

outer evil uses base:
    label = \"b\"
    cfg:
        x = \"hijacked\"
";
        let (resolved, diags) = compose(schema, src, "outer", "evil");
        assert!(
            codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "an object body is a write — the seal must fire: {diags:?}"
        );
        let body = resolved.unwrap().body;
        assert_eq!(
            nested_scalar(&body, "cfg", "x"),
            Some(&Value::String("secret".into())),
            "the base's sealed object survives"
        );
    }

    const NESTED_ONEOF_SCHEMA: &str = "\
model gcpAuth:
    keyPath string #sealed

model snsAuth:
    topicArn string

oneof auth by kind = \"gcp\":
    \"gcp\" -> gcpAuth
    \"sns\" -> snsAuth

model azArm:
    kind string
    cred auth

model snsArm:
    kind string
    topicArn string

oneof notify by kind = \"az\":
    \"az\" -> azArm
    \"sns\" -> snsArm
";

    #[test]
    fn arm_switch_backstop_sees_seals_inside_nested_oneofs() {
        let src = "\
notify base:
    kind = \"az\"
    cred:
        kind = \"gcp\"
        keyPath = \"/secret/key\"

notify top uses base:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
        let (resolved, diags) = compose(NESTED_ONEOF_SCHEMA, src, "notify", "top");
        let seal = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("nested-oneof seal blocks the switch");
        assert!(
            seal.message.contains("cred.keyPath"),
            "names the buried seal: {}",
            seal.message
        );
        // Rejected switch: the sealed arm survives.
        assert_eq!(
            scalar(&resolved.unwrap().body, "kind"),
            Some(&Value::String("az".into()))
        );
    }

    const STEP_FLOW_SCHEMA: &str = "\
model step:
    name string+
    action string
    tags []string #append

model flow:
    steps []step #identity
";

    #[test]
    fn array_spelled_lists_inside_identity_items_merge() {
        let src = "\
flow base:
    steps:
        - search:
            action = \"type\"
            tags = [\"slow\"]

flow t uses base:
    steps:
        - search:
            tags = [\"fast\"]
";
        let (resolved, diags) = compose(STEP_FLOW_SCHEMA, src, "flow", "t");
        assert!(diags.is_empty(), "clean merge: {diags:?}");
        let body = resolved.unwrap().body;
        let steps = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "steps" => Some(&nb.body),
                _ => None,
            })
            .expect("steps present");
        let item_body = steps
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::ListItem(ListItem {
                    kind: ListItemKind::Named { body, .. },
                    ..
                }) => Some(body),
                _ => None,
            })
            .expect("named item");
        let tags = list_names(item_body, "tags");
        assert_eq!(tags.len(), 2, "both layers' tags survive: {tags:?}");
    }

    const ONEOF_ROOT_LIST_SCHEMA: &str = "\
model azR:
    kind string
    hosts []string #append

model snsR:
    kind string
    topicArn string

oneof relay by kind = \"az\":
    \"az\" -> azR
    \"sns\" -> snsR
";

    #[test]
    fn oneof_root_array_spelled_lists_merge() {
        let src = "\
relay base:
    kind = \"az\"
    hosts = [\"a\"]

relay t uses base:
    hosts = [\"b\"]
";
        let (resolved, diags) = compose(ONEOF_ROOT_LIST_SCHEMA, src, "relay", "t");
        assert!(diags.is_empty(), "clean merge: {diags:?}");
        let hosts = list_names(&resolved.unwrap().body, "hosts");
        assert_eq!(
            hosts.len(),
            2,
            "oneof-root lists normalize and merge: {hosts:?}"
        );
    }

    #[test]
    fn zero_item_sealed_list_entry_does_not_block_a_switch() {
        let schema = "\
model azR:
    kind string
    hosts []string #sealed

model snsR:
    kind string
    topicArn string

oneof relay by kind = \"az\":
    \"az\" -> azR
    \"sns\" -> snsR
";
        let src = "\
relay base:
    kind = \"az\"
    hosts = []

relay t uses base:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
        let (resolved, diags) = compose(schema, src, "relay", "t");
        assert!(
            !codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "a zero-item entry is not an assigned seal: {diags:?}"
        );
        assert_eq!(
            scalar(&resolved.unwrap().body, "kind"),
            Some(&Value::String("sns".into())),
            "the legal switch proceeds"
        );
    }

    #[test]
    fn arm_switch_reports_multi_seal_count() {
        let schema = "\
model azR:
    kind string
    key1 string #sealed
    key2 string #sealed

model snsR:
    kind string
    topicArn string

oneof relay by kind = \"az\":
    \"az\" -> azR
    \"sns\" -> snsR
";
        let src = "\
relay base:
    kind = \"az\"
    key1 = \"a\"
    key2 = \"b\"

relay t uses base:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
        let (_, diags) = compose(schema, src, "relay", "t");
        let seal = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("backstop fires");
        assert!(
            seal.message.contains("2 sealed fields would be discarded"),
            "states the count: {}",
            seal.message
        );
    }

    #[test]
    fn same_kind_identity_match_beats_cross_kind_collision() {
        let src = "\
flow base:
    steps:
        - \"a\"
        - a:
            action = \"x\"

flow t uses base:
    steps:
        - a:
            action = \"y\"
";
        let (resolved, diags) = compose(STEP_FLOW_SCHEMA, src, "flow", "t");
        assert!(
            !codes_of(&diags).contains(&codes::IDENTITY_REDEFINITION),
            "the named overlay pairs with the named base entry: {diags:?}"
        );
        let body = resolved.unwrap().body;
        let steps = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "steps" => Some(&nb.body),
                _ => None,
            })
            .expect("steps present");
        let named_action = steps.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::ListItem(ListItem {
                kind: ListItemKind::Named { body, .. },
                ..
            }) => scalar(body, "action"),
            _ => None,
        });
        assert_eq!(
            named_action,
            Some(&Value::String("y".into())),
            "the override lands on the same-kind partner"
        );
    }

    #[test]
    fn wide_clause_is_bounded_before_the_c3_merge() {
        let schema = "model thing:\n    v string\n";
        let mut src = String::from("thing a:\n    v = \"a\"\n\nthing b:\n    v = \"b\"\n\n");
        for i in 0..20 {
            src.push_str(&format!("thing base{i} uses a, b:\n    v = \"x\"\n\n"));
        }
        src.push_str("thing top uses ");
        src.push_str(
            &(0..20)
                .map(|i| format!("base{i}"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        src.push_str(":\n    v = \"t\"\n");
        let (resolved, diags) = compose(schema, &src, "thing", "top");
        assert!(resolved.is_none());
        assert!(
            codes_of(&diags).contains(&codes::LAYER_BOUND_EXCEEDED),
            "breadth rejects as NML2066 without running the merge: {diags:?}"
        );
    }

    #[test]
    fn scalar_keyed_identity_merge_is_not_a_dead_delta() {
        let schema = "\
model route:
    path string+
    timeout string

model api:
    routes []route #identity
";
        let src = "\
api base:
    routes:
        - \"/api\"

api t uses base:
    routes:
        - \"/api\":
            timeout = \"60\"
";
        let (resolved, diags) = compose(schema, src, "api", "t");
        assert!(
            !codes_of(&diags).contains(&codes::DEAD_DELTA),
            "the materialized identity token is pairing machinery, not a \
             restatement: {diags:?}"
        );
        let body = resolved.unwrap().body;
        let routes = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "routes" => Some(&nb.body),
                _ => None,
            })
            .expect("routes present");
        let timeout = routes.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::ListItem(ListItem {
                kind: ListItemKind::Shorthand { body: Some(b), .. },
                ..
            }) => scalar(b, "timeout"),
            _ => None,
        });
        assert_eq!(timeout, Some(&Value::String("60".into())), "merge landed");
    }

    #[test]
    fn nml2077_names_a_transitive_base_listed_after_its_dependent() {
        let schema = "model thing:\n    v string\n";
        let src = "\
thing b:
    v = \"b\"

thing d uses b:
    v = \"d\"

thing c uses d, b:
    v = \"c\"
";
        let (_, diags) = compose(schema, src, "thing", "c");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::INCONSISTENT_LINEARIZATION))
            .expect("NML2077 fires");
        assert!(
            d.message
                .contains("'b' is already a transitive base of 'd'"),
            "names the pair and the cause: {}",
            d.message
        );
        assert!(
            !d.suggestions.is_empty(),
            "carries the machine-applicable remove-the-ref fix"
        );
    }

    #[test]
    fn nml2077_names_an_opposed_shared_pair() {
        let schema = "model thing:\n    v string\n";
        let src = "\
thing slow:
    v = \"s\"

thing fast:
    v = \"f\"

thing vendorX uses slow, fast:
    v = \"x\"

thing productY uses fast, slow:
    v = \"y\"

thing tenant uses vendorX, productY:
    v = \"t\"
";
        let (_, diags) = compose(schema, src, "thing", "tenant");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::INCONSISTENT_LINEARIZATION))
            .expect("NML2077 fires");
        assert!(
            d.message.contains("order the shared pair"),
            "names the sibling contradiction: {}",
            d.message
        );
        assert!(
            d.message.contains("'fast'") && d.message.contains("'slow'"),
            "names the pair itself: {}",
            d.message
        );
    }

    #[test]
    fn cross_kind_identity_collision_is_2063() {
        let src = "\
flow base:
    steps:
        - search:
            action = \"x\"

flow t uses base:
    steps:
        - \"search\"
";
        let (_, diags) = compose(STEP_FLOW_SCHEMA, src, "flow", "t");
        assert!(
            codes_of(&diags).contains(&codes::IDENTITY_REDEFINITION),
            "shorthand vs named at an equal token is cross-kind NML2063: {diags:?}"
        );
    }

    #[test]
    fn reference_items_identical_restatement_is_a_noop() {
        let schema = "\
model flow2:
    steps []string #identity
";
        let src = "\
flow2 base:
    steps = [@ops]

flow2 t uses base:
    steps = [@ops]
";
        let (resolved, diags) = compose(schema, src, "flow2", "t");
        assert!(
            diags.is_empty(),
            "identical restatement is a no-op: {diags:?}"
        );
        assert_eq!(
            list_names(&resolved.unwrap().body, "steps").len(),
            1,
            "one role item, not a duplicate"
        );
    }

    #[test]
    fn oneof_field_reference_lints_2076_exactly_once() {
        let schema = "\
model azR:
    kind string
    key string #sealed

oneof relay by kind = \"az\":
    \"az\" -> azR

model svc:
    out relay
";
        let index = index_from(schema);
        let diags = validate_merge_policies(&index);
        let n = diags
            .iter()
            .filter(|d| d.code == Some(codes::UNREACHABLE_SEAL))
            .count();
        assert_eq!(n, 1, "one schema defect, one warning: {diags:?}");
    }

    #[test]
    fn validate_side_uses_refs_share_check_wording() {
        let file = file_of("flow t uses missingLayer:\n    entrypoint = \"x\"\n");
        let diags = check_uses_refs("main.nml", &file);
        assert_eq!(codes_of(&diags), vec![codes::UNRESOLVED_LAYER_REF]);
        assert!(
            diags[0].message.contains("does not resolve"),
            "same wording owner as the composing path: {}",
            diags[0].message
        );
        // A schema definition's clause is definition-intrinsic: `validate`
        // owns it too, with the composing path's exact NML2062 wording.
        let schema_def = file_of("model m uses other:\n    x string\n");
        let diags = check_uses_refs("main.nml", &schema_def);
        assert_eq!(codes_of(&diags), vec![codes::LAYER_KEYWORD_MISMATCH]);
        assert!(
            diags[0].message.contains("delete the clause"),
            "same wording owner as compose_file: {}",
            diags[0].message
        );
    }

    // ── round-5 review pins ──────────────────────────────────────────────

    const MODIFIER_SEAL_SCHEMA: &str = "\
model policy:
    label string
    |deny []string #sealed
";

    #[test]
    fn spelling_is_authoring_not_identity_for_seals() {
        // A modifier-declared sealed field written back in property
        // spelling is the SAME field: the two spellings must meet in one
        // seal check, or each spelling silently dodges the other's seal
        // and the composed body carries two entries for one field.
        let src = "\
policy base:
    label = \"x\"
    |deny = [\"a\"]

policy t uses base:
    deny = [\"b\"]
";
        let (resolved, diags) = compose(MODIFIER_SEAL_SCHEMA, src, "policy", "t");
        assert!(
            codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "cross-spelling write hits the seal: {diags:?}"
        );
        let body = resolved.unwrap().body;
        let deny_entries = body
            .entries
            .iter()
            .filter(|e| match &e.kind {
                BodyEntryKind::Modifier(m) => m.name.name == "deny",
                BodyEntryKind::NestedBlock(nb) => nb.name.name == "deny",
                BodyEntryKind::Property(p) => p.name.name == "deny",
                _ => false,
            })
            .count();
        assert_eq!(deny_entries, 1, "one field, one composed entry");
    }

    #[test]
    fn zero_item_entry_never_violates_a_seal() {
        let schema = "\
model m:
    xs []string #sealed
";
        let src = "\
m base:
    xs = [\"a\"]

m t uses base:
    xs = []
";
        let (resolved, diags) = compose(schema, src, "m", "t");
        assert!(
            !codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "a zero-item entry is the same warned no-op above a seal as \
             everywhere else: {diags:?}"
        );
        assert!(
            codes_of(&diags).contains(&codes::ZERO_ITEM_LAYER_ENTRY),
            "still warned as NML2079: {diags:?}"
        );
        assert_eq!(
            list_names(&resolved.unwrap().body, "xs").len(),
            1,
            "the sealed base data survives"
        );
    }

    #[test]
    fn rejected_switch_normalizes_against_the_surviving_arm() {
        // The pre-pass mirrors the backstop: a rejected switch must not
        // have already materialized lower layers against the rejected
        // arm's element models (injecting its positional fields into the
        // composed body at the author's spans).
        let schema = "\
model stepA:
    cmd string+
    note string

model stepB:
    url string+
    note string

model armA:
    kind string
    token string #sealed
    steps []stepA #identity

model armB:
    kind string
    steps []stepB #identity

oneof job by kind = \"a\":
    \"a\" -> armA
    \"b\" -> armB
";
        let src = "\
job base:
    token = \"sekrit\"
    steps:
        - \"build\":
            note = \"n1\"

job t uses base:
    kind = \"b\"
    steps:
        - \"deploy\"
";
        let (resolved, diags) = compose(schema, src, "job", "t");
        assert!(
            codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "the switch is rejected: {diags:?}"
        );
        let body = resolved.unwrap().body;
        let item_fields: Vec<String> = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "steps" => Some(&nb.body),
                _ => None,
            })
            .expect("steps present")
            .entries
            .iter()
            .filter_map(|e| match &e.kind {
                BodyEntryKind::ListItem(ListItem {
                    kind: ListItemKind::Shorthand { body: Some(b), .. },
                    ..
                }) => Some(b),
                _ => None,
            })
            .flat_map(|b| {
                b.entries.iter().filter_map(|e| match &e.kind {
                    BodyEntryKind::Property(p) => Some(p.name.name.clone()),
                    _ => None,
                })
            })
            .collect();
        assert!(
            item_fields.contains(&"cmd".to_string()),
            "materialized against the SURVIVING arm: {item_fields:?}"
        );
        assert!(
            !item_fields.contains(&"url".to_string()),
            "no rejected-arm injection: {item_fields:?}"
        );
    }

    #[test]
    fn one_layer_stating_a_field_twice_is_one_layer() {
        let schema = "\
model item:
    name string+
    v string

model m:
    xs []item #identity
";
        // Two spellings in ONE body: the layer's own later entry is not an
        // unmatched overlay, and a duplicated identity across the two
        // entries is the within-layer duplicate error.
        let src_ok = "\
m base:
    xs = [\"a\"]
    xs:
        - \"b\"

m t uses base:
    xs:
        - \"a\":
            v = \"x\"
";
        let (resolved, diags) = compose(schema, src_ok, "m", "t");
        assert!(
            !codes_of(&diags).contains(&codes::UNMATCHED_OVERLAY_ITEM),
            "the base's own second entry is base, not overlay: {diags:?}"
        );
        assert_eq!(list_names(&resolved.unwrap().body, "xs").len(), 2);

        let src_dup = "\
m base:
    xs = [\"a\"]
    xs:
        - \"a\"

m t uses base:
    xs:
        - \"a\":
            v = \"x\"
";
        let (_, diags) = compose(schema, src_dup, "m", "t");
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::IDENTITY_REDEFINITION)
                    && d.message.contains("duplicate identity in one layer")),
            "cross-spelling within-layer duplicate is caught: {diags:?}"
        );
    }

    #[test]
    fn zero_item_overlay_cannot_empty_across_spellings() {
        let schema = "\
model m:
    xs []string
";
        let src = "\
m base:
    xs:
        - \"a\"

m t uses base:
    |xs = []
";
        let (resolved, diags) = compose(schema, src, "m", "t");
        assert!(
            codes_of(&diags).contains(&codes::ZERO_ITEM_LAYER_ENTRY),
            "warned no-op: {diags:?}"
        );
        assert_eq!(
            list_names(&resolved.unwrap().body, "xs").len(),
            1,
            "the base list survives every zero-item spelling"
        );
    }

    #[test]
    fn seal_scan_is_bounded_on_recursive_oneofs() {
        // DoS regression: a per-candidate-arm re-scan of the same child
        // body was exponential in nesting depth over a recursive oneof —
        // the union-vocabulary scan visits each node once. Depth 40 with
        // two candidate arms per level would be ~2^40 re-scans under the
        // old scheme; it must compose instantly.
        let schema = "\
model deepA:
    k string
    child rec

model deepB:
    k string
    child rec

oneof rec by k = \"a\":
    \"a\" -> deepA
    \"b\" -> deepB

model azArm:
    kind string
    secret string #sealed
    root rec

model snsArm:
    kind string
    topicArn string

oneof notify by kind = \"az\":
    \"az\" -> azArm
    \"sns\" -> snsArm
";
        let mut nest = String::new();
        let depth = 40;
        for i in 0..depth {
            let pad = "    ".repeat(i + 2);
            nest.push_str(&format!("{}child:\n{}    k = \"b\"\n", pad, pad));
        }
        let src = format!(
            "notify base:\n    kind = \"az\"\n    secret = \"s\"\n    root:\n        k = \"b\"\n{nest}\nnotify t uses base:\n    kind = \"sns\"\n    topicArn = \"arn:x\"\n"
        );
        let start = std::time::Instant::now();
        let (_, diags) = compose(schema, &src, "notify", "t");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "union-vocabulary scan is linear, took {:?}",
            start.elapsed()
        );
        assert!(
            codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "the assigned seal still blocks the switch: {diags:?}"
        );
    }

    #[test]
    fn backstop_counts_item_seals_distinctly() {
        let schema = "\
model cred:
    name string+
    key string #sealed

model azArm:
    kind string
    creds []cred #identity

model snsArm:
    kind string
    topicArn string

oneof relay by kind = \"az\":
    \"az\" -> azArm
    \"sns\" -> snsArm
";
        let src = "\
relay base:
    kind = \"az\"
    creds:
        - alpha:
            key = \"k1\"
        - beta:
            key = \"k2\"

relay t uses base:
    kind = \"sns\"
    topicArn = \"arn:x\"
";
        let (_, diags) = compose(schema, src, "relay", "t");
        let seal = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("backstop fires");
        assert!(
            seal.message.contains("2 sealed fields would be discarded"),
            "two items' seals are two findings, not one masked path: {}",
            seal.message
        );
        assert!(
            seal.message.contains("creds[alpha].key"),
            "item segment names the identity: {}",
            seal.message
        );
    }

    #[test]
    fn multi_root_cycle_reports_once() {
        let schema = "model thing:\n    v string\n";
        let src = "\
thing a uses b:
    v = \"a\"

thing b uses a:
    v = \"b\"
";
        let index = index_from(schema);
        let file = file_of(src);
        let out = compose_file(&index, "main.nml", &file, &OpenContext);
        let cycles = out
            .diagnostics
            .iter()
            .filter(|d| d.code == Some(codes::LAYER_CYCLE))
            .count();
        assert_eq!(
            cycles, 1,
            "one cycle, one finding — rotations canonicalize: {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn stack_level_denial_wording_contract() {
        // Stack-level allow-miss names the BINDING and the entering ref —
        // never the denied layer's author-chosen instance name (that
        // would leak through the denial); site-level names the author's
        // own listed token. Deny-veto may name (the allow admitted it).
        let d = ref_denial(
            RefDecision::AllowMiss,
            "secretName",
            "tenantFlows",
            Denial::Stack {
                entering: Some("vendorBase"),
            },
        )
        .unwrap();
        assert!(
            !d.message.contains("secretName"),
            "no denied-layer disclosure: {}",
            d.message
        );
        assert!(
            d.message
                .contains("'vendorBase' in this clause pulls it in")
        );
        let d = ref_denial(RefDecision::AllowMiss, "ownRef", "b", Denial::Site).unwrap();
        assert!(
            d.message.contains("ownRef"),
            "site names the author's token"
        );
        let d = ref_denial(
            RefDecision::DenyVeto(2),
            "x",
            "b",
            Denial::Stack { entering: None },
        )
        .unwrap();
        assert!(d.message.contains("denyRefs[2]"));
        assert!(d.message.contains("stack-level"));
    }

    #[test]
    fn nml2077_sibling_offers_both_reorderings() {
        let schema = "model thing:\n    v string\n";
        let src = "\
thing slow:
    v = \"s\"

thing fast:
    v = \"f\"

thing vendorX uses slow, fast:
    v = \"x\"

thing productY uses fast, slow:
    v = \"y\"

thing tenant uses vendorX, productY:
    v = \"t\"
";
        let (_, diags) = compose(schema, src, "thing", "tenant");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::INCONSISTENT_LINEARIZATION))
            .expect("NML2077 fires");
        assert!(
            d.message.contains("align them: order"),
            "offers the two reorderings: {}",
            d.message
        );
    }

    #[test]
    fn nml2077_removal_span_swallows_the_separator() {
        let schema = "model thing:\n    v string\n";
        let src = "\
thing b:
    v = \"b\"

thing d uses b:
    v = \"d\"

thing c uses d, b:
    v = \"c\"
";
        let (_, diags) = compose(schema, src, "thing", "c");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::INCONSISTENT_LINEARIZATION))
            .expect("NML2077 fires");
        let sugg = d.suggestions.first().expect("carries the machine fix");
        assert_eq!(
            &src[sugg.span.start..sugg.span.end],
            ", b",
            "deleting the span leaves no dangling comma"
        );
    }

    #[test]
    fn modifier_zero_item_warning_is_list_scoped() {
        let schema = "\
model m:
    |note string
    xs []string
";
        let src = "\
m base:
    xs = [\"a\"]

m t uses base:
    |note = []
";
        let (_, diags) = compose(schema, src, "m", "t");
        assert!(
            !codes_of(&diags).contains(&codes::ZERO_ITEM_LAYER_ENTRY),
            "NML2079 is list-scoped — a non-list modifier is the type \
             checker's business: {diags:?}"
        );
    }

    // ── round-6 review pins ──────────────────────────────────────────────

    #[test]
    fn positional_injection_never_fabricates_a_seal() {
        // The fold and the merge share ONE decision trace judged over
        // displaced-arm-normalized bodies: machinery materialized under
        // the surviving arm must never read as an authored write to the
        // displaced arm's same-named sealed field.
        let schema = "\
model stepA:
    action string #sealed
    note string

model stepB:
    action string+
    note string

model armA:
    steps []stepA #identity

model armB:
    steps []stepB #identity

oneof job by kind = \"a\":
    \"a\" -> armA
    \"b\" -> armB
";
        let src = "\
job base:
    steps:
        - \"build\":
            note = \"n1\"

job t uses base:
    kind = \"b\"
    steps:
        - \"deploy\"
";
        let (resolved, diags) = compose(schema, src, "job", "t");
        assert!(
            !codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "no layer assigned the sealed field — the switch is legal: {diags:?}"
        );
        assert_eq!(
            scalar(&resolved.unwrap().body, "kind"),
            Some(&Value::String("b".into())),
            "the accepted switch holds"
        );
    }

    const ITEM_ONEOF_SCHEMA: &str = "\
model spArm:
    ikind string
    secret string #sealed

model ptArm:
    ikind string
    port string

oneof istep by ikind = \"sp\":
    \"sp\" -> spArm
    \"pt\" -> ptArm

model armX:
    steps []istep #identity

model armY:
    note string

oneof svc by kind = \"x\":
    \"x\" -> armX
    \"y\" -> armY
";

    #[test]
    fn item_group_seal_meets_cross_layer_discriminator() {
        // Identity-matched items compose as ONE value: a discriminator in
        // the base's item and a seal in the mid's item must meet in the
        // backstop, or the pair launders through a switch.
        let src = "\
svc base:
    kind = \"x\"
    steps:
        - w:
            ikind = \"sp\"

svc mid uses base:
    steps:
        - w:
            secret = \"s3\"

svc t uses mid:
    kind = \"y\"
    note = \"n\"
";
        let (_, diags) = compose(ITEM_ONEOF_SCHEMA, src, "svc", "t");
        let seal = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("cross-layer item seal blocks the switch");
        assert!(
            seal.message.contains("steps[w].secret"),
            "full item path: {}",
            seal.message
        );
    }

    #[test]
    fn oneof_element_items_enforce_seals_directly() {
        // A oneof ELEMENT routes item bodies through the arm accumulator:
        // restating a sealed arm field across layers is NML2060, not a
        // silent model-less overwrite.
        let schema = "\
model spArm:
    ikind string
    secret string #sealed

model ptArm:
    ikind string
    port string

oneof istep by ikind = \"sp\":
    \"sp\" -> spArm
    \"pt\" -> ptArm

model flow:
    steps []istep #identity
";
        let src = "\
flow base:
    steps:
        - w:
            ikind = \"sp\"
            secret = \"a\"

flow t uses base:
    steps:
        - w:
            secret = \"b\"
";
        let (resolved, diags) = compose(schema, src, "flow", "t");
        assert!(
            codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "arm-aware item merge enforces the seal: {diags:?}"
        );
        let body = resolved.unwrap().body;
        let secret = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "steps" => Some(&nb.body),
                _ => None,
            })
            .and_then(|steps| {
                steps.entries.iter().find_map(|e| match &e.kind {
                    BodyEntryKind::ListItem(ListItem {
                        kind: ListItemKind::Named { body, .. },
                        ..
                    }) => scalar(body, "secret"),
                    _ => None,
                })
            });
        assert_eq!(
            secret,
            Some(&Value::String("a".into())),
            "the sealed base value survives"
        );
    }

    #[test]
    fn shared_property_seals_block_switches() {
        // `.shared`-distributed sealed writes are authored semantics: the
        // fold judges displaced-arm-NORMALIZED bodies, so a shared value
        // that materializes into (even bodiless) items still counts.
        let schema = "\
model xItem:
    name string+
    val string #sealed

model armX:
    xs []xItem #identity

model armY:
    note string

oneof svc by kind = \"x\":
    \"x\" -> armX
    \"y\" -> armY
";
        let src = "\
svc base:
    kind = \"x\"
    xs:
        .val = \"v\"
        - \"one\"
        - \"two\"

svc t uses base:
    kind = \"y\"
    note = \"n\"
";
        let (_, diags) = compose(schema, src, "svc", "t");
        let seal = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("shared-distributed seals are assigned seals");
        // ONE authored `.val` write distributed into two items is one
        // finding (both injected copies carry the authored span, and the
        // (path, span) dedup collapses them) — count reflects authored
        // assignments, not distribution fan-out.
        assert!(
            !seal.message.contains("sealed fields would be discarded"),
            "one authored write reports once: {}",
            seal.message
        );
    }

    #[test]
    fn nml2062_offers_a_same_keyword_did_you_mean() {
        let schema = "model thing:\n    v string\n\nmodel other:\n    v string\n";
        let src = "\
thing basePlan:
    v = \"b\"

other basePlot:
    v = \"o\"

thing t uses basePlot:
    v = \"t\"
";
        let (_, diags) = compose(schema, src, "thing", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::LAYER_KEYWORD_MISMATCH))
            .expect("cross-keyword ref is NML2062");
        assert!(
            d.message.contains("did you mean 'basePlan'"),
            "near-named same-keyword hint: {}",
            d.message
        );
    }

    #[test]
    fn nml2077_duplicated_ref_carries_no_suggestion() {
        // One span cannot remove every occurrence of a duplicated name,
        // and a machine fix that doesn't fix stalls `nml fix`.
        let schema = "model thing:\n    v string\n";
        let src = "\
thing b:
    v = \"b\"

thing d uses b:
    v = \"d\"

thing c uses d, b, b:
    v = \"c\"
";
        let (_, diags) = compose(schema, src, "thing", "c");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::INCONSISTENT_LINEARIZATION))
            .expect("NML2077 fires");
        assert!(
            d.suggestions.is_empty(),
            "hint-only when the name is duplicated: {d:?}"
        );
    }

    #[test]
    fn bare_overlay_oneof_element_list_lints_2076() {
        let schema = "\
model spArm:
    ikind string
    secret string #sealed

oneof istep by ikind = \"sp\":
    \"sp\" -> spArm

model flow:
    steps []istep
";
        let index = index_from(schema);
        let diags = validate_merge_policies(&index);
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::UNREACHABLE_SEAL)
                    && d.message.contains("bare-overlay")),
            "sealed arms of a oneof ELEMENT are seals too: {diags:?}"
        );
    }

    // ── round-7 review pins ──────────────────────────────────────────────

    const NESTED_UNDER_PARENT_SCHEMA: &str = "\
model subA:
    sk string
    w string

model subX:
    sk string
    v string #sealed

oneof subo by sk = \"sa\":
    \"sa\" -> subA
    \"sx\" -> subX

model armA:
    pk string
    note string

model armB:
    pk string
    sub subo

oneof po by pk = \"a\":
    \"a\" -> armA
    \"b\" -> armB
";

    #[test]
    fn numeric_keyed_items_scatter_across_buckets() {
        // Regression (DoS): a type-name prehash collapsed every numeric
        // token into one bucket, resurrecting the O(n²) identity scan.
        // Distinct numeric values must scatter; semantic-equals must not.
        use std::collections::HashSet;
        let distinct: HashSet<u64> = (0..500)
            .map(|i| token_prehash(&ItemKey::Scalar(Value::number(i))))
            .collect();
        assert!(
            distinct.len() > 400,
            "distinct numbers scatter across buckets, got {} for 500",
            distinct.len()
        );
        // `1` and `1.0` are the same value — same bucket.
        let one: crate::decimal::Number = "1".parse().unwrap();
        let one_point_oh: crate::decimal::Number = "1.0".parse().unwrap();
        assert_eq!(
            token_prehash(&ItemKey::Scalar(Value::number(one))),
            token_prehash(&ItemKey::Scalar(Value::number(one_point_oh))),
            "semantically-equal decimals share a bucket"
        );
    }

    #[test]
    fn duplicate_field_names_keep_plan_and_merge_aligned() {
        // The plan must key each path by the SAME field the merge's
        // first-wins map resolves — a trace folded under the wrong
        // duplicate replays against a body the merge composed under the
        // other, laundering a seal or fabricating a refusal.
        let schema = "\
model subA:
    k string
    v string #sealed

model subB:
    k string
    w string

oneof so by k = \"sa\":
    \"sa\" -> subA

oneof so2 by k = \"sb\":
    \"sb\" -> subB

model app:
    cfg subA
    cfg subB
";
        let src = "\
app base:
    cfg:
        v = \"secret\"

app t uses base:
    cfg:
        k = \"sb\"
";
        let (_, diags) = compose(schema, src, "app", "t");
        // First-wins (subA) governs everywhere: no fabricated refusal on
        // a discriminator subA doesn't have, and subA's seal is judged
        // consistently. The point is coherence, not a specific verdict.
        assert!(
            !diags
                .iter()
                .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION) && d.message.contains("k2")),
            "no fabricated refusal from a desynced plan: {diags:?}"
        );
    }

    #[test]
    fn scalar_spelling_cannot_launder_an_object_seal() {
        // An object-typed field always deep-merges its nested
        // contributions — a scalar/modifier spelling (invalid for an
        // object field) must never win and discard a sealed nested body.
        let schema = "\
model inner:
    path string #sealed

model outer:
    cfg inner
    label string
";
        for top in [
            "outer t uses base:\n    cfg = \"gone\"\n    label = \"x\"\n",
            "outer t uses base:\n    |cfg = \"gone\"\n    label = \"x\"\n",
        ] {
            let src = format!("outer base:\n    cfg:\n        path = \"secret\"\n\n{top}");
            let (resolved, _) = compose(schema, &src, "outer", "t");
            assert_eq!(
                nested_scalar(&resolved.unwrap().body, "cfg", "path"),
                Some(&Value::String("secret".into())),
                "the sealed nested body survives the scalar spelling: {top}"
            );
        }
    }

    #[test]
    fn modifier_backstop_uses_the_shared_write_predicate() {
        // The seal-scan Modifier arm must judge a WRITE the same way
        // `merge_sealed` does (`seal_write`) — a non-list sealed field
        // writes with every entry, so a modifier-spelled restatement in
        // a displaced arm must block the switch.
        let schema = "\
model ara:
    kind string
    v string #sealed

model arb:
    kind string
    other string

oneof cf by kind = \"a\":
    \"a\" -> ara
    \"b\" -> arb

model box2:
    cfg cf
";
        let src = "\
box2 base:
    cfg:
        |v = \"x\"

box2 t uses base:
    cfg:
        kind = \"b\"
        other = \"y\"
";
        let (_, diags) = compose(schema, src, "box2", "t");
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)
                    && d.message.contains("cannot launder")),
            "modifier-spelled non-list seal blocks the switch: {diags:?}"
        );
    }

    #[test]
    fn nested_traces_fold_over_the_surviving_parent_group() {
        // A discarded lower layer must not poison a nested position's
        // decisions: its stated nested arm would leave the survivors all
        // traced Join, the replay stuck at the DEFAULT arm, and the seal
        // silently overlaid — with the discarded layer's discriminator
        // still prepended, a body merged under the wrong vocabulary.
        let src = "\
po l1:
    sub:
        sk = \"sx\"

po l2 uses l1:
    pk = \"b\"
    sub:
        sk = \"sx\"
        v = \"one\"

po top uses l2:
    sub:
        sk = \"sx\"
        v = \"two\"
";
        let (resolved, diags) = compose(NESTED_UNDER_PARENT_SCHEMA, src, "po", "top");
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)
                    && d.message.contains("sub.v")),
            "the surviving group's nested seal holds: {diags:?}"
        );
        assert_eq!(
            nested_scalar(&resolved.unwrap().body, "sub", "v"),
            Some(&Value::String("one".into())),
            "the first surviving write wins"
        );
    }

    #[test]
    fn duplicate_field_entries_replay_positionally() {
        // One layer may state a oneof-typed field twice — two
        // contributions, two decisions, ONE id. An id-keyed trace lookup
        // collapsed them: a switch stuck, or the pre-Join switch body's
        // sealed key silently vanished.
        let schema = "\
model gcp:
    kind string
    path string

model az:
    kind string
    azureUrl string
    azureKey string

oneof conf by kind = \"gcp\":
    \"gcp\" -> gcp
    \"az\" -> az

model svc:
    cfg conf
";
        let src = "\
svc base:
    cfg:
        path = \"p\"

svc t uses base:
    cfg:
        kind = \"az\"
        azureKey = \"k\"
    cfg:
        azureUrl = \"u\"
";
        let (resolved, _) = compose(schema, src, "svc", "t");
        let body = resolved.unwrap().body;
        assert_eq!(
            nested_scalar(&body, "cfg", "azureKey"),
            Some(&Value::String("k".into())),
            "the switching entry's own fields survive its sibling Join"
        );
        assert_eq!(
            nested_scalar(&body, "cfg", "azureUrl"),
            Some(&Value::String("u".into())),
            "the post-switch sibling deep-merges"
        );
        assert_eq!(
            nested_scalar(&body, "cfg", "path"),
            None,
            "the switch discards the pre-switch arm's fields"
        );
    }

    #[test]
    fn duplicate_schema_field_names_are_first_wins() {
        // The field maps must match the linear scans they replaced —
        // last-wins would let a duplicate declaration silently swap which
        // `#sealed` governs (fail-open on a broken schema).
        let schema = "\
model dup:
    v string #sealed
    v string
";
        let src = "\
dup base:
    v = \"one\"

dup t uses base:
    v = \"two\"
";
        let (resolved, diags) = compose(schema, src, "dup", "t");
        assert!(
            codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "the FIRST duplicate's #sealed governs: {diags:?}"
        );
        assert_eq!(
            scalar(&resolved.unwrap().body, "v"),
            Some(&Value::String("one".into()))
        );
    }

    const MODIFIER_ITEM_SEAL_SCHEMA: &str = "\
model mstep:
    name string+
    act string #sealed

model wsa:
    skind string
    steps []mstep #identity

model wsb:
    skind string
    other string

oneof wcfg by skind = \"a\":
    \"a\" -> wsa
    \"b\" -> wsb

model box:
    cfg wcfg
";

    #[test]
    fn modifier_spelled_items_enforce_and_backstop_seals() {
        // The modifier spelling is the same list: its items' seals bind
        // in the ordinary merge...
        let src = "\
box base:
    cfg:
        |steps:
            - s1:
                act = \"x\"

box t uses base:
    cfg:
        |steps:
            - s1:
                act = \"y\"
";
        let (_, diags) = compose(MODIFIER_ITEM_SEAL_SCHEMA, src, "box", "t");
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)
                    && d.message.contains("steps[s1].act")),
            "modifier-spelled item seal enforced: {diags:?}"
        );
        // ...and in the arm-switch backstop.
        let src2 = "\
box base:
    cfg:
        |steps:
            - s1:
                act = \"x\"

box t uses base:
    cfg:
        skind = \"b\"
        other = \"y\"
";
        let (_, diags) = compose(MODIFIER_ITEM_SEAL_SCHEMA, src2, "box", "t");
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)
                    && d.message.contains("cannot launder")),
            "modifier-spelled item seal blocks the switch: {diags:?}"
        );
    }

    #[test]
    fn cross_kind_items_do_not_widen_arm_scopes() {
        // The merge refuses cross-kind pairs (NML2063) and never composes
        // them — so a scalar item's SEALED arm must not attach to a
        // same-token NAMED item's scope and fabricate a refusal of a
        // legal switch.
        let schema = "\
model epm:
    ek string
    v string

model eqm:
    ek string
    v string #sealed

oneof el by ek = \"ep\":
    \"ep\" -> epm
    \"eq\" -> eqm

model armX:
    kind string
    items []el #identity

model armY:
    kind string
    z string

oneof svc by kind = \"x\":
    \"x\" -> armX
    \"y\" -> armY
";
        let src = "\
svc base:
    kind = \"x\"
    items:
        - \"n1\":
            ek = \"eq\"

svc mid uses base:
    items:
        - n1:
            v = \"w\"

svc t uses mid:
    kind = \"y\"
    z = \"z\"
";
        let (resolved, diags) = compose(schema, src, "svc", "t");
        assert!(
            !codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "no seal was assigned in either item's OWN scope — the switch \
             is legal: {diags:?}"
        );
        assert_eq!(
            scalar(&resolved.unwrap().body, "kind"),
            Some(&Value::String("y".into()))
        );
    }

    #[test]
    fn oneof_element_scalar_items_materialize_without_dead_delta() {
        // Oneof elements materialize their `+` token through the item's
        // effective arm, and the strip knows that arm too — otherwise
        // every scalar-keyed oneof item drew a spurious NML2084.
        let schema = "\
model spArm:
    name string+
    ikind string
    note string

oneof istep by ikind = \"sp\":
    \"sp\" -> spArm

model flow:
    steps []istep #identity
";
        let src = "\
flow base:
    steps:
        - \"s1\":
            note = \"a\"

flow t uses base:
    steps:
        - \"s1\":
            note = \"b\"
";
        let (resolved, diags) = compose(schema, src, "flow", "t");
        assert!(
            !codes_of(&diags).contains(&codes::DEAD_DELTA),
            "the materialized token is pairing machinery for oneof \
             elements too: {diags:?}"
        );
        let body = resolved.unwrap().body;
        let has_name = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "steps" => Some(&nb.body),
                _ => None,
            })
            .is_some_and(|steps| {
                steps.entries.iter().any(|e| match &e.kind {
                    BodyEntryKind::ListItem(ListItem {
                        kind: ListItemKind::Shorthand { body: Some(b), .. },
                        ..
                    }) => scalar(b, "name").is_some(),
                    _ => false,
                })
            });
        assert!(has_name, "the + token materializes through the arm");
    }

    #[test]
    fn plan_beats_default_for_omitted_nested_discriminators() {
        // The pre-pass's reason to exist: a layer that OMITS a nested
        // discriminator inherits the stack's effective arm, so its
        // zero-item entry warns against that arm's vocabulary — under the
        // layer's own default-filled consult the field would be unknown
        // and the entry silently unclassified.
        let schema = "\
model azN:
    kind string
    hosts []string

model gcpN:
    kind string
    path string

oneof notify by kind = \"gcp\":
    \"az\" -> azN
    \"gcp\" -> gcpN

model svc:
    out notify
";
        let src = "\
svc base:
    out:
        kind = \"az\"
        hosts = [\"a\"]

svc t uses base:
    out:
        hosts = []
";
        let (_, diags) = compose(schema, src, "svc", "t");
        assert!(
            codes_of(&diags).contains(&codes::ZERO_ITEM_LAYER_ENTRY),
            "the omitted-discriminator layer normalizes against the \
             stack's arm, not the schema default: {diags:?}"
        );
    }
}
