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

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::ast::{
    ArmSelector, ArmTarget, BlockDecl, Body, BodyEntry, BodyEntryKind, DeclarationKind, File,
    Identifier, ListItem, ListItemKind, Modifier, ModifierValue, NestedBlock,
};
use crate::diagnostic::{Diagnostic, codes};
use crate::diff::Origin;
use crate::model::{FieldDef, FieldType, ModelDef, OneOfDef};
use crate::query::Document;
use crate::schema_index::{BodyShape, FieldTarget, NameableVariant, SchemaIndex};
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
            // A union element has plenty to key and merge — its
            // rejection is a narrower fact (identity-across-variants is
            // not yet defined), and the scalar-list wording would
            // mislead there.
            let msg = if inner.is_some_and(|i| i.union_variants().is_some()) {
                format!(
                    "'{}.{}': `#identity` on a union-element list is not \
                     supported — item identity across variants is not yet \
                     defined; drop `#identity` (the list then composes by \
                     overlay)",
                    model.name, field.name
                )
            } else {
                format!(
                    "'{}.{}': `#identity` needs items with something to key and \
                     something to merge — plain scalar lists and `set<T>` have \
                     neither (`#append` and overlay are the policies that mean \
                     something there); seal the item fields, not the list, when \
                     that is the intent",
                    model.name, field.name
                )
            };
            diags.push(at(msg));
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
    // A oneof ELEMENT's seals live in its arm models, and a UNION
    // element's in its variant models (nameable variants; a oneof
    // variant's in ITS arm models) — look through both, or a sealed-arm
    // element type slips the lint entirely.
    if policy_of(field) == MergePolicy::Overlay {
        let named_declares_seal = |name: &str| -> bool {
            ctx.model(name)
                .is_some_and(|m| model_declares_seal(ctx, m, &mut HashSet::new()))
                || ctx.oneof(name).is_some_and(|o| {
                    o.variants.iter().any(|(_, v)| {
                        ctx.model(v)
                            .is_some_and(|m| model_declares_seal(ctx, m, &mut HashSet::new()))
                    })
                })
        };
        // (sealed item type, whether `#identity` is a grantable fix —
        // it is not for a union ELEMENT (NML2068 rejects it today) nor
        // for a union's LIST VARIANT (policies attach to fields, not
        // variants), so those get honest advice instead of a dead end.)
        // (lead naming the shape, whether `#identity` is a grantable fix —
        // it is not for a union ELEMENT (NML2068 rejects it today) nor
        // for a union's LIST VARIANT (policies attach to fields, not
        // variants), so those get honest advice instead of a dead end.)
        let at_field = format!("'{}.{}'", model.name, field.name);
        let sealed_item: Option<(String, bool)> = match list_inner(effective_type(ty)) {
            Some(FieldType::ModelRef(item)) if named_declares_seal(item) => Some((
                format!("{at_field}: item model '{item}' declares `#sealed` fields"),
                true,
            )),
            Some(inner) => inner.union_variants().and_then(|variants| {
                variants.iter().find_map(|v| match v {
                    FieldType::ModelRef(n) if named_declares_seal(n) => Some((
                        format!(
                            "{at_field}: union-element item model '{n}' declares \
                             `#sealed` fields"
                        ),
                        false,
                    )),
                    _ => None,
                })
            }),
            // Not list-typed: a UNION field whose LIST/SET variant holds
            // a sealed element model — that list is necessarily bare
            // (list-over-list replaces wholesale), so the seals never
            // engage there either; only the variant-switch backstop
            // guards the position.
            None => {
                // Only the block-shape list variant is reachable (never a
                // set variant, never a second list variant); a promise for
                // an unreachable variant would be a false one — and the
                // backstop judges under the SAME variant this promises
                // (`union_block_list_variant`, one owner for both).
                union_block_list_variant(effective_type(ty)).and_then(|v| match list_inner(v) {
                    Some(FieldType::ModelRef(n)) if named_declares_seal(n) => Some((
                        format!(
                            "{at_field}: list variant `[]{n}` carries item model \
                             '{n}' with `#sealed` fields"
                        ),
                        false,
                    )),
                    _ => None,
                })
            }
        };
        if let Some((lead, identity_grantable)) = sealed_item {
            let advice = if identity_grantable {
                "grant the list `#identity` (and optionally `#append`) \
                 to make them reachable"
            } else {
                "`#identity` is not grantable at a union list position, so \
                 seal outside the list element (the variant-switch \
                 backstop still guards switches)"
            };
            let d = Diagnostic::warning(format!(
                "{lead}, but a bare-overlay list is replaced wholesale — \
                 the seals never engage; {advice}"
            ))
            .with_code(codes::UNREACHABLE_SEAL)
            .with_span(span);
            diags.push(match &model.source {
                Some(src) => d.with_source(src.clone()),
                None => d,
            });
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
    /// One-home guard for the discovery-depth NML2066: the guard fires
    /// once per frame otherwise (one chain, N over-cap instances, N
    /// identical-cause errors), and the clause-level depth report usually
    /// follows anyway.
    depth_reported: bool,
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
            if !self.depth_reported {
                self.depth_reported = true;
                let mut d = layer_bound_exceeded(LayerBound::Discovery { instance: id.name })
                    .with_source(id.source_path.to_string());
                // Anchor at the instance whose clause the recursion was
                // entering — a span-less diagnostic renders 0:0 in the
                // CLI and is DROPPED by the editor's span policy.
                if let Some(block) = self.instances.get(id) {
                    d = d.with_span(block.name.span);
                }
                self.diags.push(d);
            }
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
            let teach = if cycle.len() == 1 {
                format!(
                    "'{}' must not `uses` itself; remove the self-reference",
                    anchor.name
                )
            } else {
                "one of these is the base and must not `uses` the other(s); \
                 break the cycle"
                    .to_string()
            };
            let mut d = Diagnostic::error(format!(
                "`uses` reference cycle: {} — {teach}",
                path.join(" -> ")
            ))
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
        let Some(block) = self.instances.get(id) else {
            // The one failure the resolve path could previously return
            // WITHOUT a diagnostic — a vanishing instance with no
            // explanation. Unreachable through `compose_file` (it
            // resolves refs first), but `resolve_layers` is the
            // documented embedder entry point and its contract is
            // "every diagnostic in one pass".
            self.diags.push(
                Diagnostic::error(format!("layer '{}' is not in the instance index", id.name))
                    .with_code(codes::UNRESOLVED_LAYER_REF)
                    .with_source(id.source_path.to_string()),
            );
            return None;
        };
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
                        // The contradicting ref is listed after its own
                        // dependent, so it is never first: `deduped`
                        // preserves first occurrences and the pair loops
                        // iterate the redundant ref after the ref it
                        // contradicts. The resolver owns the bytes — the
                        // separator, the colon on a bodiless header, and
                        // every refusal.
                        if idx == 0 {
                            return None;
                        }
                        Some(blk.uses[idx].span)
                    });
                    if let Some(s) = sugg {
                        d = d.with_deletion(s);
                    }
                    if let Some(ab) = self.instances.get(*a) {
                        d = d.with_related_in(
                            ab.name.span,
                            format!("'{}' already composes '{}' here", a.name, b.name),
                            Some(a.source_path.to_string()),
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
        // Case 3: neither two-clause pattern holds — the pairwise orders
        // ROTATE across three or more clauses. A C3 failure guarantees a
        // cycle in the union "x above y" constraint graph (no valid head
        // = every candidate has an incoming edge), so name it: the old
        // generic fallback asserted the two patterns just ruled out,
        // misleading exactly the reader who reached it.
        let mut edges: HashMap<(&str, &str), &str> = HashMap::new();
        for r in deduped {
            if let Some(Some(lin)) = self.memo.get(r) {
                for (i, x) in lin.iter().enumerate() {
                    for y in &lin[i + 1..] {
                        edges.entry((x.name, y.name)).or_insert(r.name);
                    }
                }
            }
        }
        for (i, lo) in deduped.iter().enumerate() {
            for hi in &deduped[i + 1..] {
                // Listing order: later-listed sits above earlier-listed.
                edges.entry((hi.name, lo.name)).or_insert(id.name);
            }
        }
        if let Some(cycle) = find_order_cycle(&edges) {
            let steps: Vec<String> = cycle
                .windows(2)
                .map(|w| {
                    let src = edges.get(&(w[0], w[1])).copied().unwrap_or(id.name);
                    format!("'{}' above '{}' (per '{}')", w[0], w[1], src)
                })
                .collect();
            return Diagnostic::error(format!(
                "no consistent linearization for '{}' — the listed stacks' \
                 orders rotate: {}; no listing order can satisfy them all — \
                 drop one ref or align the contradicting stacks",
                id.name,
                steps.join(", ")
            ))
            .with_code(codes::INCONSISTENT_LINEARIZATION);
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
            if let GrantLookup::Granted {
                grant,
                binding,
                manifest,
            } = &site
            {
                let decision = self.grants.ref_decision(grant, target.source_path);
                let gref = GrantRef {
                    binding,
                    manifest,
                    file: id.source_path,
                };
                if let Some(d) = ref_denial(decision, &r.name, &gref, Denial::Site) {
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
    let d = Diagnostic::error(format!(
        "`uses` is an instance clause — a `{}` definition cannot \
         compose layers; delete the clause",
        block.keyword.name
    ))
    .with_code(codes::LAYER_KEYWORD_MISMATCH)
    .with_span(block.name.span)
    .with_source(source_path.to_string());
    // The promised fix (RFC 0019 plan): delete the clause — structural;
    // the resolver computes the bytes (the clause node with its leading
    // space, and the colon rule on a bodiless header). The primary span
    // stays the name.
    match block.uses_span {
        Some(span) => d.with_deletion(span),
        None => d,
    }
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
/// First cycle in a pairwise-order constraint graph, rendered as the node
/// path with the closing node repeated (`[x, y, z, x]`). Bounded tiny:
/// nodes ≤ the 16-layer cap.
fn find_order_cycle<'n>(edges: &HashMap<(&'n str, &'n str), &'n str>) -> Option<Vec<&'n str>> {
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for (above, below) in edges.keys() {
        adj.entry(above).or_default().push(below);
    }
    fn dfs<'n>(
        node: &'n str,
        adj: &HashMap<&'n str, Vec<&'n str>>,
        path: &mut Vec<&'n str>,
        done: &mut HashSet<&'n str>,
    ) -> Option<Vec<&'n str>> {
        if let Some(start) = path.iter().position(|n| *n == node) {
            let mut cycle: Vec<&str> = path[start..].to_vec();
            cycle.push(node);
            return Some(cycle);
        }
        if done.contains(node) {
            return None;
        }
        path.push(node);
        if let Some(nexts) = adj.get(node) {
            for next in nexts {
                if let Some(c) = dfs(next, adj, path, done) {
                    return Some(c);
                }
            }
        }
        path.pop();
        done.insert(node);
        None
    }
    let mut done: HashSet<&str> = HashSet::new();
    let mut nodes: Vec<&str> = adj.keys().copied().collect();
    nodes.sort_unstable();
    for n in nodes {
        if let Some(c) = dfs(n, &adj, &mut Vec::new(), &mut done) {
            return Some(c);
        }
    }
    None
}

/// The governing grant's identity, for the denial family's contract
/// tail (RFC 0019, "recovery paths are part of the contract"): every
/// denial names the binding AND its manifest file, states plainly that
/// the change is an operator's, and ends by pointing at
/// `nml binding <file>`.
struct GrantRef<'a> {
    binding: &'a str,
    manifest: &'a str,
    /// The checked file — interpolated into the recovery pointer.
    file: &'a str,
}

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
    grant: &GrantRef<'_>,
    scope: Denial<'_>,
) -> Option<Diagnostic> {
    let GrantRef {
        binding,
        manifest,
        file,
    } = grant;
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
    // The denial family's contract tail (RFC 0019): binding AND manifest
    // named, operator ownership stated, recovery pointer last.
    let tail = format!(" — an operator change, not fixable here; run `nml binding {file}`");
    let msg = match decision {
        RefDecision::Allowed => return None,
        RefDecision::DenyVeto(i) => format!(
            "`uses` ref '{ref_name}' denied by denyRefs[{i}] of binding \
             '{binding}' ({manifest}){suffix}{tail}"
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
                 binding '{binding}' ({manifest}) admits this layer{tail}"
            ),
            Denial::Stack { .. } => format!(
                "`uses` stack denied: no allowRefs entry of binding \
                 '{binding}' ({manifest}) admits a composed layer{suffix}{tail}"
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
    // The recovery pointer names the CHECKED file — a literal `<file>`
    // placeholder when the emitter knows the path is a wall, not a
    // doorway.
    let file = id.source_path;
    match lookup {
        GrantLookup::Granted { .. } | GrantLookup::Unbound { open_context: true } => None,
        GrantLookup::NoGrant { binding, manifest } => Some(base(format!(
            "composition not permitted: binding '{binding}' ({manifest}) \
             carries no `layers:` grant — an operator change, not fixable \
             from a content file; run `nml binding {file}` to see the \
             effective grant"
        ))),
        GrantLookup::Ambiguous { manifests } => Some(base(format!(
            "composition not permitted: {} manifests claim this file ({}) — \
             an ambiguously-claimed file is denied; remove or narrow one \
             claim, then run `nml binding {file}`",
            manifests.len(),
            manifests.join(", ")
        ))),
        GrantLookup::Unbound {
            open_context: false,
        } => Some(base(format!(
            "composition not permitted: no binding governs this file in a \
             closed universe — add a `files` glob that claims it (an \
             operator change), then run `nml binding {file}`"
        ))),
    }
}

// ───────────────────────────────────────────────────────── normalization ──

fn normalize_inlined(
    index: &SchemaIndex,
    root: &str,
    inlined: &Body,
    plan: &ArmPlan,
    source_path: &str,
    diags: &mut Vec<Diagnostic>,
) -> Body {
    let positional = crate::identity::apply_positional_planned(index, root, inlined, plan);
    let shared = crate::resolve::apply_shared_properties(&positional);
    let vocab = Vocab::of_model(named_vocab(index, root, &shared, "", plan));
    normalize_spellings(index, vocab, &shared, "", plan, source_path, diags)
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
fn candidate_variants(
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
fn scan_arm_bodies<'a>(
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

/// A list element's merge target, resolved once per list position —
/// see [`Merger::item_target`].
enum ItemTarget {
    Model(ModelDef),
    OneOf(OneOfDef),
    /// A union-typed element (RFC 0015): item groups compose by the
    /// union authority.
    Union(FieldType),
    /// No schema target (structural mode, scalar elements, a dangling
    /// name): model-less deep merge.
    Opaque,
}

/// One layer's decision at a position.
type Decision<'a> = (InstanceId<'a>, ArmDecision<'a>);
/// A position's per-layer decision trace — a fold's output, the merge's
/// replay input.
type DecisionTrace<'a> = Vec<Decision<'a>>;

/// One step of a seal's path relative to the judged position.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Seg {
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
struct FieldIdentity(Vec<Seg>);

impl FieldIdentity {
    fn child(&self, seg: Seg) -> Self {
        let mut segs = self.0.clone();
        segs.push(seg);
        FieldIdentity(segs)
    }

    /// The ONE join — `secret`, `[w].secret`, `nest.secret` joined at
    /// `position`: a dot for fields, brackets for items (the
    /// non-disclosing [`ItemKey::segment`]), nothing for arms (two
    /// arms' seals of one field are two FIELDS that render alike).
    fn at(&self, position: &str) -> String {
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
#[derive(Clone)]
struct SealHit<'a> {
    id: FieldIdentity,
    span: Span,
    layer: InstanceId<'a>,
}
/// The hits of one judgment — lowest-then-document order, so `.first()`
/// is the related span; fields are the distinct identities among them,
/// assignments the distinct `(file, span)` sites.
type SealHits<'a> = Vec<SealHit<'a>>;

/// The seal scan's sink: hits plus the (identity, file, span) set that
/// dedups them — the same assignment of the same field re-encountered
/// is one hit, but the identity stays in the key because one span IS
/// two fields when a list-level `.shared` line distributes into several
/// items, and two files can carry byte-identical spans at one identity
/// (a linear scan of the hits per hit was the O(hits²) term of a wide
/// judgment).
struct SealSink<'a> {
    hits: SealHits<'a>,
    seen: HashSet<(FieldIdentity, &'a str, usize, usize)>,
}

impl<'a> SealSink<'a> {
    fn new() -> Self {
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

/// RFC 0019 step 3's variant pre-pass result: the stack's effective arm
/// (and union variant) at each planned position, folded bottom-up over
/// the array-ref-inlined layer bodies — computed BEFORE per-layer
/// materialization so every layer normalizes against the variant the
/// stack actually composes, not its own default-filled guess. Keyed by
/// dotted field path from the instance root; `""` is a oneof root. List
/// items are their own single-item-group scopes and are deliberately not
/// planned (their vocabularies resolve per item).
///
/// `decisions`/`unions` carry the folds' full per-layer traces at each
/// planned position — the ONE decision authority. The merge REPLAYS a
/// trace rather than re-deriving decisions from its own (differently
/// normalized) view of the bodies: two independent accumulators over two
/// representations is exactly how positional machinery injected under
/// one arm fabricated seal refusals against another.
#[derive(Default)]
pub(crate) struct ArmPlan<'a> {
    arms: HashMap<String, String>,
    decisions: HashMap<String, DecisionTrace<'a>>,
    /// Union positions' plans (RFC 0015): established variant + the
    /// per-layer decision trace the merge replays. Same single-authority
    /// discipline as `decisions` — the merge never re-judges a variant
    /// the plan already judged.
    unions: HashMap<String, UnionPlan<'a>>,
}

/// One union position's plan (RFC 0015 nominal unions): the LOWEST layer
/// that supplies the value establishes the variant — its authored `as`
/// annotation, else its body shape — and the composed body carries that
/// variant as an explicit annotation from then on (synthesized when the
/// establishment was inferred, so the merged shape can never re-infer).
/// Layers above merge into the effective variant; only an authored `as`
/// naming a DIFFERENT variant switches — wholesale, subject to the seal
/// backstop, exactly like a oneof arm switch.
struct UnionPlan<'a> {
    /// The fold's establishment — every kind, not just named ones: the
    /// merge consumes the plan unconditionally.
    established: Establishment,
    decisions: DecisionTrace<'a>,
    /// The supply kinds the trace was folded over — a mechanical parity
    /// check for the merge's re-classification (replaces a prose
    /// invariant that raw and normalized bodies classify alike).
    kinds: Vec<SupplyKind>,
}

impl UnionPlan<'_> {
    /// Entry-for-entry alignment with the merge's supplies: same layer
    /// ids in the same order, same classification — see [`trace_aligns`].
    fn aligns(&self, ids: &[InstanceId<'_>], kinds: &[SupplyKind]) -> bool {
        trace_aligns(&self.decisions, ids) && self.kinds == kinds
    }
}

/// A planned trace aligns with a contribution group only entry-for-entry:
/// one layer may state a field twice (two contributions, two decisions,
/// ONE id), so an id-keyed lookup would collapse them and misapply the
/// first decision to both. Any structural mismatch (membership drift,
/// count drift) must fall back to a local refold — recomputing is always
/// safe; misapplying a stale decision is how a switch silently sticks or
/// a sealed write silently vanishes.
fn trace_aligns(trace: &DecisionTrace<'_>, ids: &[InstanceId<'_>]) -> bool {
    trace.len() == ids.len() && trace.iter().zip(ids).all(|((tid, _), lid)| tid == lid)
}

impl<'a> ArmPlan<'a> {
    /// The stack's effective oneof arm at `path`, when planned.
    pub(crate) fn planned_arm(&self, path: &str) -> Option<&str> {
        self.arms.get(path).map(String::as_str)
    }

    /// The stack's effective union variant at `path`, when planned and
    /// named (an ambiguous or structural establishment has no
    /// vocabulary to normalize under — no guess).
    pub(crate) fn planned_union_variant(&self, path: &str) -> Option<&str> {
        #[cfg(test)]
        PLAN_LOOKUPS.with(|l| l.borrow_mut().push(path.to_string()));
        match &self.unions.get(path)?.established {
            Establishment::Named { variant, .. } => Some(variant),
            _ => None,
        }
    }

    /// The union plan at `path`, when the position was planned.
    fn union_at(&self, path: &str) -> Option<&UnionPlan<'a>> {
        self.unions.get(path)
    }

    /// The planned oneof trace at `path`, only when it aligns with the
    /// contribution ids being merged — see [`trace_aligns`].
    fn aligned_decisions(&self, path: &str, ids: &[InstanceId<'a>]) -> Option<&DecisionTrace<'a>> {
        self.decisions.get(path).filter(|t| trace_aligns(t, ids))
    }
}

/// One layer's fate at one variant-typed position, decided by a fold.
enum ArmDecision<'a> {
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
    let positional = crate::identity::apply_positional_planned(index, &arm.name, body, &no_plan);
    let shared = crate::resolve::apply_shared_properties(&positional);
    normalize_spellings(
        index,
        Vocab::Model(arm),
        &shared,
        "",
        &no_plan,
        "",
        &mut Vec::new(),
    )
}

fn build_arm_plan<'a>(
    index: &SchemaIndex,
    root: &str,
    layers: &[(InstanceId<'a>, Body)],
) -> ArmPlan<'a> {
    let mut plan = ArmPlan::default();
    let bodies: Vec<(InstanceId<'a>, &Body)> = layers.iter().map(|(l, b)| (*l, b)).collect();
    // The root resolves in the ONE order every pass shares (a model before
    // a oneof of the same name — `SchemaIndex::nameable`); the plan and
    // the merge once read a colliding root oneof-first while normalization
    // and the validator read the model.
    match index.nameable(root) {
        Some(NameableVariant::OneOf(oneof)) => {
            let (arm, trace) = fold_arm_checked(index, oneof, &bodies);
            let survivors = surviving_entries(&trace, &bodies);
            plan.decisions.insert(String::new(), trace);
            if let Some(arm) = arm {
                let vocab = variant_model_of(index, oneof, &arm);
                plan.arms.insert(String::new(), arm);
                if let Some(m) = vocab {
                    arm_plan_walk(
                        index,
                        m,
                        &survivors,
                        "",
                        &mut plan,
                        Some(&oneof.discriminator),
                    );
                }
            }
        }
        Some(NameableVariant::Model(m)) => arm_plan_walk(index, m, &bodies, "", &mut plan, None),
        None => {}
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
    trace: &[Decision<'a>],
    bodies: &[(InstanceId<'a>, &'b Body)],
) -> Vec<(InstanceId<'a>, &'b Body)> {
    surviving_indexes(trace)
        .into_iter()
        .map(|i| bodies[i])
        .collect()
}

/// The survivorship rule, once: which positions of a trace SURVIVE its
/// decisions — a join or a pin keeps, a switch restarts the group with
/// itself, a rejection or a discard contributes nothing. The plan's two
/// walks and [`surviving_entries`] all read this one rule (four copies
/// of it were tied together by prose).
fn surviving_indexes(trace: &[Decision<'_>]) -> Vec<usize> {
    let mut group: Vec<usize> = Vec::new();
    for (i, (_, d)) in trace.iter().enumerate() {
        match d {
            ArmDecision::Join | ArmDecision::Pinned => group.push(i),
            ArmDecision::Switch => {
                group.clear();
                group.push(i);
            }
            ArmDecision::Rejected { .. } | ArmDecision::Discarded { .. } => {}
        }
    }
    group
}

/// Plan every union and oneof position under `model` over a group of
/// sibling bodies. `hidden_discriminator` is the enclosing oneof's
/// discriminator when the bodies are ARM bodies: its stated string
/// entries belong to the arm accumulator (the merge strips them before
/// merging the arm body — `without_discriminator`), so a union field
/// named like it gathers the same supply set on both sides — a filtered
/// view here, no cloned copies. Recursion passes `None` (the strip is
/// top-level only, exactly like the merge's).
fn arm_plan_walk<'a>(
    index: &SchemaIndex,
    model: &ModelDef,
    bodies: &[(InstanceId<'a>, &Body)],
    path: &str,
    plan: &mut ArmPlan<'a>,
    hidden_discriminator: Option<&str>,
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
        // Nothing under a `#sealed` position composes (write-once is
        // judged by `seal_write` alone, and the merge never consults the
        // plan there) — so nothing under it is planned: a plan would
        // hand normalization a REJECTED upper layer's variant for the
        // surviving lowest body.
        if policy_of(f) == MergePolicy::Sealed {
            continue;
        }
        let ety = effective_type(&f.field_type);
        // Union-typed position (RFC 0015): fold the variant decisions
        // here — the one authority — and recurse into the established
        // variant's vocabulary over the surviving group.
        if ety.union_variants().is_some() {
            // The SAME supply set the merge composes — every spelling —
            // so the planned trace aligns and replays (a trace folded
            // over nested bodies only never aligned once a whole-value
            // sibling existed, and the local refold then judged bodies
            // already normalized under the final variant).
            let entries = sub_entries_at(bodies, &f.name);
            let entries: Vec<(InstanceId<'a>, &BodyEntry)> = match hidden_discriminator {
                Some(disc) if disc == f.name => entries
                    .into_iter()
                    .filter(|(_, e)| !is_discriminator_named(e, disc))
                    .collect(),
                _ => entries,
            };
            if entries.is_empty() {
                continue;
            }
            let fpath = join_path(path, &f.name);
            debug_assert!(
                !fpath.contains('['),
                "plan keys are dotted field paths; an item scope ({fpath}) is never planned"
            );
            let supplies = union_supplies(index, ety, &entries);
            let (established, trace) = fold_variant_checked(index, ety, &supplies);
            // Nested positions plan over the surviving BODIES.
            let survivors: Vec<(InstanceId<'a>, &Body)> = surviving_indexes(&trace)
                .into_iter()
                .filter_map(|i| supplies[i].1.body().map(|b| (supplies[i].0, b)))
                .collect();
            // Every establishment is planned (the merge consumes the plan
            // unconditionally); only a NAMED one has a vocabulary to
            // recurse into — resolved in the ONE order every pass shares
            // (`resolve_ref`: a model before a oneof of the same name).
            if let Some(established) = established {
                if let Establishment::Named { variant, .. } = &established {
                    match index.nameable(variant) {
                        Some(NameableVariant::Model(m)) => {
                            arm_plan_walk(index, m, &survivors, &fpath, plan, None);
                        }
                        Some(NameableVariant::OneOf(oneof)) => {
                            // A oneof VARIANT: its arm decisions plan at the
                            // same path (the union body IS the oneof body).
                            let (arm, arm_trace) = fold_arm_checked(index, oneof, &survivors);
                            let arm_survivors = surviving_entries(&arm_trace, &survivors);
                            plan.decisions.insert(fpath.clone(), arm_trace);
                            if let Some(arm) = arm {
                                let vocab = variant_model_of(index, oneof, &arm);
                                plan.arms.insert(fpath.clone(), arm);
                                if let Some(nested) = vocab {
                                    arm_plan_walk(
                                        index,
                                        nested,
                                        &arm_survivors,
                                        &fpath,
                                        plan,
                                        Some(&oneof.discriminator),
                                    );
                                }
                            }
                        }
                        // A dangling variant name: nothing to recurse into
                        // (the validator reports it).
                        None => {}
                    }
                }
                let kinds = supplies.iter().map(|(_, s)| s.kind()).collect();
                plan.unions.insert(
                    fpath,
                    UnionPlan {
                        established,
                        decisions: trace,
                        kinds,
                    },
                );
            }
            continue;
        }
        let FieldType::ModelRef(n) = ety else {
            continue;
        };
        let subs = sub_bodies_at(bodies, &f.name);
        if subs.is_empty() {
            continue;
        }
        let fpath = join_path(path, &f.name);
        debug_assert!(
            !fpath.contains('['),
            "plan keys are dotted field paths; an item scope ({fpath}) is never planned"
        );
        match index.nameable(n) {
            Some(NameableVariant::Model(nested)) => {
                arm_plan_walk(index, nested, &subs, &fpath, plan, None);
            }
            Some(NameableVariant::OneOf(oneof)) => {
                let (arm, trace) = fold_arm_checked(index, oneof, &subs);
                let survivors = surviving_entries(&trace, &subs);
                plan.decisions.insert(fpath.clone(), trace);
                if let Some(arm) = arm {
                    let vocab = variant_model_of(index, oneof, &arm);
                    plan.arms.insert(fpath.clone(), arm);
                    if let Some(nested) = vocab {
                        arm_plan_walk(
                            index,
                            nested,
                            &survivors,
                            &fpath,
                            plan,
                            Some(&oneof.discriminator),
                        );
                    }
                }
            }
            None => {}
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
fn stated_variant(index: &SchemaIndex, union_ty: &FieldType, body: &Body) -> Option<String> {
    let variants = union_ty.union_variants()?;
    let name = body.type_annotation.as_ref()?.name.as_str();
    index
        .select_variant_by_type_name(variants, name)
        .map(|_| name.to_string())
}

fn union_variant_vocab<'i>(
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
#[derive(Clone, Debug)]
enum Establishment {
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
    fn clause(&self) -> String {
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
/// value. ONE constructor ([`union_supplies`]) serves the plan and the
/// merge, so the two fold over the same supply set and the plan's trace
/// always aligns — a trace folded over one set and replayed over another
/// is how a switch silently sticks or a refusal gets fabricated.
enum UnionSupply<'b> {
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
    fn classify(index: &SchemaIndex, union_ty: &FieldType, body: Cow<'b, Body>) -> Self {
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

    fn body(&self) -> Option<&Body> {
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

    /// The classification alone — what the plan records so the merge's
    /// re-classification can be checked mechanically against it.
    fn kind(&self) -> SupplyKind {
        match self {
            UnionSupply::Authored { .. } => SupplyKind::Authored,
            UnionSupply::Inferred { .. } => SupplyKind::Inferred,
            UnionSupply::Ambiguous { .. } => SupplyKind::Ambiguous,
            UnionSupply::Items { .. } => SupplyKind::Items,
            UnionSupply::Empty => SupplyKind::Empty,
            UnionSupply::Value => SupplyKind::Value,
        }
    }

    /// The variant this supply names or unambiguously selects — the one
    /// reading every normalization consumer takes (`None` for ambiguous
    /// and structural supplies: nothing to normalize under, no guess).
    fn nameable_variant(&self) -> Option<&str> {
        match self {
            UnionSupply::Authored { variant, .. } | UnionSupply::Inferred { variant, .. } => {
                Some(variant)
            }
            _ => None,
        }
    }
}

/// A supply's classification, recorded by the plan beside its trace.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SupplyKind {
    Authored,
    Inferred,
    Ambiguous,
    Items,
    Empty,
    Value,
}

/// The one supply constructor: every entry a group of sibling bodies
/// holds under `name`, in group order — the exact set the merge composes
/// and the plan folds.
fn union_supplies<'a, 'b>(
    index: &SchemaIndex,
    union_ty: &FieldType,
    entries: &[(InstanceId<'a>, &'b BodyEntry)],
) -> Vec<(InstanceId<'a>, UnionSupply<'b>)> {
    entries
        .iter()
        .map(|(l, e)| (*l, UnionSupply::classify_entry(index, union_ty, &e.kind)))
        .collect()
}

/// A VALUE contribution at a field: every spelling except a
/// type-annotation modifier (a declaration, inert in a bound instance —
/// it composes beside the value and never counts as one). The ONE
/// predicate the plan's gather and the merge's partition share, so the
/// two fold over the same supply set.
fn is_value_entry(kind: &BodyEntryKind) -> bool {
    !matches!(kind, BodyEntryKind::Modifier(m)
        if matches!(m.value, ModifierValue::TypeAnnotation { .. }))
}

/// The VALUE entries named `name` across a group of sibling bodies, in
/// group order, across EVERY spelling (property, nested block, modifier)
/// — the supply set of a union position at plan time, identical to the
/// merge's gather.
fn sub_entries_at<'b, 'a>(
    siblings: &[(InstanceId<'a>, &'b Body)],
    name: &str,
) -> Vec<(InstanceId<'a>, &'b BodyEntry)> {
    let mut out = Vec::new();
    for (id, b) in siblings {
        for e in &b.entries {
            let entry_name = match &e.kind {
                BodyEntryKind::Property(p) => Some(p.name.name.as_str()),
                BodyEntryKind::NestedBlock(nb) => Some(nb.name.name.as_str()),
                BodyEntryKind::Modifier(m) => Some(m.name.name.as_str()),
                _ => None,
            };
            if entry_name == Some(name) && is_value_entry(&e.kind) {
                out.push((*id, e));
            }
        }
    }
    out
}

/// The fold's policy verdict for one supply over the establishment in
/// force — RFC 0019's rules as a table, separated from the group/trace
/// bookkeeping that carries them out.
#[derive(Debug, PartialEq)]
enum Verdict {
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
fn union_verdict(est: Option<&Establishment>, supply: &UnionSupply<'_>) -> Verdict {
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

/// What the engine did with the contribution when an invariant broke.
#[derive(Clone, Copy)]
enum InvariantOutcome {
    /// The contribution was left out of the composed body.
    Dropped,
    /// Composition fell back to a local fold; the plan was ignored.
    Refolded,
}

/// Which merge owns a field group — [`Merger::merge_field`]'s ownership
/// order as data, so the order is a table a test enumerates cell by cell
/// ("sealed beats union beats all-modifier beats policy"), not an
/// arm-order accident.
#[derive(Clone, Copy, Debug)]
enum FieldRoute<'t> {
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
fn route_of<'t>(
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

/// The NML2086 diagnostic — a violated internal composition invariant,
/// named with its position (elided at an instance root) and what became
/// of the contribution. Pure, so its wording is testable in every
/// build; [`Merger::internal_invariant`] pairs it with the debug
/// assertion.
fn internal_invariant_diag(path: &str, what: &str, outcome: InvariantOutcome) -> Diagnostic {
    let position = if path.is_empty() {
        String::new()
    } else {
        format!(" at '{path}'")
    };
    let became = match outcome {
        InvariantOutcome::Dropped => "this layer's contribution was not composed",
        InvariantOutcome::Refolded => {
            "composition fell back to a local fold and the plan was ignored"
        }
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
struct DiscardSite<'a> {
    est_at: Span,
    est_layer: InstanceId<'a>,
    at: Span,
    layer: InstanceId<'a>,
}

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

/// The variant a union position composes BLOCK-shaped items under: the
/// FIRST `List` variant in source order — the resolver's own selection
/// for a list-item body (`resolve_type_in_body`), so the displaced-list
/// judgment, the NML2076 promise and the items' normalization vocabulary
/// all bind to exactly the variant the validator would. A set variant is
/// never selected by block shape (it is reachable only by array literal,
/// which carries scalars) and a second list variant is unreachable — a
/// judgment under either would judge the wrong list (a set variant ahead
/// of the list variant let a switch discard sealed items silently).
fn union_block_list_variant(ty: &FieldType) -> Option<&FieldType> {
    ty.union_variants()?
        .iter()
        .find(|v| matches!(v, FieldType::List(_)))
}

/// Whether a type admits item-bearing spellings — a list/set field, or
/// a union with ANY list-like variant (a set variant admits `= []`) —
/// the one gate for "is a zero-item entry a no-op here" (NML2079, the
/// union classifier, the seal-write predicate) so a union position is
/// treated like the list it can be.
fn admits_items(ty: &FieldType) -> bool {
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
fn zero_item_body_at(ty: &FieldType, body: &Body) -> bool {
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
fn zero_item_at(ty: Option<&FieldType>, kind: &BodyEntryKind) -> bool {
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

/// A body's diagnostic anchor when no entry span is known: its
/// annotation, else its first entry, else the file start — callers with
/// an entry or item span in hand (every live face) pass that instead.
fn body_anchor(body: &Body) -> Span {
    body.type_annotation
        .as_ref()
        .map(|i| i.span)
        .or_else(|| body.entries.first().map(|e| e.span))
        .unwrap_or_else(|| Span::empty(0))
}

/// The scan vocabulary of one list ELEMENT over an item group — a model
/// element directly, a oneof element under every arm the group could
/// have made effective (fail-closed, the schema default plus each
/// stated discriminator), a union element under every variant the
/// group could have established. One owner for the displaced-list
/// judgment and the general list scan.
fn list_element_vocab<'i>(
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
fn displaced_list_seals<'a>(
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

fn arm_body_vocab<'i>(
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

/// The bare-list rule's winner: the highest contribution that SUPPLIES
/// items (≥1 after normalization) replaces wholesale; zero-item entries
/// are warned no-ops; when no layer supplies items the field survives
/// authored-empty rather than dropping (a valid inherited `xs = []` must
/// not turn into a missing-required error). One owner for every list
/// spelling and for the list slice of a union position.
fn bare_list_winner<'c, 'a>(
    contributions: &[&'c Contribution<'a>],
) -> Option<&'c Contribution<'a>> {
    contributions
        .iter()
        .rev()
        .find(|c| items_of(&c.entry.kind).is_some_and(|v| !v.is_empty()))
        .or_else(|| {
            contributions
                .iter()
                .rev()
                .find(|c| items_of(&c.entry.kind).is_some())
        })
        .or_else(|| contributions.last())
        .copied()
}

/// Seal judgment over a displaced group, normalized under the DISPLACED
/// vocabulary — the one "what would the displaced compose carry?" scan
/// every backstop face asks ([`fold_arm_checked`],
/// [`fold_variant_checked`], [`Merger::merge_arm_set`]). Normalizing
/// under the surviving vocabulary instead counts that vocabulary's
/// machinery injections as authored writes; scanning raw bodies misses
/// `.shared`-distributed and positionally-materialized sealed writes.
/// Empty vocab ⇒ no seals (nothing nameable was displaced).
fn displaced_group_seals<'a>(
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
fn displaced_group_seals_into<'a>(
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
enum BackstopFace<'n> {
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
const RELATED_SEALS: usize = 4;

/// The one NML2060 backstop rejection — position named uniformly (elided
/// at an instance root, where there is no path to name), the seal
/// identity joined here through the ONE rule ([`FieldIdentity::at`]),
/// counted by FIELD (RFC 0019's promise: distinct identities) with the
/// assignment count when it EXCEEDS the fields (a distributed `.shared`
/// line has more fields than assignments — no suffix), and an
/// action-bearing tail. One
/// `sealed here` note per distinct assignment — the first
/// `RELATED_SEALS` — each carrying its own file (`Related.source`).
fn seal_backstop_rejection(
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

#[cfg(test)]
thread_local! {
    /// Test-only observability for the judgment memo (a DoS defence has
    /// no behavioral signature): the number of judgments actually
    /// computed rather than reused.
    static JUDGMENT_MISSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Test-only observability for the plan's consumers: every path
    /// normalization asked the plan about — the guard behind "every
    /// planned named position is consulted, and item scopes never ask
    /// for a container's key".
    static PLAN_LOOKUPS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Test-only seam at the compose boundary: a one-shot mutation of the
    /// plan `resolve_layers` just built — the only way to reach an
    /// internal invariant through the public API (every remaining site
    /// needs a corrupted plan, and `resolve_layers` builds its own), so
    /// the boundary assertion is PROVEN live rather than assumed.
    static PLAN_TAMPER: std::cell::Cell<Option<fn(&mut ArmPlan<'_>)>> = const { std::cell::Cell::new(None) };
}

/// THE variant-decision authority for union-typed positions (RFC 0015),
/// the exact sibling of [`fold_arm_checked`]: the rule table is
/// [`union_verdict`]; this loop only carries verdicts out — group and
/// trace bookkeeping, plus the seal judgment a switch must pass
/// ([`Establishment::displaced_seals`], judged over the displaced group
/// normalized under the DISPLACED vocabulary). Supplies come from the
/// one constructor ([`union_supplies`]); returns the establishment and
/// the per-layer trace the merge replays.
fn fold_variant_checked<'a, 'b>(
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

/// A stated discriminator entry — a string-valued property named like
/// the discriminator — the SELECTION predicate behind
/// [`stated_discriminator`] and [`stated_discriminator_entry`], so the
/// fold, the merge accumulator and the vocab pickers read exactly the
/// same entries. A non-string value is not a discriminator: it is a
/// type error the validator owns (NML2042, on every entry of that
/// name), never an arm selection. Stripping is a different job with a
/// different predicate — [`is_discriminator_named`].
fn is_discriminator_entry(entry: &BodyEntry, disc: &str) -> bool {
    matches!(&entry.kind, BodyEntryKind::Property(p)
        if p.name.name == disc && matches!(p.value.value, Value::String(_)))
}

/// ANY property named like the discriminator, whatever its value — the
/// STRIP predicate ([`without_discriminator`] and the plan's hidden-
/// discriminator filter, the only two strip sites). The validator's own
/// reading (`variant_body` hides every property of the name), so no
/// discriminator-named group ever forms in the model merge and the plan
/// gathers none — parity by construction. Non-string entries stripped
/// here pass through the composed view beside the canonical entry
/// ([`Merger::merge_oneof_bodies`]) so the validator reports each at
/// its author's span (RFC 0019 erratum E16).
fn is_discriminator_named(entry: &BodyEntry, disc: &str) -> bool {
    matches!(&entry.kind, BodyEntryKind::Property(p) if p.name.name == disc)
}

/// The string discriminator a body states, if any — the one reader for
/// "which arm did this layer name?", shared by the fold, the merge
/// accumulator, the vocab pickers, and the arm-set seal scans.
fn stated_discriminator(body: &Body, disc: &str) -> Option<String> {
    stated_discriminator_entry(body, disc).and_then(|e| match &e.kind {
        BodyEntryKind::Property(p) => p.value.value.as_str().map(str::to_string),
        _ => None,
    })
}

/// The discriminator entry a body states (the [`BodyEntry`] behind
/// [`stated_discriminator`]) — for callers that need its span.
fn stated_discriminator_entry<'b>(body: &'b Body, disc: &str) -> Option<&'b BodyEntry> {
    body.entries
        .iter()
        .find(|e| is_discriminator_entry(e, disc))
}

/// A oneof body without its discriminator-NAMED properties — the ONE
/// strip the merge applies before merging an arm body; the plan hides
/// the same entries from its supply gather (`arm_plan_walk`'s hidden
/// discriminator): a union field named like the discriminator (an
/// advisory NML2054 shape) must see the same supply set on both sides,
/// or the planned trace misaligns. By NAME, not by string-ness
/// ([`is_discriminator_named`]): a non-string entry is a type error,
/// not a field contribution, and letting it into the merge composed it
/// over siblings (`kind = 6` overlaying `kind = 5`) and fed the NML2054
/// union field a supply the validator says can never be set.
fn without_discriminator(body: &Body, disc: &str) -> Body {
    body.with_entries(
        body.entries
            .iter()
            .filter(|e| !is_discriminator_named(e, disc))
            .cloned()
            .collect(),
    )
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

/// The vocabulary a body normalizes under: a model's fields, or — for a
/// LIST body — its element type, resolved per ITEM by [`item_vocab`]. One
/// owner for "which model normalizes this body", so model, oneof and
/// union elements are peers (oneof- and union-element items had NO
/// vocabulary: their zero-item entries went unwarned, their arrays
/// un-re-spelled, unlike the same items under a model element).
#[derive(Clone, Copy)]
enum Vocab<'i> {
    None,
    Model(&'i ModelDef),
    /// A list body: the element type its items resolve under.
    Items(&'i FieldType),
}

impl<'i> Vocab<'i> {
    fn of_model(model: Option<&'i ModelDef>) -> Self {
        model.map_or(Vocab::None, Vocab::Model)
    }

    /// The model whose fields this body's entries are read by (`None`
    /// for a list body — its items carry their own).
    fn model(self) -> Option<&'i ModelDef> {
        match self {
            Vocab::Model(m) => Some(m),
            Vocab::None | Vocab::Items(_) => None,
        }
    }
}

/// The normalization vocabulary of a NAMED type: a model directly; a
/// oneof under the plan's arm at `path`, else the body's own stated
/// discriminator, else the schema default (or its list fields silently
/// keep their Property spelling and every policy misses them). One
/// owner (it was written twice), resolved in the one order
/// (`SchemaIndex::nameable`).
fn named_vocab<'i>(
    index: &'i SchemaIndex,
    name: &str,
    body: &Body,
    path: &str,
    plan: &ArmPlan,
) -> Option<&'i ModelDef> {
    match index.nameable(name)? {
        NameableVariant::Model(m) => Some(m),
        NameableVariant::OneOf(_) => oneof_vocab(index, name, body, path, plan),
    }
}

/// The vocabulary ONE list item's body normalizes under — the
/// Positionalizer's reading (`identity.rs`), so the two normalization
/// passes agree: a model element directly; a oneof element under the arm
/// the item's own body states, else the schema default (item scopes are
/// never planned); a union element under the variant the item's own
/// annotation or shape selects — and NONE when the D2 oracle calls it
/// ambiguous, so the resolver's first-wins guess never injects a
/// variant's machinery into a body compose refuses to assign one.
fn item_vocab<'i>(
    index: &'i SchemaIndex,
    element: &'i FieldType,
    body: &Body,
    path: &str,
    plan: &ArmPlan,
) -> Vocab<'i> {
    if let Some(variants) = element.union_variants() {
        if index.ambiguous_union_variants(variants, body).is_some() {
            return Vocab::None;
        }
    }
    match index.resolve_type_in_body(element, body) {
        FieldTarget::Model(m) => Vocab::Model(m),
        FieldTarget::OneOf(o) => Vocab::of_model(oneof_vocab(index, &o.name, body, path, plan)),
        _ => Vocab::None,
    }
}

/// Normalize one list item's body — under its own vocabulary for a list
/// body, at its own bracketed path (an item scope is never planned: the
/// plan keys dotted field paths, so a nested position inside an item can
/// never read the container's plan — the Positionalizer's `path: None`
/// rule, spelled as a path). Shared by the item and modifier-block
/// spellings, so both normalize alike.
fn normalize_item<'i>(
    index: &'i SchemaIndex,
    vocab: Vocab<'i>,
    item: &ListItem,
    path: &str,
    plan: &ArmPlan,
    source_path: &str,
    diags: &mut Vec<Diagnostic>,
) -> ListItem {
    let item_path = format!("{path}[{}]", ItemKey::of(&item.kind).segment());
    let body_vocab = |body: &Body| match vocab {
        Vocab::Items(element) => item_vocab(index, element, body, &item_path, plan),
        other => other,
    };
    let kind = match &item.kind {
        ListItemKind::Named { name, body } => ListItemKind::Named {
            name: name.clone(),
            body: normalize_spellings(
                index,
                body_vocab(body),
                body,
                &item_path,
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
                body_vocab(b),
                b,
                &item_path,
                plan,
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
                        let item_path = join_path(path, &m.name.name);
                        BodyEntryKind::Modifier(Modifier {
                            name: m.name.clone(),
                            value: ModifierValue::Block(
                                items
                                    .iter()
                                    .map(|item| {
                                        normalize_item(
                                            index,
                                            items_vocab,
                                            item,
                                            &item_path,
                                            plan,
                                            source_path,
                                            diags,
                                        )
                                    })
                                    .collect(),
                            ),
                        })
                    }
                    ModifierValue::TypeAnnotation { .. } => entry.kind.clone(),
                },
                BodyEntryKind::NestedBlock(nb) => {
                    let child_path = join_path(path, &nb.name.name);
                    let nb_field = field_map.get(nb.name.name.as_str()).copied();
                    let inner_vocab = nb_field.map_or(Vocab::None, |f| {
                        match effective_type(&f.field_type) {
                            // A model or oneof body (the oneof under the
                            // pre-pass's effective arm, else its own).
                            FieldType::ModelRef(name) => Vocab::of_model(named_vocab(
                                index,
                                name,
                                &nb.body,
                                &child_path,
                                plan,
                            )),
                            // A list body: each item resolves its own
                            // vocabulary from the element type.
                            FieldType::List(inner) | FieldType::Set(inner) => {
                                Vocab::Items(inner.as_ref())
                            }
                            // Union-typed position (RFC 0015): normalize
                            // against the planned variant (else this
                            // body's authored/inferred one) — same rule
                            // as planned oneof arms.
                            ty if ty.union_variants().is_some() => {
                                match UnionSupply::classify(index, ty, Cow::Borrowed(&nb.body)) {
                                    // Block-shaped items are their own
                                    // scope (never planned): they
                                    // normalize under the first `List`
                                    // variant's element type — the
                                    // resolver's selection for a
                                    // list-item body — exactly like a
                                    // plain list field's items, whatever
                                    // the position's plan says about
                                    // MODEL bodies there (a discarded
                                    // list under a named establishment is
                                    // still a list).
                                    UnionSupply::Items { .. } => union_block_list_variant(ty)
                                        .and_then(list_inner)
                                        .map_or(Vocab::None, Vocab::Items),
                                    // A model body: the planned variant,
                                    // else its own. An oracle-ambiguous,
                                    // unplanned body gets NO vocabulary:
                                    // normalizing under the resolver's
                                    // first-wins guess would inject that
                                    // variant's machinery (positional
                                    // tokens, zero-item verdicts) into a
                                    // body compose refuses to assign a
                                    // variant.
                                    supply => Vocab::of_model(
                                        plan.planned_union_variant(&child_path)
                                            .map(str::to_string)
                                            .or_else(|| {
                                                supply.nameable_variant().map(str::to_string)
                                            })
                                            .and_then(|v| {
                                                named_vocab(index, &v, &nb.body, &child_path, plan)
                                            }),
                                    ),
                                }
                            }
                            _ => Vocab::None,
                        }
                    });
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
                        body: normalize_spellings(
                            index,
                            inner_vocab,
                            &nb.body,
                            &child_path,
                            plan,
                            source_path,
                            diags,
                        ),
                    })
                }
                BodyEntryKind::ListItem(item) => BodyEntryKind::ListItem(normalize_item(
                    index,
                    vocab,
                    item,
                    path,
                    plan,
                    source_path,
                    diags,
                )),
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

/// Field path → origin: which layer's assignment produced each effective
/// entry. List items key by the identity pair (kind, token). Load-bearing
/// for NML2060's related spans, `--resolve-layers` attribution, the LSP's
/// resolved peek, and the embedder audit hash.
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
    #[cfg(test)]
    let plan = {
        let mut plan = plan;
        if let Some(tamper) = PLAN_TAMPER.with(|c| c.take()) {
            tamper(&mut plan);
        }
        plan
    };
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
#[derive(Clone)]
struct Contribution<'a> {
    layer: InstanceId<'a>,
    entry: BodyEntry,
}

/// A composed body plus the group-relative index of the HEAD of its
/// surviving group — the layer whose contribution the composed body IS:
/// the base when nothing switched; the switching layer after an
/// accepted switch; unchanged by a pin (a pin names, it does not
/// displace) or by a rejection. The head extends RFC 0019's receiver
/// rule from the body to the composed ENTRY (span, name identifier,
/// provenance row — erratum E15): the entry keeps the base SLOT
/// (replace in place), but a finding on the composed block anchors at
/// the layer that produced it.
struct Composed {
    body: Body,
    head: usize,
}

impl Composed {
    /// A merge with no switch semantics (model and model-less bodies):
    /// the head is the base.
    fn base(body: Body) -> Self {
        Composed { body, head: 0 }
    }
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
        // The one resolution order (`nameable`) — the plan resolved the
        // same root the same way.
        let index = self.index;
        match index.nameable(root) {
            Some(NameableVariant::OneOf(oneof)) => self.merge_oneof_bodies("", oneof, layers).body,
            Some(NameableVariant::Model(model)) => self.merge_model_bodies("", Some(model), layers),
            None => self.merge_model_bodies("", None, layers),
        }
    }

    /// Merge bodies against a model: one VALUE entry per field name (a
    /// declaration passes through ahead of it), replace in place at the
    /// base entry's position (`Body::with_entries` on the establishing
    /// layer's body — the receiver carries the annotation).
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
    ) -> Option<BodyEntry> {
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
    /// fix; on a sealed field this takes precedence over NML2084).
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
            // highest layer that SUPPLIES items (≥1 after normalization)
            // replaces wholesale; zero-item entries were already warned
            // and are no-ops — an escaped-normalization `xs = []` must
            // never EMPTY the base list through the scalar-overlay path.
            // When NO layer supplies items, the field survives
            // authored-empty rather than dropping (a valid inherited
            // `xs = []` must not turn into a missing-required error).
            let refs: Vec<&Contribution<'a>> = contributions.iter().collect();
            let winner = bare_list_winner(&refs)?;
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
        // Route by TARGET instead: gather the nested bodies and
        // deep-merge them. A dropped non-nested spelling is currently
        // SILENT (the substituted validation view never sees it —
        // tracked as the scalar-on-object validator gap); it still can
        // never win the field.
        let nested: Vec<(InstanceId<'a>, &NestedBlock)> = contributions
            .iter()
            .filter_map(|c| match &c.entry.kind {
                BodyEntryKind::NestedBlock(nb) => Some((c.layer, nb)),
                _ => None,
            })
            .collect();
        if let Some(FieldType::ModelRef(type_name)) = target {
            let index = self.index;
            // The one resolution order (`nameable`), same as the plan and
            // normalization: a colliding model/oneof name merges under
            // the reading it was planned under.
            let named = index.nameable(type_name);
            if let (Some(named), false) = (named, nested.is_empty()) {
                let sub: Vec<(InstanceId<'a>, Body)> =
                    nested.iter().map(|(l, nb)| (*l, nb.body.clone())).collect();
                let composed = match named {
                    NameableVariant::OneOf(oneof) => self.merge_oneof_bodies(path, oneof, &sub),
                    NameableVariant::Model(model) => {
                        Composed::base(self.merge_model_bodies(path, Some(model), &sub))
                    }
                };
                // The composed entry carries the HEAD contribution's span
                // and name — the base when nothing switched, the switching
                // layer after an accepted arm switch (RFC 0019 E15) — so a
                // finding on the composed block (a switched arm's missing
                // field) anchors at the layer that produced the body.
                let (head_layer, head_nb) = nested[composed.head];
                self.record(path, head_layer, head_nb.name.span);
                return Some(BodyEntry {
                    span: head_nb.name.span,
                    kind: BodyEntryKind::NestedBlock(NestedBlock {
                        name: head_nb.name.clone(),
                        body: composed.body,
                    }),
                });
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
            let item_bearing = nested.iter().any(|(_, nb)| {
                nb.body
                    .entries
                    .iter()
                    .any(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
            });
            if item_bearing {
                let refs: Vec<&Contribution<'a>> = contributions.iter().collect();
                let winner = bare_list_winner(&refs)?;
                self.record(path, winner.layer, winner.entry.span);
                return Some(winner.entry.clone());
            }
            let sub: Vec<(InstanceId<'a>, Body)> =
                nested.iter().map(|(l, nb)| (*l, nb.body.clone())).collect();
            let (base_layer, base_nb) = nested[0];
            let merged = self.merge_model_bodies(path, None, &sub);
            self.record(path, base_layer, base_nb.name.span);
            return Some(BodyEntry {
                span: base_nb.name.span,
                kind: BodyEntryKind::NestedBlock(NestedBlock {
                    name: base_nb.name.clone(),
                    body: merged,
                }),
            });
        }
        // Scalar overlay: later wins; a dead delta warns (NML2084).
        let refs: Vec<&Contribution<'a>> = contributions.iter().collect();
        self.scalar_overlay(path, &refs)
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

    /// Union compose (RFC 0015 nominal unions) at FIELD scope: every
    /// contribution is a supply through the ONE constructor
    /// ([`union_supplies`]), the establishment picks the output — a
    /// named-variant merge with an explicit annotation, a model-less
    /// un-annotated merge for an ambiguous group (NML2052 stays the
    /// validator's), or the structural overlay of the surviving whole
    /// values. THE PLAN IS THE AUTHORITY: its trace was folded over the
    /// raw, inlined supply set — the only representation a seal
    /// judgment may start from (the merge's bodies already carry the
    /// final variant's machinery, so a refold over them fabricates
    /// refusals). The merge re-classifies only to map indexes to bodies
    /// and spans, and checks that classification against the plan's
    /// recorded kinds: a planned position that does not align is an
    /// internal invariant failure (loud), with a local refold as the
    /// last resort; an unplanned position (inside list items, whose
    /// vocabularies resolve per item) folds locally — sound there
    /// because item bodies normalize under their own variant.
    fn merge_union(
        &mut self,
        path: &str,
        union_ty: &FieldType,
        contributions: &[Contribution<'a>],
    ) -> Option<BodyEntry> {
        let entries: Vec<(InstanceId<'a>, &BodyEntry)> =
            contributions.iter().map(|c| (c.layer, &c.entry)).collect();
        let supplies = union_supplies(self.index, union_ty, &entries);
        let ids: Vec<InstanceId<'a>> = supplies.iter().map(|(l, _)| *l).collect();
        let kinds: Vec<SupplyKind> = supplies.iter().map(|(_, s)| s.kind()).collect();
        let owned_trace;
        let (established, trace): (Option<Establishment>, &[Decision<'a>]) = match self
            .plan
            .union_at(path)
        {
            Some(up) if up.aligns(&ids, &kinds) => (Some(up.established.clone()), &up.decisions),
            planned => {
                if planned.is_some() {
                    let first = contributions.first()?;
                    self.internal_invariant(
                        path,
                        first.entry.span,
                        first.layer,
                        "a planned union position whose supplies do not align with its plan",
                        InvariantOutcome::Refolded,
                    );
                }
                let (est, t) = fold_variant_checked(self.index, union_ty, &supplies);
                owned_trace = t;
                (est, &owned_trace)
            }
        };
        let Some(established) = established else {
            // Zero-item entries only: nothing supplies the position — it
            // survives authored-empty rather than dropping (the bare-list
            // rule's own verdict). A block survivor is re-spelled `= []`,
            // the one spelling every consumer reads as the empty list
            // (an entry-less block reads as an empty OBJECT of the first
            // model variant downstream); a modifier survivor keeps its
            // spelling.
            let last = contributions.last()?;
            self.record(path, last.layer, last.entry.span);
            let (name, span) = match &last.entry.kind {
                BodyEntryKind::NestedBlock(nb) => (nb.name.clone(), nb.name.span),
                _ => return Some(last.entry.clone()),
            };
            return Some(BodyEntry {
                span: last.entry.span,
                kind: BodyEntryKind::Property(crate::ast::Property {
                    name,
                    value: SpannedValue::new(Value::Array(Vec::new()), span),
                }),
            });
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
        match established {
            Establishment::Named { .. } | Establishment::Ambiguous { .. } => {
                let Some(&est) = replay.group.first() else {
                    // Unreachable by construction (a body establishment
                    // has a body-bearing establishing supply) — fail
                    // safe and LOUD, never silently drop the field.
                    let last = contributions.last()?;
                    self.internal_invariant(
                        path,
                        last.entry.span,
                        last.layer,
                        "a body establishment with no establishing supply",
                        InvariantOutcome::Dropped,
                    );
                    return Some(last.entry.clone());
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
                    return Some(c.entry.clone());
                };
                let est_span = contributions[est].entry.span;
                let (out_body, head) = match &established {
                    Establishment::Named {
                        variant,
                        synthesized,
                    } => {
                        let composed = self.merge_variant_group(path, variant, &group);
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
                Some(BodyEntry {
                    span: head_span,
                    kind: BodyEntryKind::NestedBlock(NestedBlock {
                        name: head_nb.name.clone(),
                        body: out_body,
                    }),
                })
            }
            Establishment::Value | Establishment::Items => {
                let survivors: Vec<&Contribution<'a>> = replay
                    .structural
                    .iter()
                    .map(|&i| &contributions[i])
                    .collect();
                self.structural_overlay(path, &established, &survivors)
            }
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
            // in this exact order (plan alignment was checked by the
            // caller; the local fold produces one decision per supply).
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

    /// NML2086 — an internal composition invariant the engine holds to
    /// be unreachable was reached: fail safe and LOUD (the position
    /// elided at an instance root), never silently wrong. The module's
    /// precedent for believed-unreachable arms. The diagnostic makes the
    /// reach visible in every build; the compose boundary
    /// ([`resolve_layers`]) asserts on it in debug builds, so every
    /// compose-driven test stays loud while a direct `Merger` test can
    /// observe the diagnostic AND the fail-safe composition it
    /// describes in every build.
    fn internal_invariant(
        &mut self,
        path: &str,
        at: Span,
        layer: InstanceId<'a>,
        what: &str,
        outcome: InvariantOutcome,
    ) {
        self.diags.push(
            internal_invariant_diag(path, what, outcome)
                .with_span(at)
                .with_source(layer.source_path.to_string()),
        );
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
        self.diags.push(seal_backstop_rejection(
            BackstopFace::VariantSwitch { stated },
            path,
            seals,
            at,
            layer,
        ));
    }

    fn merge_variant_group(
        &mut self,
        path: &str,
        variant: &str,
        group: &[(InstanceId<'a>, Body)],
    ) -> Composed {
        let index = self.index;
        match index.nameable(variant) {
            Some(NameableVariant::OneOf(oneof)) => self.merge_oneof_bodies(path, oneof, group),
            Some(NameableVariant::Model(model)) => {
                Composed::base(self.merge_model_bodies(path, Some(model), group))
            }
            None => Composed::base(self.merge_model_bodies(path, None, group)),
        }
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
        self.diags.push(
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
    fn report_unknown_union_annotations<'b>(
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
            self.diags.push(
                self.index
                    .unknown_union_variant(variants, ann)
                    .with_source(layer.source_path.to_string()),
            );
        }
    }

    /// Union compose over identity-matched ITEM bodies — the body-level
    /// face of [`Self::merge_union`], with the same
    /// establishment/switch/backstop/annotation contract and the same
    /// replay. Item scopes are unplanned (their vocabularies resolve per
    /// item), so the fold always runs locally — sound because item
    /// bodies normalize under their OWN variant. Reachable under an
    /// identity policy on a union-element list (NML2068 at schema load,
    /// but composition still runs over the loaded schema and embedders
    /// may skip the loader's policy check), so it is a live face, not
    /// defense in depth. `anchors` are the items' own spans, one per
    /// body (an entry-less item body has no span of its own to anchor
    /// on).
    fn merge_union_bodies(
        &mut self,
        path: &str,
        union_ty: &FieldType,
        layers: &[(InstanceId<'a>, Body)],
        anchors: &[Span],
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
        let Some(established) = established else {
            // Zero-item bodies only: nothing to establish — model-less
            // merge keeps whatever (nothing) they carry; the head is the
            // base.
            return Composed::base(self.merge_model_bodies(path, None, layers));
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
                    return Composed::base(self.merge_model_bodies(path, None, layers));
                };
                let authored = replay
                    .annotated_by
                    .map(|i| &layers[i].1)
                    .unwrap_or(&layers[est].1)
                    .type_annotation
                    .clone();
                let composed = self.merge_variant_group(path, &variant, &group);
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
                Composed { body: merged, head }
            }
            Establishment::Ambiguous { .. } => Composed {
                body: self.merge_model_bodies(path, None, &group),
                head: replay.group.first().copied().unwrap_or(0),
            },
            // Structural item bodies: model-less deep merge of the
            // surviving slice — the pre-union-route verdict, preserved
            // for structural spellings. No single layer IS a deep-merged
            // body; the first structural survivor anchors.
            Establishment::Value | Establishment::Items => {
                let survivors: Vec<(InstanceId<'a>, Body)> = replay
                    .structural
                    .iter()
                    .map(|&i| layers[i].clone())
                    .collect();
                Composed {
                    body: self.merge_model_bodies(path, None, &survivors),
                    head: replay.structural.first().copied().unwrap_or(0),
                }
            }
        }
    }

    /// Plain overlay over a union position's structural survivors,
    /// dispatched on the establishment (a homogeneous slice by
    /// construction — the fold discards every cross-shape supply): a
    /// list establishment composes by the bare-list rule, a scalar one
    /// by scalar overlay (later wins, NML2084 on a dead delta) — the
    /// same two rules `merge_overlay` applies to structurally TYPED
    /// fields, through the same owners.
    fn structural_overlay(
        &mut self,
        path: &str,
        established: &Establishment,
        survivors: &[&Contribution<'a>],
    ) -> Option<BodyEntry> {
        match established {
            Establishment::Items => {
                let winner = bare_list_winner(survivors)?;
                self.record(path, winner.layer, winner.entry.span);
                Some(winner.entry.clone())
            }
            _ => self.scalar_overlay(path, survivors),
        }
    }

    /// Scalar overlay: later wins; a dead delta warns (NML2084). The one
    /// owner for the rule, shared by structurally typed fields and the
    /// scalar slice of a union position.
    fn scalar_overlay(
        &mut self,
        path: &str,
        contributions: &[&Contribution<'a>],
    ) -> Option<BodyEntry> {
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
    fn merge_arm_set(
        &mut self,
        path: &str,
        field: Option<&FieldDef>,
        contributions: &[Contribution<'a>],
    ) -> Option<BodyEntry> {
        let target_name = field.and_then(|f| match effective_type(&f.field_type) {
            FieldType::Arms { target, .. } => match target.as_ref() {
                FieldType::ModelRef(n) => Some(n.as_str()),
                _ => None,
            },
            _ => None,
        });
        let target = target_name.and_then(|n| self.index.nameable(n));
        let mut effective: Option<&Contribution<'a>> = None;
        for c in contributions {
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
            let Some(prev) = effective else {
                effective = Some(c);
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
                    self.diags.push(seal_backstop_rejection(
                        BackstopFace::ArmSetReplacement,
                        path,
                        &seals,
                        c.entry.span,
                        c.layer,
                    ));
                    // Rejected: this layer contributes nothing.
                    continue;
                }
            }
            effective = Some(c);
        }
        let winner = effective.or_else(|| contributions.last())?;
        self.record(path, winner.layer, winner.entry.span);
        Some(winner.entry.clone())
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
            // survives authored-empty.
            let winner = contributions
                .iter()
                .rev()
                .find(|c| !zero_item(&c.entry.kind))
                .or_else(|| contributions.last())?;
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
        let Some((base_layer, base_span, name, _)) = items_per_layer.first() else {
            // No item-bearing contribution at all (e.g. only a
            // type-annotation form): keep the last entry rather than
            // silently deleting the field from the composed body.
            let last = contributions.last()?;
            self.record(path, last.layer, last.entry.span);
            return Some(last.entry.clone());
        };
        let target = self.item_target(field);
        let merged = self.merge_items(
            path,
            policy,
            &target,
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
        let target = self.item_target(field);
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
        Some(BodyEntry {
            span: base.entry.span,
            kind,
        })
    }

    fn merge_items(
        &mut self,
        path: &str,
        policy: MergePolicy,
        target: &ItemTarget,
        per_layer: &[(InstanceId<'a>, Span, Vec<ListItem>)],
    ) -> Vec<ListItem> {
        // The classic split some sites still consume (shorthand-token
        // stripping); a union element derives neither — its variant is
        // per item group, not per position.
        let (item_model, item_oneof): (Option<&ModelDef>, Option<&OneOfDef>) = match target {
            ItemTarget::Model(m) => (Some(m), None),
            ItemTarget::OneOf(o) => (None, Some(o)),
            ItemTarget::Union(_) | ItemTarget::Opaque => (None, None),
        };
        // Identity-matched item bodies merge per the element's kind: a
        // oneof element routes through the arm accumulator (seal
        // enforcement, backstop), a union element through the union
        // authority (establishment, backstop, annotation synthesis), a
        // model element deep-merges.
        let merge_item_bodies = |me: &mut Self,
                                 item_path: &str,
                                 sub: &[(InstanceId<'a>, Body)],
                                 anchors: &[Span]|
         -> Composed {
            match target {
                ItemTarget::OneOf(oneof) => me.merge_oneof_bodies(item_path, oneof, sub),
                ItemTarget::Union(ty) => me.merge_union_bodies(item_path, ty, sub, anchors),
                ItemTarget::Model(m) => {
                    Composed::base(me.merge_model_bodies(item_path, Some(m), sub))
                }
                ItemTarget::Opaque => Composed::base(me.merge_model_bodies(item_path, None, sub)),
            }
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
                                        ListItemKind::Named {
                                            name: hi_name,
                                            body: hi,
                                        },
                                    ) => {
                                        let sub = vec![
                                            (*existing_layer, lo.clone()),
                                            (*layer, hi.clone()),
                                        ];
                                        let item_path =
                                            format!("{path}[{}]", existing_key.segment());
                                        let composed = merge_item_bodies(
                                            self,
                                            &item_path,
                                            &sub,
                                            &[existing_item.span, item.span],
                                        );
                                        // The merged item keeps the BASE
                                        // slot (replace in place); its
                                        // span, name identifier and owner
                                        // follow the HEAD of the surviving
                                        // group — the base when nothing
                                        // switched, the switching item
                                        // after an accepted switch
                                        // (RFC 0019 E15).
                                        let (span, owner, name) = if composed.head == 0 {
                                            (existing_item.span, *existing_layer, name.clone())
                                        } else {
                                            (item.span, *layer, hi_name.clone())
                                        };
                                        resolved[pos] = (
                                            existing_key.clone(),
                                            ListItem {
                                                kind: ListItemKind::Named {
                                                    name,
                                                    body: composed.body,
                                                },
                                                span,
                                            },
                                            owner,
                                        );
                                    }
                                    (
                                        ListItemKind::Shorthand { value, body: lo },
                                        ListItemKind::Shorthand {
                                            value: hi_value,
                                            body: hi,
                                        },
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
                                                // A union element's `+`
                                                // field lives on the
                                                // variant the item
                                                // unambiguously selects
                                                // (an ambiguous item was
                                                // never materialized).
                                                let ItemTarget::Union(ty) = target else {
                                                    return None;
                                                };
                                                [&lo_body, &hi_body].into_iter().find_map(|b| {
                                                    UnionSupply::classify(
                                                        self.index,
                                                        ty,
                                                        Cow::Borrowed(b),
                                                    )
                                                    .nameable_variant()
                                                    .and_then(|v| self.index.model(v))
                                                    .and_then(|m| {
                                                        m.fields.iter().find(|f| f.shorthand)
                                                    })
                                                })
                                            })
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
                                        let composed = merge_item_bodies(
                                            self,
                                            &item_path,
                                            &sub,
                                            &[existing_item.span, item.span],
                                        );
                                        // Span, value token and owner
                                        // follow the head (RFC 0019 E15);
                                        // the base slot is kept.
                                        let (span, owner, value) = if composed.head == 0 {
                                            (existing_item.span, *existing_layer, value.clone())
                                        } else {
                                            (item.span, *layer, hi_value.clone())
                                        };
                                        resolved[pos] = (
                                            existing_key.clone(),
                                            ListItem {
                                                kind: ListItemKind::Shorthand {
                                                    value,
                                                    body: Some(composed.body),
                                                },
                                                span,
                                            },
                                            owner,
                                        );
                                    }
                                    // Reference / role items are immutable:
                                    // identical restatement is a no-op.
                                    (ListItemKind::Reference(_), ListItemKind::Reference(_))
                                    | (ListItemKind::Role(_), ListItemKind::Role(_)) => {}
                                    _ => {
                                        // Believed unreachable: `same()`
                                        // matched first, the cross-kind
                                        // guard above exits with NML2063,
                                        // and the four same-kind pairs
                                        // are covered. But this engine
                                        // processes UNTRUSTED input and
                                        // ten review rounds have watched
                                        // "can't happen" invariants break
                                        // under later edits — so fail
                                        // SAFE (the cross-kind wording,
                                        // debug-asserted), never abort.
                                        debug_assert!(
                                            false,
                                            "identity merge saw a \
                                             cross-kind pair past the \
                                             same-kind guard"
                                        );
                                        self.diags.push(
                                            Diagnostic::error(format!(
                                                "item matches an existing \
                                                 identity in '{path}' \
                                                 across item kinds — match \
                                                 the base's spelling"
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
    /// fabricated refusals against another. The composed entry follows
    /// the HEAD of the surviving group ([`Composed`]), derived from
    /// [`surviving_indexes`] — the one survivorship rule, never a second
    /// accumulator.
    fn merge_oneof_bodies(
        &mut self,
        path: &str,
        oneof: &OneOfDef,
        layers: &[(InstanceId<'a>, Body)],
    ) -> Composed {
        // A planned trace is replayed POSITIONALLY, and only when it
        // aligns entry-for-entry with the contributions being merged —
        // see [`trace_aligns`]; any drift falls back to a local fold
        // over these bodies.
        let ids: Vec<InstanceId<'a>> = layers.iter().map(|(l, _)| *l).collect();
        let owned_trace;
        let trace: &[Decision<'a>] = match self.plan.aligned_decisions(path, &ids) {
            Some(t) => t,
            None => {
                let refs: Vec<(InstanceId<'a>, &Body)> =
                    layers.iter().map(|(l, b)| (*l, b)).collect();
                owned_trace = fold_arm_checked(self.index, oneof, &refs).1;
                &owned_trace
            }
        };
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
                    self.diags.push(seal_backstop_rejection(
                        BackstopFace::ArmSwitch {
                            discriminator: &oneof.discriminator,
                            stated: &stated,
                        },
                        path,
                        seals,
                        at,
                        *layer,
                    ));
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
        // its own message and exclude it (the union face keeps pins;
        // RFC 0025 promotes this filter to a parameter of the one rule).
        let survivors: Vec<usize> = surviving_indexes(trace)
            .into_iter()
            .filter(|&i| !matches!(trace[i].1, ArmDecision::Pinned))
            .collect();
        debug_assert!(
            layers.is_empty() || !surviving_indexes(trace).is_empty(),
            "index 0 is always Switch or Join, so a non-empty stack has a survivor"
        );
        if !layers.is_empty() && survivors.is_empty() {
            // Reachable only under a tampered trace (index 0 is Switch or
            // Join on every real fold): every layer was diagnosed above as
            // not composed — compose what the diagnostics say.
            return Composed {
                body: Body::fresh(Vec::new()),
                head: 0,
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
        // the discriminator; one canonical entry is re-added below).
        let stripped: Vec<(InstanceId<'a>, Body)> = group
            .iter()
            .map(|(l, b)| (*l, without_discriminator(b, &oneof.discriminator)))
            .collect();
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
        Composed { body: merged, head }
    }

    fn variant_model(&self, oneof: &OneOfDef, discriminator: &str) -> Option<ModelDef> {
        variant_model_of(self.index, oneof, discriminator).cloned()
    }
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
fn push_model<'i>(vocab: &mut Vec<&'i ModelDef>, m: &'i ModelDef) {
    if !vocab.iter().any(|x| x.name == m.name) {
        vocab.push(m);
    }
}

fn seal_scan_body<'a>(
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
enum ItemKey {
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
    fn segment(&self) -> String {
        match self {
            ItemKey::Named(n) | ItemKey::Reference(n) | ItemKey::Role(n) => n.clone(),
            ItemKey::Scalar(v) => v.type_name().to_string(),
        }
    }

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

    /// The layer stack of a single-file instance index, by name — for
    /// tests that observe the plan or drive the merger directly.
    fn layers_of<'i>(instances: &'i InstanceIndex, names: &[&str]) -> Vec<(InstanceId<'i>, Body)> {
        names
            .iter()
            .map(|n| {
                let id = instances.resolve_ref(n).unwrap();
                (id, instances.get(id).unwrap().body.clone())
            })
            .collect()
    }

    /// The first NAMED item's body of a list body.
    fn first_named_item_body(list: &Body) -> &Body {
        list.entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::ListItem(ListItem {
                    kind: ListItemKind::Named { body, .. },
                    ..
                }) => Some(body),
                _ => None,
            })
            .expect("a named item")
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
        // The denial CLAUSE never names the denied target's path; the
        // recovery tail names the CHECKED file (the author's own — the
        // contract's `nml binding <file>` pointer), which in this
        // single-file harness is the same string — so split the tail off
        // before asserting non-disclosure.
        let clause = diags[0]
            .message
            .split(" — an operator change")
            .next()
            .unwrap();
        assert!(
            !clause.contains("main.nml"),
            "allow-miss never names the denied path: {clause}"
        );
        assert!(
            diags[0].message.ends_with("run `nml binding main.nml`"),
            "recovery pointer names the checked file: {}",
            diags[0].message
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

    #[test]
    fn non_string_discriminators_pass_through_at_the_front() {
        // Part C (RFC 0019 E16): stripping is by NAME, so non-string
        // discriminator entries never compose over each other — the
        // surviving group's non-string entries pass through in layer
        // order, FIRST when no survivor states a string discriminator,
        // and carry no provenance row (validator-facing, never
        // effective).
        let src = "\
notify base:
    kind = 5
    path = \"p\"

notify top uses base:
    kind = 6
";
        let (resolved, diags) = compose(ONEOF_SCHEMA, src, "notify", "top");
        assert!(diags.is_empty(), "{diags:?}");
        let resolved = resolved.unwrap();
        let kind_of = |e: &BodyEntry| match &e.kind {
            BodyEntryKind::Property(p) if p.name.name == "kind" => Some(p.value.value.clone()),
            _ => None,
        };
        let front: Vec<Option<Value>> = resolved.body.entries.iter().take(2).map(kind_of).collect();
        assert_eq!(
            front,
            vec![Some(Value::number(5)), Some(Value::number(6))],
            "passthroughs first, in layer order: {:?}",
            resolved.body.entries
        );
        assert!(
            !resolved.origins.iter().any(|(p, _)| p == "kind"),
            "no provenance row for a passthrough: {:?}",
            resolved.origins
        );
    }

    #[test]
    fn a_restated_string_discriminator_composes_to_one_canonical_entry() {
        // First-wins for STRING entries — a later restatement is neither
        // canonical nor passed through — while a non-string sibling
        // passes through beside the canonical entry.
        let src = "\
notify base:
    kind = \"az\"
    azureUrl = \"https://a\"
    azureKey = \"k\"

notify top uses base:
    kind = \"az\"
    kind = 7
";
        let (resolved, diags) = compose(ONEOF_SCHEMA, src, "notify", "top");
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
        let strings = kinds
            .iter()
            .filter(|v| matches!(v, Value::String(_)))
            .count();
        assert_eq!(strings, 1, "one canonical string entry: {kinds:?}");
        assert_eq!(kinds.len(), 2, "plus the non-string passthrough: {kinds:?}");
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
            seal.message.contains("(and 1 more field)"),
            "states the field count: {}",
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
            seal.message.contains("(and 1 more field)"),
            "two items' seals are two fields, not one masked path: {}",
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
        let gref = GrantRef {
            binding: "tenantFlows",
            manifest: "site/nml.binding.toml",
            file: "tenants/cu-x/a.flow.nml",
        };
        let d = ref_denial(
            RefDecision::AllowMiss,
            "secretName",
            &gref,
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
        // The denial-family contract tail: binding AND manifest named,
        // operator ownership stated, recovery pointer with the real path.
        assert!(
            d.message.contains("(site/nml.binding.toml)"),
            "manifest named: {}",
            d.message
        );
        assert!(
            d.message
                .ends_with("run `nml binding tenants/cu-x/a.flow.nml`"),
            "recovery pointer last, real path: {}",
            d.message
        );
        let d = ref_denial(RefDecision::AllowMiss, "ownRef", &gref, Denial::Site).unwrap();
        assert!(
            d.message.contains("ownRef"),
            "site names the author's token"
        );
        let d = ref_denial(
            RefDecision::DenyVeto(2),
            "x",
            &gref,
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
    fn nml2077_deletion_targets_the_refs_own_span() {
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
            sugg.kind,
            crate::diagnostic::SuggestionKind::Delete,
            "structural, not verbatim"
        );
        assert_eq!(
            &src[sugg.span.start..sugg.span.end],
            "b",
            "the ref's own span — the resolver owns the separator bytes"
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
        // ONE authored `.val` write distributed into two items is ONE
        // assignment on TWO fields (RFC 0019 counts fields — E17): the
        // identity keeps the items apart even though both injected
        // copies carry the authored span and render `xs[string].val`
        // alike — and ONE note points at the one authored site.
        assert!(
            seal.message.contains("(and 1 more field)"),
            "two distributed fields: {}",
            seal.message
        );
        assert_eq!(
            seal.related.len(),
            1,
            "one note per distinct assignment: {seal:?}"
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

    // ── round-9 review pins ──────────────────────────────────────────────

    #[test]
    fn schemaless_nested_groups_still_deep_merge() {
        // Structural (no-schema) composition is a documented,
        // fixture-pinned capability: all-nested groups with no resolvable
        // object target — no schema, an undeclared field, a dangling
        // type — deep-merge name-keyed. The target-routed object path
        // must not drop them to wholesale replacement (silently
        // discarding every lower layer's nested data).
        let src = "\
box base:
    cfg:
        x = \"1\"
        sub:
            deep = \"d\"

box t uses base:
    cfg:
        y = \"2\"
";
        // No schema at all: compose structurally.
        let (resolved, diags) = compose("", src, "box", "t");
        assert!(diags.is_empty(), "structural compose is clean: {diags:?}");
        let body = resolved.unwrap().body;
        assert_eq!(
            nested_scalar(&body, "cfg", "x"),
            Some(&Value::String("1".into())),
            "the base's nested data survives"
        );
        assert_eq!(
            nested_scalar(&body, "cfg", "y"),
            Some(&Value::String("2".into())),
            "the overlay deep-merges in"
        );
        let sub_deep = body.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "cfg" => {
                nested_scalar(&nb.body, "sub", "deep")
            }
            _ => None,
        });
        assert_eq!(
            sub_deep,
            Some(&Value::String("d".into())),
            "recursively, not just one level"
        );
        // Dangling type name: same contract.
        let schema = "model box:\n    cfg ghost\n";
        let (resolved, _) = compose(schema, src, "box", "t");
        let body = resolved.unwrap().body;
        assert_eq!(
            nested_scalar(&body, "cfg", "x"),
            Some(&Value::String("1".into()))
        );
        assert_eq!(
            nested_scalar(&body, "cfg", "y"),
            Some(&Value::String("2".into()))
        );
    }

    // ── union compose (RFC 0015) ─────────────────────────────────────────

    const UNION_SCHEMA: &str = "\
model ua:
    x string
    secret string #sealed

model ub:
    y string

model holder:
    slot (ua | ub)
    label string
";

    fn slot_annotation(body: &Body) -> Option<String> {
        body.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "slot" => {
                nb.body.type_annotation.as_ref().map(|i| i.name.clone())
            }
            _ => None,
        })
    }

    #[test]
    fn union_establishment_and_unannotated_merge() {
        // The lowest supplying layer establishes (authored `as`); an
        // un-annotated upper body NEVER switches, whatever its shape —
        // it deep-merges into the effective variant.
        let src = "\
holder base:
    slot as ua:
        x = \"1\"
    label = \"a\"

holder t uses base:
    slot:
        y = \"2\"
";
        let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
        assert!(diags.is_empty(), "clean merge: {diags:?}");
        let body = resolved.unwrap().body;
        assert_eq!(
            slot_annotation(&body).as_deref(),
            Some("ua"),
            "the composed body carries the effective variant explicitly"
        );
        assert_eq!(
            nested_scalar(&body, "slot", "x"),
            Some(&Value::String("1".into())),
            "base survives"
        );
        assert_eq!(
            nested_scalar(&body, "slot", "y"),
            Some(&Value::String("2".into())),
            "the un-annotated upper deep-merges (its mis-typed fields are \
             the validator's business, never a silent switch)"
        );
    }

    #[test]
    fn union_shape_establishment_synthesizes_only_where_d2_would_allow() {
        // The D2 oracle calls an un-annotated keyed body under a
        // ≥2-nameable union AMBIGUOUS — compose must not guess a variant
        // and synthesize an annotation that would silence the
        // validator's fail-closed NML2052. The composed body stays
        // un-annotated, exactly as ambiguous as the authored one.
        let src = "\
holder base:
    slot:
        x = \"1\"

holder t uses base:
    slot:
        x = \"2\"
";
        let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
        assert!(
            diags.is_empty(),
            "compose is silent; D2 is the validator's: {diags:?}"
        );
        let body = resolved.unwrap().body;
        assert_eq!(
            slot_annotation(&body),
            None,
            "no guessed variant, no synthesized annotation — NML2052 \
             fires on the composed view"
        );
        assert_eq!(
            nested_scalar(&body, "slot", "x"),
            Some(&Value::String("2".into())),
            "the ambiguous group still deep-merges model-less"
        );

        // A DISJOINT union (one nameable variant) is never ambiguous:
        // shape establishment synthesizes there, so the merged shape can
        // never re-infer a different variant.
        const DISJOINT: &str = "\
model ua:
    x string

model holder6:
    slot (ua | string)
";
        let src2 = "\
holder6 base:
    slot:
        x = \"1\"

holder6 t uses base:
    slot:
        x = \"2\"
";
        let (resolved, diags) = compose(DISJOINT, src2, "holder6", "t");
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ua"),
            "synthesized annotation pins the unambiguously inferred variant"
        );
    }

    #[test]
    fn union_authored_switch_replaces_wholesale() {
        let src = "\
holder base:
    slot as ub:
        y = \"1\"

holder t uses base:
    slot as ua:
        x = \"2\"
";
        let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
        assert!(diags.is_empty(), "legal switch: {diags:?}");
        let body = resolved.unwrap().body;
        assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
        assert_eq!(
            nested_scalar(&body, "slot", "y"),
            None,
            "wholesale: nothing of the displaced arm survives"
        );
        assert_eq!(
            nested_scalar(&body, "slot", "x"),
            Some(&Value::String("2".into()))
        );
    }

    #[test]
    fn union_switch_discarding_a_seal_is_backstopped() {
        let src = "\
holder base:
    slot as ua:
        secret = \"locked\"

holder t uses base:
    slot as ub:
        y = \"2\"
";
        let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("the union switch is backstopped like a oneof switch");
        assert!(
            d.message.contains("variant switch to `as ub`") && d.message.contains("secret"),
            "names the switch and the seal: {}",
            d.message
        );
        let body = resolved.unwrap().body;
        assert_eq!(
            nested_scalar(&body, "slot", "secret"),
            Some(&Value::String("locked".into())),
            "the sealed variant survives the rejected switch"
        );
        assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
    }

    // ── arm-set compose (RFC 0007) ───────────────────────────────────────

    const ARM_SET_SCHEMA: &str = "\
model handler:
    note string
    token string #sealed

model router:
    route (string -> handler)
    label string
";

    #[test]
    fn arm_set_replacement_is_wholesale_and_backstopped() {
        // v1: a layer that states the field replaces the WHOLE set.
        let src = "\
router base:
    route:
        \"a\" -> One:
            note = \"n1\"

router t uses base:
    route:
        \"b\" -> Two:
            note = \"n2\"
";
        let (resolved, diags) = compose(ARM_SET_SCHEMA, src, "router", "t");
        assert!(diags.is_empty(), "clean replacement: {diags:?}");
        let body = resolved.unwrap().body;
        let arm_count = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "route" => Some(
                    nb.body
                        .entries
                        .iter()
                        .filter(|e| matches!(e.kind, BodyEntryKind::Arm(_)))
                        .count(),
                ),
                _ => None,
            })
            .unwrap_or(0);
        assert_eq!(arm_count, 1, "whole-set replacement, no accumulation");

        // ...subject to the seal backstop: a displaced set whose inline
        // bodies carry an assigned sealed field refuses the replacement.
        let src2 = "\
router base:
    route:
        \"a\" -> One:
            token = \"locked\"

router t uses base:
    route:
        \"b\" -> Two:
            note = \"n2\"
";
        let (resolved, diags) = compose(ARM_SET_SCHEMA, src2, "router", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("arm-set replacement is backstopped");
        assert!(
            d.message.contains("arm-set replacement") && d.message.contains("token"),
            "names the replacement and the seal: {}",
            d.message
        );
        let body = resolved.unwrap().body;
        let kept: Vec<String> = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "route" => Some(
                    nb.body
                        .entries
                        .iter()
                        .filter_map(|e| match &e.kind {
                            BodyEntryKind::Arm(a) => match &a.target {
                                ArmTarget::Inline { name, .. } => Some(name.name.clone()),
                                _ => None,
                            },
                            _ => None,
                        })
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default();
        assert_eq!(kept, vec!["One"], "the sealed set survives: {kept:?}");
    }

    // ── union compose: round-12 battery (backstop depth, structural
    //    variants, oneof-target arm sets, list-of-union items) ──────────

    fn sub_block<'b>(body: &'b Body, name: &str) -> Option<&'b Body> {
        body.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == name => Some(&nb.body),
            _ => None,
        })
    }

    #[test]
    fn union_restated_effective_variant_joins() {
        // Restating the effective variant (authored over authored, or
        // authored over shape-established) is a Join, never a switch.
        let src = "\
holder base:
    slot:
        x = \"1\"

holder t uses base:
    slot as ua:
        x = \"2\"
";
        let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
        assert!(diags.is_empty(), "restatement joins: {diags:?}");
        let body = resolved.unwrap().body;
        assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
        assert_eq!(
            nested_scalar(&body, "slot", "x"),
            Some(&Value::String("2".into()))
        );
    }

    #[test]
    fn union_switch_after_merge_displaces_the_whole_group() {
        // establish → merge → switch: the switch displaces the whole
        // accumulated group, not just the establishing layer.
        let src = "\
holder base:
    slot as ua:
        x = \"1\"

holder mid uses base:
    slot:
        x = \"2\"

holder t uses mid:
    slot as ub:
        y = \"3\"
";
        let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
        assert!(diags.is_empty(), "legal switch: {diags:?}");
        let body = resolved.unwrap().body;
        assert_eq!(slot_annotation(&body).as_deref(), Some("ub"));
        assert_eq!(nested_scalar(&body, "slot", "x"), None, "wholesale");
        assert_eq!(
            nested_scalar(&body, "slot", "y"),
            Some(&Value::String("3".into()))
        );
    }

    #[test]
    fn union_merge_after_rejected_switch_targets_the_original_variant() {
        // A rejected switch contributes NOTHING (wholesale), and a later
        // un-annotated layer merges into the ORIGINAL variant.
        let src = "\
holder base:
    slot as ua:
        secret = \"locked\"

holder mid uses base:
    slot as ub:
        y = \"2\"

holder t uses mid:
    slot:
        x = \"3\"
";
        let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        let body = resolved.unwrap().body;
        assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
        assert_eq!(
            nested_scalar(&body, "slot", "secret"),
            Some(&Value::String("locked".into()))
        );
        assert_eq!(
            nested_scalar(&body, "slot", "x"),
            Some(&Value::String("3".into())),
            "the third layer merges into the surviving variant"
        );
        assert_eq!(
            nested_scalar(&body, "slot", "y"),
            None,
            "the rejected layer's whole body is discarded"
        );
    }

    const NESTED_UNION_SCHEMA: &str = "\
model leafA:
    p string
    s string #sealed

model leafB:
    q string

model mid:
    inner (leafA | leafB)

model other:
    z string

model holder2:
    slot (mid | other)
";

    #[test]
    fn union_switch_is_backstopped_through_a_nested_union() {
        // RFC 0019: \"at any depth, recursively\" — a seal INSIDE a
        // union-typed field of the displaced variant must reject the
        // outer switch (the scan's ModelRef-only child vocabulary was a
        // laundering hole one union deep).
        let src = "\
holder2 base:
    slot as mid:
        inner as leafA:
            s = \"locked\"

holder2 t uses base:
    slot as other:
        z = \"1\"
";
        let (resolved, diags) = compose(NESTED_UNION_SCHEMA, src, "holder2", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("the outer switch is backstopped through the inner union");
        assert!(
            d.message.contains("slot.inner.s")
                && d.message.contains("unseal the field in the schema"),
            "full path + teaching tail: {}",
            d.message
        );
        let body = resolved.unwrap().body;
        assert_eq!(slot_annotation(&body).as_deref(), Some("mid"));
    }

    #[test]
    fn nested_union_inner_switch_is_backstopped_and_clean_when_unsealed() {
        // The inner union position runs the same authority: a sealed
        // inner switch rejects with the full nested path; an unsealed
        // one switches cleanly.
        let sealed = "\
holder2 base:
    slot as mid:
        inner as leafA:
            s = \"locked\"

holder2 t uses base:
    slot:
        inner as leafB:
            q = \"2\"
";
        let (resolved, diags) = compose(NESTED_UNION_SCHEMA, sealed, "holder2", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("inner switch is backstopped");
        assert!(d.message.contains("slot.inner.s"), "{}", d.message);
        let body = resolved.unwrap().body;
        let inner = sub_block(&body, "slot").and_then(|b| sub_block(b, "inner"));
        assert_eq!(
            inner
                .and_then(|b| b.type_annotation.as_ref())
                .map(|i| &i.name[..]),
            Some("leafA"),
            "the sealed inner variant survives"
        );

        let clean = "\
holder2 base:
    slot as mid:
        inner as leafA:
            p = \"1\"

holder2 t uses base:
    slot:
        inner as leafB:
            q = \"2\"
";
        let (resolved, diags) = compose(NESTED_UNION_SCHEMA, clean, "holder2", "t");
        assert!(
            diags.is_empty(),
            "unsealed inner switch is legal: {diags:?}"
        );
        let body = resolved.unwrap().body;
        let inner = sub_block(&body, "slot").and_then(|b| sub_block(b, "inner"));
        assert_eq!(
            inner
                .and_then(|b| b.type_annotation.as_ref())
                .map(|i| &i.name[..]),
            Some("leafB")
        );
    }

    const ONEOF_WITH_UNION_SCHEMA: &str = "\
model leafA:
    p string
    s string #sealed

model leafB:
    q string

model armX:
    kind string
    inner (leafA | leafB)

model armY:
    kind string
    w string

oneof pay by kind:
    \"x\" -> armX
    \"y\" -> armY
";

    #[test]
    fn oneof_switch_is_backstopped_through_a_union_field() {
        // The dual of the nested-union pin: a oneof ARM switch must see
        // seals hiding inside the displaced arm's union-typed field.
        let src = "\
pay base:
    kind = \"x\"
    inner as leafA:
        s = \"locked\"

pay top uses base:
    kind = \"y\"
    w = \"1\"
";
        let (resolved, diags) = compose(ONEOF_WITH_UNION_SCHEMA, src, "pay", "top");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("arm switch is backstopped through the union interior");
        assert!(
            d.message.contains("arm switch to `kind = \"y\"`") && d.message.contains("inner.s"),
            "names the switch and the buried seal: {}",
            d.message
        );
        let body = resolved.unwrap().body;
        assert_eq!(scalar(&body, "kind"), Some(&Value::String("x".into())));
    }

    const ONEOF_ARM_SET_SCHEMA: &str = "\
model card:
    kind string
    pan string #sealed

model cash:
    kind string
    amount string

oneof pay2 by kind:
    \"card\" -> card
    \"cash\" -> cash

model router2:
    route (string -> pay2)
";

    #[test]
    fn arm_set_replacement_with_a_oneof_target_is_backstopped() {
        // RFC 0019 binds the backstop to all three variant forms
        // \"equally\": a oneof-TARGETED arm set judges each displaced
        // inline body under the arm its own discriminator selects
        // (resolving only `index.model` here was a laundering hole —
        // and NML2076 explicitly promises this backstop).
        let src = "\
router2 base:
    route:
        \"a\" -> H:
            kind = \"card\"
            pan = \"4111\"

router2 t uses base:
    route:
        \"b\" -> H2:
            kind = \"cash\"
            amount = \"5\"
";
        let (resolved, diags) = compose(ONEOF_ARM_SET_SCHEMA, src, "router2", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("oneof-target arm-set replacement is backstopped");
        assert!(
            d.message.contains("arm-set replacement") && d.message.contains("route.pan"),
            "{}",
            d.message
        );
        let body = resolved.unwrap().body;
        let kept: Vec<String> = sub_block(&body, "route")
            .map(|b| {
                b.entries
                    .iter()
                    .filter_map(|e| match &e.kind {
                        BodyEntryKind::Arm(a) => match &a.target {
                            ArmTarget::Inline { name, .. } => Some(name.name.clone()),
                            _ => None,
                        },
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(kept, vec!["H"], "the sealed set survives");
    }

    const LIST_UNION_SCHEMA: &str = "\
model ua:
    x string
    secret string #sealed

model ub:
    y string

model holder3:
    xs [](ua | ub) #identity
";

    #[test]
    fn list_of_union_items_are_guarded_by_the_union_authority() {
        // A union list ELEMENT routes each identity-matched item group
        // through the union authority — merging model-less skipped seal
        // enforcement, establishment, and annotation synthesis entirely.
        // (`#identity` on a union list is itself NML2068 at schema load;
        // the engine still guards, defense in depth.)
        let src = "\
holder3 base:
    xs:
        - w as ua:
            x = \"1\"
            secret = \"locked\"

holder3 t uses base:
    xs:
        - w:
            secret = \"stomped\"
";
        let (resolved, diags) = compose(LIST_UNION_SCHEMA, src, "holder3", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("item seals are enforced through the union element");
        assert!(d.message.contains("xs[w].secret"), "{}", d.message);
        let body = resolved.unwrap().body;
        let item_annotation = sub_block(&body, "xs").and_then(|b| {
            b.entries.iter().find_map(|e| match &e.kind {
                BodyEntryKind::ListItem(ListItem {
                    kind: ListItemKind::Named { body, .. },
                    ..
                }) => body.type_annotation.as_ref().map(|i| i.name.clone()),
                _ => None,
            })
        });
        assert_eq!(
            item_annotation.as_deref(),
            Some("ua"),
            "the merged item body carries its variant explicitly"
        );
    }

    #[test]
    fn dependent_bogus_as_is_reported_not_swallowed() {
        // A bogus `as` on a dependent layer never switches (fail-safe) —
        // but the composed view replaces the annotation before the
        // validator sees it, so the MERGE must report NML2051 or the
        // typo composes silently into the wrong variant.
        let src = "\
holder base:
    slot as ua:
        x = \"1\"

holder t uses base:
    slot as zz:
        y = \"2\"
";
        let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::UNKNOWN_UNION_VARIANT))
            .expect("the swallowed annotation is reported");
        assert!(d.message.contains("`zz` is not a variant"), "{}", d.message);
        let body = resolved.unwrap().body;
        assert_eq!(
            slot_annotation(&body).as_deref(),
            Some("ua"),
            "the bogus name joined, never switched"
        );
    }

    const SCALAR_UNION_SCHEMA: &str = "\
model ua:
    x string

model holder4:
    slot (ua | string)
";

    #[test]
    fn structural_value_over_named_establishment_is_discarded_loudly() {
        // RFC 0015: scalar variants are structurally unambiguous and not
        // nameable — a whole-value spelling can neither merge into a
        // named variant nor switch it. Silence here was data loss; the
        // discard is NML2085 and the established value survives.
        let src = "\
holder4 base:
    slot as ua:
        x = \"1\"

holder4 t uses base:
    slot = \"replacement\"
";
        let (resolved, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::DISCARDED_UNION_CONTRIBUTION))
            .expect("the dropped scalar is loud");
        assert!(d.message.contains("established `as ua`"), "{}", d.message);
        let body = resolved.unwrap().body;
        assert_eq!(
            nested_scalar(&body, "slot", "x"),
            Some(&Value::String("1".into()))
        );
    }

    #[test]
    fn unannotated_body_over_structural_establishment_is_discarded_loudly() {
        // The reverse hijack: the lowest supplying layer established the
        // STRUCTURAL variant; an un-annotated upper body never switches
        // (its shape notwithstanding) — discarding it silently while its
        // shape \"won\" the position violated both halves of the rule.
        let src = "\
holder4 base:
    slot = \"the-base-value\"

holder4 t uses base:
    slot:
        x = \"1\"
";
        let (resolved, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::DISCARDED_UNION_CONTRIBUTION))
            .expect("the shape hijack is loud");
        assert!(
            d.message.contains("author `as ua` to switch"),
            "{}",
            d.message
        );
        let body = resolved.unwrap().body;
        assert_eq!(
            scalar(&body, "slot"),
            Some(&Value::String("the-base-value".into())),
            "the structural establishment survives"
        );
    }

    #[test]
    fn authored_as_switches_away_from_a_structural_value() {
        // An authored `as` IS the switch spelling — from a structural
        // establishment too (a displaced scalar carries no seals, so the
        // backstop always admits it).
        let src = "\
holder4 base:
    slot = \"money\"

holder4 t uses base:
    slot as ua:
        x = \"1\"
";
        let (resolved, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "t");
        assert!(diags.is_empty(), "authored switch is legal: {diags:?}");
        let body = resolved.unwrap().body;
        assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
        assert_eq!(
            nested_scalar(&body, "slot", "x"),
            Some(&Value::String("1".into()))
        );
    }

    #[test]
    fn optional_union_field_composes_with_the_backstop() {
        // Optionality is a FieldDef flag, not a type wrapper — `?` must
        // not cost a union position its compose authority.
        const OPT: &str = "\
model ua:
    secret string #sealed

model ub:
    y string

model holder5:
    slot (ua | ub)?
";
        let src = "\
holder5 base:
    slot as ua:
        secret = \"locked\"

holder5 t uses base:
    slot as ub:
        y = \"2\"
";
        let (resolved, diags) = compose(OPT, src, "holder5", "t");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        let body = resolved.unwrap().body;
        assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
    }

    // ── union compose: round-13 battery (list-variant establishments,
    //    per-shape structural buckets, ambiguity discipline, depth) ────

    const LIST_VARIANT_SCHEMA: &str = "\
model ua:
    x string

model ub:
    kind string
    secret string #sealed

model holder7:
    slot (ua | []ub)
";

    #[test]
    fn union_switch_off_a_list_variant_establishment_is_backstopped() {
        // \"A displaced structural group has no seals\" is true only for
        // scalars: block-form list items ARE bodies, and a switch away
        // from a list-variant establishment is judged over them under
        // the list variants' element models — assuming otherwise was a
        // laundering hole.
        let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"locked\"

holder7 t uses base:
    slot as ua:
        x = \"2\"
";
        let (resolved, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("the switch off the list establishment is backstopped");
        assert!(
            d.message.contains("slot[w].secret"),
            "item-prefixed seal path: {}",
            d.message
        );
        let body = resolved.unwrap().body;
        assert_eq!(
            slot_annotation(&body),
            None,
            "the structural list establishment survives, un-annotated"
        );
    }

    #[test]
    fn unsealed_list_variant_establishment_switches_cleanly() {
        // The same shape without an assigned seal admits the switch —
        // the backstop rejects laundering, not switching.
        let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"

holder7 t uses base:
    slot as ua:
        x = \"2\"
";
        let (resolved, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "t");
        assert!(diags.is_empty(), "unsealed switch is legal: {diags:?}");
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ua")
        );
    }

    const UNION_ELEMENT_SCHEMA: &str = "\
model leafA:
    p string
    s string #sealed

model leafB:
    q string

model bigv:
    ys [](leafA | leafB)

model other:
    z string

model holder8:
    slot (bigv | other)
";

    #[test]
    fn union_switch_is_backstopped_through_a_union_list_element() {
        // \"At any depth\" binds union-typed LIST ELEMENTS of a displaced
        // variant too — the scan's ModelRef-only element read was a
        // laundering hole through `[](a|b)` items.
        let src = "\
holder8 base:
    slot as bigv:
        ys:
            - w as leafA:
                s = \"locked\"

holder8 t uses base:
    slot as other:
        z = \"1\"
";
        let (resolved, diags) = compose(UNION_ELEMENT_SCHEMA, src, "holder8", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("the switch is backstopped through the union element");
        assert!(
            d.message.contains("slot.ys[w].s"),
            "full element path: {}",
            d.message
        );
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("bigv")
        );
    }

    #[test]
    fn structural_cross_shape_supplies_are_discarded_loudly() {
        // Scalar↔list inside the structural bucket is a variant change
        // with no `as` spelling to authorize it — one collapsed bucket
        // let the winner flip with the base's SPELLING and discarded a
        // later value silently.
        const S: &str = "\
model ua:
    x string

model holder9:
    slot (ua | string | []string)
";
        let scalar_over_list = "\
holder9 base:
    slot:
        - \"a\"

holder9 t uses base:
    slot = \"s\"
";
        let (resolved, diags) = compose(S, scalar_over_list, "holder9", "t");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        assert!(
            diags[0].message.contains("established as a list value"),
            "{}",
            diags[0].message
        );
        assert!(
            diags[0]
                .related
                .iter()
                .any(|r| r.message == "in force here"),
            "points at the list in force"
        );
        assert!(resolved.is_some(), "the established list survives");

        let list_over_scalar = "\
holder9 base:
    slot = \"s\"

holder9 t uses base:
    slot = [\"a\", \"b\"]
";
        let (resolved, diags) = compose(S, list_over_scalar, "holder9", "t");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        assert!(
            diags[0].message.contains("established as a scalar value"),
            "{}",
            diags[0].message
        );
        let body = resolved.unwrap().body;
        assert_eq!(
            scalar(&body, "slot"),
            Some(&Value::String("s".into())),
            "layer order wins, not spelling"
        );
    }

    #[test]
    fn structural_supply_after_a_rejected_switch_is_discarded_against_the_original() {
        // Two backstops in one fold: the rejected switch (NML2060) does
        // not disturb the establishment, and a later structural supply
        // is judged against the ORIGINAL variant (NML2085).
        const S: &str = "\
model ua:
    x string
    secret string #sealed

model ub:
    y string

model holder10:
    slot (ua | ub | string)
";
        let src = "\
holder10 base:
    slot as ua:
        secret = \"locked\"

holder10 mid uses base:
    slot as ub:
        y = \"2\"

holder10 t uses mid:
    slot = \"cash\"
";
        let (resolved, diags) = compose(S, src, "holder10", "t");
        assert_eq!(
            codes_of(&diags),
            [
                codes::SEALED_FIELD_VIOLATION,
                codes::DISCARDED_UNION_CONTRIBUTION
            ]
        );
        assert!(diags[1].message.contains("established `as ua`"));
        let body = resolved.unwrap().body;
        assert_eq!(
            nested_scalar(&body, "slot", "secret"),
            Some(&Value::String("locked".into()))
        );
    }

    #[test]
    fn structural_supply_after_an_authored_switch_from_structural_is_discarded() {
        // structural → authored switch → structural: the trailing value
        // is judged against the NEW named establishment, loudly.
        let src = "\
holder4 base:
    slot = \"money\"

holder4 mid uses base:
    slot as ua:
        x = \"2\"

holder4 t uses mid:
    slot = \"cash\"
";
        let (resolved, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "t");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        assert!(diags[0].message.contains("established `as ua`"));
        assert_eq!(
            nested_scalar(&resolved.unwrap().body, "slot", "x"),
            Some(&Value::String("2".into()))
        );
    }

    #[test]
    fn structural_union_restatement_is_a_dead_delta() {
        // The structural_overlay scalar route carries the same NML2084
        // dead-delta contract as the plain scalar overlay — and NML2085
        // and NML2084 coexist in one position.
        const S: &str = "\
model ua:
    x string

model holder11:
    slot (ua | string)
";
        let src = "\
holder11 base:
    slot = \"v\"

holder11 mid uses base:
    slot:
        x = \"1\"

holder11 t uses mid:
    slot = \"v\"
";
        let (resolved, diags) = compose(S, src, "holder11", "t");
        assert_eq!(
            codes_of(&diags),
            [codes::DISCARDED_UNION_CONTRIBUTION, codes::DEAD_DELTA]
        );
        assert_eq!(
            scalar(&resolved.unwrap().body, "slot"),
            Some(&Value::String("v".into()))
        );
    }

    #[test]
    fn item_scope_union_faces_fire_with_item_paths() {
        // The item-scope face: a rejected switch inside one identity
        // item names the ITEM path; a bogus `as` inside an item is
        // reported, never swallowed; a legal item switch after a merge
        // displaces the whole item group.
        let rejected = "\
holder3 base:
    xs:
        - w as ua:
            secret = \"locked\"

holder3 t uses base:
    xs:
        - w as ub:
            y = \"2\"
";
        let (_, diags) = compose(LIST_UNION_SCHEMA, rejected, "holder3", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("item switch is backstopped");
        assert!(
            d.message.contains("'xs[w]'") && d.message.contains("xs[w].secret"),
            "item path spelling: {}",
            d.message
        );

        let bogus = "\
holder3 base:
    xs:
        - w as ua:
            x = \"1\"

holder3 t uses base:
    xs:
        - w as zz:
            y = \"2\"
";
        let (_, diags) = compose(LIST_UNION_SCHEMA, bogus, "holder3", "t");
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::UNKNOWN_UNION_VARIANT)),
            "item bogus `as` is reported: {diags:?}"
        );

        let switch = "\
holder3 base:
    xs:
        - w as ua:
            x = \"1\"

holder3 mid uses base:
    xs:
        - w:
            x = \"2\"

holder3 t uses mid:
    xs:
        - w as ub:
            y = \"3\"
";
        let (resolved, diags) = compose(LIST_UNION_SCHEMA, switch, "holder3", "t");
        assert!(diags.is_empty(), "legal item switch: {diags:?}");
        let body = resolved.unwrap().body;
        let item_body = sub_block(&body, "xs").and_then(|b| {
            b.entries.iter().find_map(|e| match &e.kind {
                BodyEntryKind::ListItem(ListItem {
                    kind: ListItemKind::Named { body, .. },
                    ..
                }) => Some(body),
                _ => None,
            })
        });
        let item_body = item_body.expect("merged item survives");
        assert_eq!(
            item_body.type_annotation.as_ref().map(|i| &i.name[..]),
            Some("ub")
        );
        assert!(
            !item_body.entries.iter().any(|e| matches!(&e.kind,
                BodyEntryKind::Property(p) if p.name.name == "x")),
            "wholesale item switch"
        );
    }

    #[test]
    fn discarded_contributions_report_once_across_dependent_composes() {
        // The one-home dedup binds the new codes too: a transitive
        // dependent's re-compose must not re-report t's discard.
        let schema = "\
model ua:
    x string

model holder12:
    slot (ua | string)
";
        let src = "\
holder12 base:
    slot as ua:
        x = \"1\"

holder12 t uses base:
    slot = \"cash\"

holder12 t2 uses t:
    slot as ua:
        x = \"3\"
";
        let index = index_from(schema);
        let file = file_of(src);
        let out = compose_file(&index, "main.nml", &file, &OpenContext);
        let n = out
            .diagnostics
            .iter()
            .filter(|d| d.code == Some(codes::DISCARDED_UNION_CONTRIBUTION))
            .count();
        assert_eq!(n, 1, "one discard, one finding: {:?}", out.diagnostics);
    }

    #[test]
    fn bare_overlay_union_element_and_list_variant_lint_2076() {
        // The unreachable-seal lint sees union ELEMENTS (`[](a|b)`) and
        // a union's LIST VARIANT (`(a | []b)`) — with honest advice for
        // each (`#identity` is not grantable at either position).
        let diags = validate_merge_policies(&index_from(
            "model ua:\n    s string #sealed\n\nmodel ub:\n    q string\n\n\
             model m:\n    xs [](ua | ub)\n",
        ));
        assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL]);
        assert!(diags[0].message.contains("'ua'"), "{}", diags[0].message);

        let diags = validate_merge_policies(&index_from(
            "model ua:\n    x string\n\nmodel ub:\n    s string #sealed\n\n\
             model m:\n    slot (ua | []ub)\n",
        ));
        assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL]);
        assert!(
            diags[0]
                .message
                .contains("not grantable at a union list position")
                && diags[0].message.contains("list variant `[]ub`"),
            "honest advice with the list-variant lead, not a dead end: {}",
            diags[0].message
        );
        // A set variant AHEAD of the list variant is skipped: the lead
        // names the variant the backstop judges under.
        let diags = validate_merge_policies(&index_from(
            "model ua:\n    x string\n\nmodel ub:\n    s string #sealed\n\n\
             model m:\n    slot (ua | set<string> | []ub)\n",
        ));
        assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL]);
        assert!(
            diags[0].message.contains("list variant `[]ub`"),
            "{}",
            diags[0].message
        );
    }

    #[test]
    fn union_identity_element_gets_the_union_wording_of_2068() {
        let diags = validate_merge_policies(&index_from(
            "model ua:\n    x string\n\nmodel ub:\n    y string\n\n\
             model m:\n    xs [](ua | ub) #identity\n",
        ));
        assert_eq!(codes_of(&diags), [codes::INVALID_MERGE_POLICY]);
        assert!(
            diags[0].message.contains("identity across variants"),
            "union-specific wording: {}",
            diags[0].message
        );
    }

    #[test]
    fn seal_scan_reaches_arms_in_arms_and_root_positions() {
        // Depth: an arms-typed field INSIDE an arm's inline body still
        // scans (two levels of arm sets); and a rejection at an
        // instance ROOT renders without a position clause.
        const S: &str = "\
model deep:
    pan string #sealed

model hop:
    route2 (string -> deep)

model armx:
    kind string
    route (string -> hop)

model army:
    kind string
    w string

oneof pay3 by kind:
    \"x\" -> armx
    \"y\" -> army
";
        let src = "\
pay3 base:
    kind = \"x\"
    route:
        \"a\" -> H:
            route2:
                \"b\" -> D:
                    pan = \"locked\"

pay3 top uses base:
    kind = \"y\"
    w = \"1\"
";
        let (_, diags) = compose(S, src, "pay3", "top");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("two arm-set levels deep still scans");
        assert!(
            d.message.contains("route.route2.pan")
                && d.message.contains("arm switch to `kind = \"y\"` would"),
            "depth + root-elided position: {}",
            d.message
        );
    }

    // ── union compose: round-14 battery (list-variant judgment as a
    //    LIST, oracle candidates everywhere, zero-item non-supplies,
    //    every spelling through the authority, recorded discard faces) ──

    #[test]
    fn ambiguous_group_is_pinned_by_an_authored_as() {
        // An `as` above an ambiguous group RESOLVES it — nothing was
        // chosen to switch from, so nothing is discarded: the group
        // deep-merges under the named variant and the output carries it.
        let src = "\
holder base:
    slot:
        x = \"1\"

holder t uses base:
    slot as ua:
        x = \"2\"
";
        let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
        assert!(diags.is_empty(), "a pin is not a switch: {diags:?}");
        let body = resolved.unwrap().body;
        assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
        assert_eq!(
            nested_scalar(&body, "slot", "x"),
            Some(&Value::String("2".into())),
            "the ambiguous base joined the pinned group"
        );
    }

    #[test]
    fn ambiguous_interiors_are_scanned_under_every_oracle_candidate() {
        // A oneof arm switch displacing an arm whose union field holds an
        // AMBIGUOUS body: the seal lives in the SECOND candidate. The scan
        // must judge under every oracle candidate — the resolver's
        // first-wins pick made the verdict depend on variant source
        // order (both orders pinned).
        for (order, schema_variants) in [
            ("a-first", "(leafA | leafB)"),
            ("b-first", "(leafB | leafA)"),
        ] {
            let schema = format!(
                "\
model leafA:
    p string
    q string

model leafB:
    p string
    s string #sealed

model armX:
    kind string
    inner {schema_variants}

model armY:
    kind string
    w string

oneof pay4 by kind:
    \"x\" -> armX
    \"y\" -> armY
"
            );
            let src = "\
pay4 base:
    kind = \"x\"
    inner:
        s = \"locked\"

pay4 top uses base:
    kind = \"y\"
    w = \"1\"
";
            let (_, diags) = compose(&schema, src, "pay4", "top");
            let d = diags
                .iter()
                .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
                .unwrap_or_else(|| {
                    panic!("{order}: ambiguous interior seal must reject: {diags:?}")
                });
            assert!(d.message.contains("inner.s"), "{order}: {}", d.message);
        }
    }

    const LIST_VARIANT_SHARED_SCHEMA: &str = "\
model ua:
    x string

model ub:
    name string+ #sealed
    secret string? #sealed
    note string?

model holder13:
    slot (ua | []ub)
";

    #[test]
    fn union_switch_off_a_list_variant_judges_shared_and_positional_writes() {
        // The displaced LIST is judged as a list: a list-level `.shared`
        // sealed write and a bodiless item's positional `+` token are
        // both assigned seals — scanning item bodies in isolation saw
        // neither.
        let shared = "\
holder13 base:
    slot:
        .secret = \"locked\"
        - w:
            note = \"n\"

holder13 t uses base:
    slot as ua:
        x = \"2\"
";
        let (_, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, shared, "holder13", "t");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        assert!(
            diags[0].message.contains("slot[w].secret"),
            "{}",
            diags[0].message
        );

        let positional = "\
holder13 base:
    slot:
        - \"w\"

holder13 t uses base:
    slot as ua:
        x = \"2\"
";
        let (_, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, positional, "holder13", "t");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        assert!(
            diags[0].message.contains("slot[string].name"),
            "{}",
            diags[0].message
        );
    }

    #[test]
    fn list_variant_with_a_oneof_element_is_backstopped() {
        // `(ua | []uo)` — a oneof ELEMENT: each displaced item is judged
        // under the arm its own discriminator selects (NML2076 promises
        // exactly this backstop for the shape).
        const S: &str = "\
model ua:
    x string

model arma:
    kind string
    secret string #sealed

model armb:
    kind string
    q string

oneof uo by kind:
    \"a\" -> arma
    \"b\" -> armb

model holder14:
    slot (ua | []uo)
";
        let src = "\
holder14 base:
    slot:
        - w:
            kind = \"a\"
            secret = \"s\"

holder14 t uses base:
    slot as ua:
        x = \"1\"
";
        let (_, diags) = compose(S, src, "holder14", "t");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        assert!(
            diags[0].message.contains("slot[w].secret"),
            "{}",
            diags[0].message
        );
    }

    #[test]
    fn modifier_spelled_union_positions_take_the_union_route() {
        // Every spelling reaches the union authority: a modifier-spelled
        // item block is a scannable list body, and an all-modifier
        // scalar↔list cross is as loud as the property spelling.
        let launder = "\
holder13 base:
    |slot:
        - w:
            secret = \"s\"

holder13 t uses base:
    slot as ua:
        x = \"1\"
";
        let (_, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, launder, "holder13", "t");
        assert_eq!(
            codes_of(&diags),
            [codes::SEALED_FIELD_VIOLATION],
            "modifier-spelled items are judged, and nothing else fires"
        );

        const S: &str = "\
model holder15:
    slot ([]string | string)
";
        let cross = "\
holder15 base:
    |slot = [\"a\"]

holder15 t uses base:
    |slot = \"v\"
";
        let (_, diags) = compose(S, cross, "holder15", "t");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    }

    #[test]
    fn zero_item_entries_at_union_positions_are_warned_and_never_establish() {
        // NML2079's contract holds at union positions: `= []` and an
        // empty block warn and are no-ops — as the lowest supply they
        // establish nothing (a valid upper is not a false NML2085), over
        // a list they are inert.
        let lowest = "\
holder13 base:
    slot = []

holder13 t uses base:
    slot:
        x = \"1\"
";
        let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, lowest, "holder13", "t");
        assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ua"),
            "the first REAL supply establishes"
        );

        let over_items = "\
holder13 base:
    slot:
        - w:
            note = \"n\"

holder13 t uses base:
    slot:
";
        let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, over_items, "holder13", "t");
        assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
        let body = resolved.unwrap().body;
        let items = sub_block(&body, "slot")
            .map(|b| {
                b.entries
                    .iter()
                    .filter(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(items, 1, "the base list survives the zero-item no-op");
    }

    #[test]
    fn discard_faces_follow_the_recorded_context() {
        // The face is keyed on the (establishment, supply) pair the FOLD
        // recorded: a list over a scalar is the cross-shape face; a
        // scalar over an ambiguous body names the candidates; a discard
        // followed by a switch is reported once, against the
        // establishment in force at the time.
        const S: &str = "\
model card:
    last4 string

model debit:
    last4 string
    note string

model wallet2:
    payment (card | debit | string | []string)
";
        let cross = "\
wallet2 base:
    payment = \"cash\"

wallet2 t uses base:
    payment:
        - \"a\"
";
        let (_, diags) = compose(S, cross, "wallet2", "t");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        assert!(
            diags[0]
                .message
                .contains("a list value cannot merge into it"),
            "cross-shape face: {}",
            diags[0].message
        );

        let over_ambiguous = "\
wallet2 base:
    payment:
        last4 = \"1\"

wallet2 t uses base:
    payment = \"cash\"
";
        let (_, diags) = compose(S, over_ambiguous, "wallet2", "t");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        assert!(
            diags[0]
                .message
                .contains("un-annotated body (ambiguous between card | debit)"),
            "ambiguous establishment names its candidates: {}",
            diags[0].message
        );

        let discard_then_switch = "\
wallet2 base:
    payment = \"cash\"

wallet2 mid uses base:
    payment:
        last4 = \"1\"

wallet2 t uses mid:
    payment as card:
        last4 = \"2\"
";
        let (resolved, diags) = compose(S, discard_then_switch, "wallet2", "t");
        assert_eq!(
            codes_of(&diags),
            [codes::DISCARDED_UNION_CONTRIBUTION],
            "one discard, one finding, judged against the scalar establishment"
        );
        assert!(
            diags[0].message.contains("established as a scalar value"),
            "{}",
            diags[0].message
        );
        assert_eq!(
            slot_annotation_named(&resolved.unwrap().body, "payment").as_deref(),
            Some("card")
        );
    }

    fn slot_annotation_named(body: &Body, name: &str) -> Option<String> {
        sub_block(body, name).and_then(|b| b.type_annotation.as_ref().map(|i| i.name.clone()))
    }

    #[test]
    fn dependent_as_naming_a_list_element_gets_the_honest_form() {
        // `as ub` where `ub` is only a list variant's ELEMENT: not a
        // nameable variant, and "did you mean ua" would mislead.
        let src = "\
holder13 base:
    slot as ua:
        x = \"1\"

holder13 t uses base:
    slot as ub:
        x = \"2\"
";
        let (_, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, src, "holder13", "t");
        assert_eq!(
            codes_of(&diags),
            [codes::UNKNOWN_UNION_VARIANT],
            "one defect, one finding (no NML2085 riding along): {diags:?}"
        );
        assert!(
            diags[0].message.contains("names a list variant's element")
                && diags[0].suggestions.is_empty(),
            "honest form, no did-you-mean: {}",
            diags[0].message
        );
    }

    #[test]
    fn items_after_a_rejected_switch_join_the_original_list_establishment() {
        let src = "\
holder13 base:
    slot:
        - w:
            secret = \"locked\"

holder13 mid uses base:
    slot as ua:
        x = \"1\"

holder13 t uses mid:
    slot:
        - v:
            note = \"n\"
";
        let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, src, "holder13", "t");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        let body = resolved.unwrap().body;
        assert_eq!(slot_annotation(&body), None, "still the list establishment");
        let names: Vec<String> = sub_block(&body, "slot")
            .map(|b| {
                b.entries
                    .iter()
                    .filter_map(|e| match &e.kind {
                        BodyEntryKind::ListItem(ListItem {
                            kind: ListItemKind::Named { name, .. },
                            ..
                        }) => Some(name.name.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(names, vec!["v"], "the top list wins by the bare-list rule");
    }

    #[test]
    fn all_structural_union_groups_take_the_union_route() {
        // All-scalar and all-array groups compose by the same rules as
        // before the routing widen — scalar overlay with its dead delta,
        // the bare-list winner — now through one owner each.
        const S: &str = "\
model holder16:
    a (string | number)
    xs ([]string | string)
";
        let src = "\
holder16 base:
    a = \"v\"
    xs = [\"a\"]

holder16 t uses base:
    a = \"v\"
    xs = [\"b\", \"c\"]
";
        let (resolved, diags) = compose(S, src, "holder16", "t");
        assert_eq!(codes_of(&diags), [codes::DEAD_DELTA]);
        let body = resolved.unwrap().body;
        assert_eq!(scalar(&body, "a"), Some(&Value::String("v".into())));
        let xs_items = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::Property(p) if p.name.name == "xs" => match &p.value.value {
                    Value::Array(v) => Some(v.len()),
                    _ => None,
                },
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "xs" => Some(
                    nb.body
                        .entries
                        .iter()
                        .filter(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
                        .count(),
                ),
                _ => None,
            })
            .unwrap_or(0);
        assert_eq!(xs_items, 2, "the higher item supplier wins");
    }

    #[test]
    fn discard_notes_point_at_the_establishing_entry_for_every_face() {
        let src = "\
holder4 base:
    slot as ua:
        x = \"1\"

holder4 t uses base:
    slot = \"cash\"
";
        let (_, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "t");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        assert!(
            diags[0]
                .related
                .iter()
                .any(|r| r.message == "established here"),
            "the named face carries the note too"
        );
        for src in [
            // ambiguous establishment, scalar supply
            "holder base:\n    slot:\n        x = \"1\"\n\nholder t uses base:\n    slot = \"v\"\n",
            // list establishment, scalar supply
            "holder13 base:\n    slot:\n        - w:\n            note = \"n\"\n\nholder13 t uses base:\n    slot = \"v\"\n",
        ] {
            let (schema, kw, name) = if src.starts_with("holder13") {
                (LIST_VARIANT_SHARED_SCHEMA, "holder13", "t")
            } else {
                (UNION_SCHEMA, "holder", "t")
            };
            let (_, diags) = compose(schema, src, kw, name);
            assert_eq!(
                codes_of(&diags),
                [codes::DISCARDED_UNION_CONTRIBUTION],
                "{src}"
            );
            let expected = if src.starts_with("holder13") {
                "in force here"
            } else {
                "established here"
            };
            assert!(
                diags[0].related.iter().any(|r| r.message == expected),
                "every face carries its note: {src}"
            );
        }
    }

    #[test]
    fn nested_union_discard_agrees_between_planned_and_refolded_routes() {
        // Discarded rides the PLAN at an all-nested inner position; a
        // whole-value sibling breaks alignment and forces the local
        // refold — both routes must say the same thing.
        const S: &str = "\
model leafA:
    s string #sealed

model outer:
    inner (leafA | []string)

model holder17:
    slot (outer | string)
";
        let planned = "\
holder17 base:
    slot as outer:
        inner as leafA:
            s = \"locked\"

holder17 t uses base:
    slot:
        inner:
            - \"v\"
";
        let (resolved, planned_diags) = compose(S, planned, "holder17", "t");
        assert_eq!(
            codes_of(&planned_diags),
            [codes::DISCARDED_UNION_CONTRIBUTION]
        );
        let inner_s = sub_block(&resolved.unwrap().body, "slot")
            .and_then(|b| sub_block(b, "inner"))
            .and_then(|b| scalar(b, "s").cloned());
        assert_eq!(inner_s, Some(Value::String("locked".into())));

        let refolded = "\
holder17 base:
    slot as outer:
        inner as leafA:
            s = \"locked\"

holder17 mid uses base:
    slot:
        inner:
            - \"v\"

holder17 t uses mid:
    slot:
        inner = \"w\"
";
        let (_, refold_diags) = compose(S, refolded, "holder17", "t");
        assert_eq!(
            codes_of(&refold_diags),
            [
                codes::DISCARDED_UNION_CONTRIBUTION,
                codes::DISCARDED_UNION_CONTRIBUTION
            ]
        );
        assert_eq!(
            planned_diags[0].message, refold_diags[0].message,
            "planned and refolded routes agree"
        );
    }

    // ── union compose: round-15 battery (the rule table itself, every
    //    spelling through the authority, list judgment as the bare-list
    //    winner, plan/merge supply parity, loud fail-safes) ──────────────

    #[test]
    fn union_verdict_table_enumerates_every_cell() {
        // The rule table, cell by cell — RFC 0019's rules (and the
        // documented errata) as one readable matrix. A regression in
        // any cell shows up as a named (row, column).
        fn body() -> Cow<'static, Body> {
            Cow::Owned(Body::fresh(Vec::new()))
        }
        let named = || Establishment::Named {
            variant: "ua".into(),
            synthesized: false,
        };
        let ambiguous = || Establishment::Ambiguous {
            candidates: vec!["ua".into(), "ub".into()],
        };
        let rows: [(&str, Option<Establishment>); 5] = [
            ("none", None),
            ("named ua", Some(named())),
            ("ambiguous", Some(ambiguous())),
            ("value", Some(Establishment::Value)),
            ("items", Some(Establishment::Items)),
        ];
        type Make = fn() -> UnionSupply<'static>;
        let supplies: [(&str, Make); 7] = [
            ("authored ua", || UnionSupply::Authored {
                variant: "ua".into(),
                body: body(),
            }),
            ("authored ub", || UnionSupply::Authored {
                variant: "ub".into(),
                body: body(),
            }),
            ("inferred", || UnionSupply::Inferred {
                variant: "ua".into(),
                body: body(),
            }),
            ("ambiguous", || UnionSupply::Ambiguous {
                candidates: vec!["ua".into(), "ub".into()],
                body: body(),
            }),
            ("items", || UnionSupply::Items { body: body() }),
            ("empty", || UnionSupply::Empty),
            ("value", || UnionSupply::Value),
        ];
        // Columns: authored-same, authored-other, inferred, ambiguous,
        // items, empty, value.
        use Verdict as V;
        let expected: [(&str, [V; 7]); 5] = [
            // RFC 0019 "the lowest supplying layer establishes"; a
            // zero-item entry never supplies (NML2079's contract, E7).
            (
                "none",
                [
                    V::Establish,
                    V::Establish,
                    V::Establish,
                    V::Establish,
                    V::Establish,
                    V::Join,
                    V::Establish,
                ],
            ),
            // RFC 0019: restatement joins, a different `as` switches
            // (judged), an un-annotated body never switches; a whole
            // value cannot merge into a named variant (E2).
            (
                "named ua",
                [
                    V::Join,
                    V::JudgeSwitch("ub".into()),
                    V::Join,
                    V::Join,
                    V::Discard,
                    V::Join,
                    V::Discard,
                ],
            ),
            // E5/E6: an `as` above an ambiguous group pins it (nothing
            // was chosen to switch from); bodies join; whole values
            // cannot merge into a body (E2).
            (
                "ambiguous",
                [
                    V::Pin("ua".into()),
                    V::Pin("ub".into()),
                    V::Join,
                    V::Join,
                    V::Discard,
                    V::Join,
                    V::Discard,
                ],
            ),
            // E4: per-shape structural establishments — an authored `as`
            // switches (a displaced scalar has no seals; a list is
            // judged, E9); bodies cannot merge (E3); the same shape joins
            // its overlay; a cross shape is discarded.
            (
                "value",
                [
                    V::JudgeSwitch("ua".into()),
                    V::JudgeSwitch("ub".into()),
                    V::Discard,
                    V::Discard,
                    V::Discard,
                    V::Join,
                    V::Join,
                ],
            ),
            (
                "items",
                [
                    V::JudgeSwitch("ua".into()),
                    V::JudgeSwitch("ub".into()),
                    V::Discard,
                    V::Discard,
                    V::Join,
                    V::Join,
                    V::Discard,
                ],
            ),
        ];
        for ((row, est), (_, want)) in rows.iter().zip(expected.iter()) {
            for ((col, make), w) in supplies.iter().zip(want.iter()) {
                assert_eq!(
                    &union_verdict(est.as_ref(), &make()),
                    w,
                    "cell ({row}, {col})"
                );
            }
        }
    }

    #[test]
    fn type_annotation_modifiers_never_bypass_the_union_authority() {
        // A `|slot (ua | ub)` declaration inside an instance body is a
        // declaration, not a value: it must neither route the group
        // around the authority (a debug panic; in release a last-wins
        // that laundered seals or deleted the value) nor count as a
        // sealing write.
        const S: &str = "\
model ua:
    x string #sealed

model ub:
    y string

model holder18:
    slot (ua | ub)
";
        let launder = "\
holder18 base:
    slot as ua:
        x = \"1\"

holder18 top uses base:
    |slot (ua | ub)
    slot as ub:
        y = \"2\"
";
        let (resolved, diags) = compose(S, launder, "holder18", "top");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ua"),
            "the sealed base survives the annotated switch"
        );

        let alone = "\
holder18 base:
    slot as ua:
        x = \"1\"

holder18 top uses base:
    |slot (ua | ub)
";
        let (resolved, diags) = compose(S, alone, "holder18", "top");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        assert_eq!(
            nested_scalar(&body, "slot", "x"),
            Some(&Value::String("1".into())),
            "an annotation-only upper never deletes the established value"
        );

        const SEALED: &str = "\
model ua:
    x string

model ub:
    y string

model holder19:
    slot (ua | ub) #sealed
";
        let sealed = "\
holder19 base:
    |slot (ua | ub)
    slot as ua:
        x = \"1\"

holder19 top uses base:
    slot as ub:
        y = \"2\"
";
        let (resolved, diags) = compose(SEALED, sealed, "holder19", "top");
        assert_eq!(
            codes_of(&diags),
            [codes::SEALED_FIELD_VIOLATION],
            "the real assignment seals; the declaration never does"
        );
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ua")
        );
    }

    #[test]
    fn plan_and_merge_fold_the_same_supply_set() {
        // A whole-value sibling used to break plan alignment, and the
        // local refold then judged bodies already normalized under the
        // FINAL planned variant — a fabricated refusal (itb's `key`
        // token injected into the base, then scanned as ua's sealed
        // `key`). Same supply set on both sides: the trace aligns.
        const S: &str = "\
model ita:
    name string+
    key string? #sealed

model itb:
    key string+ #sealed

model ua:
    items []ita

model ub:
    items []itb

model holder20:
    slot (ua | ub | string)
";
        let src = "\
holder20 base:
    slot as ua:
        items:
            - \"w\"

holder20 mid uses base:
    slot = \"x\"

holder20 top uses mid:
    slot as ub:
        items:
            - \"k\"
";
        let (resolved, diags) = compose(S, src, "holder20", "top");
        assert_eq!(
            codes_of(&diags),
            [codes::DISCARDED_UNION_CONTRIBUTION],
            "mid's scalar is discarded; the switch is clean (no seal was assigned): {diags:?}"
        );
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ub")
        );
    }

    #[test]
    fn shared_only_union_blocks_survive_authored_empty() {
        // A `.shared`-only block owns no entries: a zero-item entry raw
        // AND normalized (the plan and the merge agree), and an
        // all-zero-item position survives in the `= []` spelling rather
        // than dropping (a phantom "missing required field 'slot'").
        let src = "\
holder13 base:
    slot:
        .note = \"n\"

holder13 t uses base:
    slot:
        .note = \"m\"
";
        let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, src, "holder13", "t");
        assert_eq!(
            codes_of(&diags),
            [codes::ZERO_ITEM_LAYER_ENTRY, codes::ZERO_ITEM_LAYER_ENTRY]
        );
        let body = resolved.unwrap().body;
        let respelled = body.entries.iter().any(|e| {
            matches!(&e.kind, BodyEntryKind::Property(p)
                if p.name.name == "slot" && matches!(&p.value.value, Value::Array(v) if v.is_empty()))
        });
        assert!(respelled, "survives as `slot = []`: {body:?}");
    }

    #[test]
    fn replaced_lists_are_not_judged_on_a_later_switch() {
        // The displaced compose of a list establishment is the bare-list
        // WINNER; a lower list the engine itself replaced wholesale
        // (its seals never engaged, as NML2076 warns) must not refuse a
        // later switch.
        let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"locked\"

holder7 mid uses base:
    slot:
        - v:
            kind = \"k\"

holder7 top uses mid:
    slot as ua:
        x = \"1\"
";
        let (resolved, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "top");
        assert!(diags.is_empty(), "no phantom refusal: {diags:?}");
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ua")
        );
    }

    #[test]
    fn zero_item_entry_never_seals_a_sealed_union_position() {
        const S: &str = "\
model ua:
    x string

model ub:
    kind string

model holder21:
    slot (ua | []ub) #sealed
";
        let src = "\
holder21 base:
    slot = []

holder21 t uses base:
    slot as ua:
        x = \"1\"
";
        let (resolved, diags) = compose(S, src, "holder21", "t");
        assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ua"),
            "the first REAL assignment seals"
        );
    }

    #[test]
    fn empty_array_under_a_listless_union_is_a_loud_whole_value() {
        // `= []` under a union with no list variant is an (invalid)
        // whole value — never a phantom empty object that classifies as
        // a body and swallows silently.
        const S: &str = "\
model ua:
    x string #sealed

model uc:
    z string

model holder22:
    slot (ua | uc)
";
        let src = "\
holder22 base:
    slot as ua:
        x = \"s\"

holder22 t uses base:
    slot = []
";
        let (resolved, diags) = compose(S, src, "holder22", "t");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        assert_eq!(
            nested_scalar(&resolved.unwrap().body, "slot", "x"),
            Some(&Value::String("s".into()))
        );
    }

    #[test]
    fn pin_carries_the_authored_identifier() {
        // The pinning layer's `as` is authored: the composed annotation
        // is that identifier (span inside the pinning layer), not one
        // synthesized at the ambiguous base.
        let src = "\
holder base:
    slot:
        x = \"1\"

holder t uses base:
    slot as ua:
        x = \"2\"
";
        let (resolved, diags) = compose(UNION_SCHEMA, src, "holder", "t");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        let ann = sub_block(&body, "slot")
            .and_then(|b| b.type_annotation.clone())
            .expect("annotated");
        assert_eq!(ann.name, "ua");
        let tok = src.rfind("as ua").unwrap() + 3;
        assert_eq!(
            (ann.span.start, ann.span.end),
            (tok, tok + 2),
            "the annotation is the pinning layer's own `ua` token"
        );
    }

    #[test]
    fn pin_then_switch_is_judged_under_the_pinned_vocabulary() {
        // Once pinned, the group IS the pinned variant: a later
        // different `as` is judged over it under that vocabulary only
        // (a write meaningful in an un-pinned candidate is not a seal
        // there). Both orders pinned.
        const S: &str = "\
model ua:
    x string
    s string

model uc:
    x string
    s string #sealed

model holder23:
    slot (ua | uc)
";
        let pin_ua = "\
holder23 base:
    slot:
        s = \"locked\"

holder23 mid uses base:
    slot as ua:
        x = \"1\"

holder23 top uses mid:
    slot as uc:
        x = \"2\"
";
        let (resolved, diags) = compose(S, pin_ua, "holder23", "top");
        assert!(diags.is_empty(), "under ua, `s` is unsealed: {diags:?}");
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("uc")
        );

        let pin_uc = "\
holder23 base:
    slot:
        s = \"locked\"

holder23 mid uses base:
    slot as uc:
        x = \"1\"

holder23 top uses mid:
    slot as ua:
        x = \"2\"
";
        let (resolved, diags) = compose(S, pin_uc, "holder23", "top");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("uc")
        );
    }

    #[test]
    fn memo_is_invalidated_when_a_join_changes_the_list() {
        // Two rejected switches reuse one judgment; a layer that
        // supplies a NEW list (the bare-list winner changes) is judged
        // fresh — the count suffix follows the new list.
        let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"a\"

holder7 l1 uses base:
    slot as ua:
        x = \"1\"

holder7 l2 uses l1:
    slot as ua:
        x = \"2\"

holder7 l3 uses l2:
    slot:
        - v:
            kind = \"k\"
            secret = \"b\"
        - u:
            kind = \"k\"
            secret = \"c\"

holder7 l4 uses l3:
    slot as ua:
        x = \"4\"
";
        let (_, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "l4");
        let seals: Vec<&Diagnostic> = diags
            .iter()
            .filter(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .collect();
        assert_eq!(seals.len(), 3, "{diags:?}");
        assert!(!seals[0].message.contains("(and "), "{}", seals[0].message);
        assert!(!seals[1].message.contains("(and "), "{}", seals[1].message);
        assert!(
            seals[2].message.contains("(and 1 more field)"),
            "judged fresh over l3's list: {}",
            seals[2].message
        );
    }

    #[test]
    fn same_layer_discard_names_the_earlier_entry() {
        let src = "\
holder4 base:
    slot as ua:
        x = \"1\"
    slot = \"cash\"
";
        let (_, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "base");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        assert!(
            diags[0]
                .message
                .contains("by an earlier entry in this same layer"),
            "{}",
            diags[0].message
        );
    }

    #[test]
    fn ambiguous_establishment_discard_advises_resolving_the_lower_body() {
        let src = "\
holder base:
    slot:
        x = \"1\"

holder t uses base:
    slot = \"cash\"
";
        let (_, diags) = compose(UNION_SCHEMA, src, "holder", "t");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        assert!(
            diags[0]
                .message
                .contains("resolve the establishing body with `as <ua | ub>`"),
            "{}",
            diags[0].message
        );
    }

    // ── union compose: round-16 battery (sealed union bodies are writes,
    //    declarations pass through everywhere, plan authority, the sink) ──

    #[test]
    fn keyed_bodies_at_a_sealed_item_admitting_union_are_writes() {
        // Regression: `admits_items` made every keyed body at
        // `(ua | []ub) #sealed` a "zero-item" non-write — an upper layer
        // replaced the sealed value silently, and both backstops missed
        // it. Field face (annotated and inferred) and backstop face.
        const S: &str = "\
model ua:
    x string

model ub:
    kind string

model holder24:
    slot (ua | []ub) #sealed
";
        for src in [
            "holder24 base:\n    slot as ua:\n        x = \"1\"\n\nholder24 top uses base:\n    slot as ua:\n        x = \"2\"\n",
            "holder24 base:\n    slot:\n        x = \"1\"\n\nholder24 top uses base:\n    slot:\n        x = \"2\"\n",
            "holder24 base:\n    slot as ua:\n        x = \"1\"\n\nholder24 top uses base:\n    slot = \"gone\"\n",
        ] {
            let (resolved, diags) = compose(S, src, "holder24", "top");
            assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION], "{src}");
            assert_eq!(
                nested_scalar(&resolved.unwrap().body, "slot", "x"),
                Some(&Value::String("1".into())),
                "the sealed body survives: {src}"
            );
        }

        const ONEOF: &str = "\
model ua:
    x string

model ub:
    kind string

model armA:
    kind string
    inner (ua | []ub) #sealed

model armB:
    kind string
    z string

oneof cfg by kind:
    \"a\" -> armA
    \"b\" -> armB
";
        let src = "\
cfg base:
    kind = \"a\"
    inner as ua:
        x = \"1\"

cfg top uses base:
    kind = \"b\"
    z = \"2\"
";
        let (_, diags) = compose(ONEOF, src, "cfg", "top");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        assert!(diags[0].message.contains("'inner'"), "{}", diags[0].message);
    }

    #[test]
    fn declarations_pass_through_beside_the_value_everywhere() {
        // A type-annotation modifier is a declaration on EVERY route:
        // it never deletes a scalar value (the old last-wins), never
        // desynchronizes the plan (a fabricated refusal), never writes a
        // seal — and it survives into the composed view so the validator
        // still checks it.
        const BOX: &str = "\
model box:
    label string
";
        for src in [
            "box base:\n    label = \"a\"\n\nbox t uses base:\n    |label string\n",
            "box base:\n    label = \"a\"\n\nbox t uses base:\n    |label string\n    label = \"b\"\n",
        ] {
            let (resolved, diags) = compose(BOX, src, "box", "t");
            assert!(diags.is_empty(), "{src}: {diags:?}");
            let body = resolved.unwrap().body;
            assert!(
                scalar(&body, "label").is_some(),
                "the value survives: {src}"
            );
            assert!(
                body.entries.iter().any(|e| matches!(&e.kind,
                    BodyEntryKind::Modifier(m) if matches!(m.value, ModifierValue::TypeAnnotation { .. }))),
                "the declaration passes through: {src}"
            );
        }

        // Plan parity: one annotation line above a positional-token stack
        // used to misalign the plan and refold over final-variant
        // bodies (the r15 fabricated-refusal shape).
        const S: &str = "\
model ita:
    name string+
    key string? #sealed

model itb:
    key string+ #sealed

model ua:
    items []ita

model ub:
    items []itb

model holder25:
    slot (ua | ub)
";
        let src = "\
holder25 base:
    slot as ua:
        items:
            - \"w\"

holder25 top uses base:
    |slot (ua | ub)
    slot as ub:
        items:
            - \"k\"
";
        let (resolved, diags) = compose(S, src, "holder25", "top");
        assert!(diags.is_empty(), "no fabricated refusal: {diags:?}");
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ub")
        );

        // Sealed union: the annotation above the establishment is not a
        // write.
        const SEALED: &str = "\
model ua:
    x string

model ub:
    y string

model holder26:
    slot (ua | ub) #sealed
";
        let src = "\
holder26 base:
    slot as ua:
        x = \"1\"

holder26 top uses base:
    |slot (ua | ub)
";
        let (resolved, diags) = compose(SEALED, src, "holder26", "top");
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ua")
        );
    }

    #[test]
    fn sealed_union_positions_still_report_a_bogus_as() {
        const SEALED: &str = "\
model ua:
    x string

model ub:
    y string

model holder27:
    slot (ua | ub) #sealed
";
        let src = "\
holder27 base:
    slot as ua:
        x = \"1\"

holder27 top uses base:
    slot as zz:
        y = \"2\"
";
        let (_, diags) = compose(SEALED, src, "holder27", "top");
        assert_eq!(
            codes_of(&diags),
            [codes::UNKNOWN_UNION_VARIANT, codes::SEALED_FIELD_VIOLATION]
        );
    }

    #[test]
    fn later_list_variants_and_set_variants_are_unreachable_by_shape() {
        // The resolver selects the FIRST `List` variant for a list body
        // and never a set variant; judgment, lint and validator agree.
        const S: &str = "\
model ua:
    x string

model ub:
    kind string

model uc:
    kind string
    secret string #sealed

model holder28:
    slot (ua | []ub | []uc)
";
        let src = "\
holder28 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"s\"

holder28 top uses base:
    slot as ua:
        x = \"1\"
";
        let (resolved, diags) = compose(S, src, "holder28", "top");
        assert!(
            diags.is_empty(),
            "uc is unreachable, its seal is not judged: {diags:?}"
        );
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ua")
        );
        let lint = validate_merge_policies(&index_from(S));
        assert!(
            lint.is_empty(),
            "no promise for an unreachable variant: {lint:?}"
        );

        const SET: &str = "\
model ua:
    x string

model ub:
    secret string #sealed

model holder29:
    slot (ua | set<ub>)
";
        let lint = validate_merge_policies(&index_from(SET));
        assert!(
            lint.is_empty(),
            "a set variant is unreachable by shape: {lint:?}"
        );
    }

    #[test]
    fn zero_item_entry_between_list_layers_keeps_the_judged_group() {
        let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"locked\"

holder7 mid uses base:
    slot = []

holder7 top uses mid:
    slot as ua:
        x = \"1\"
";
        let (resolved, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "top");
        assert_eq!(
            codes_of(&diags),
            [codes::ZERO_ITEM_LAYER_ENTRY, codes::SEALED_FIELD_VIOLATION]
        );
        assert_eq!(slot_annotation(&resolved.unwrap().body), None);
    }

    #[test]
    fn pin_bookkeeping_survives_empty_discard_and_restatement() {
        const S: &str = "\
model ua:
    x string

model uc:
    x string

model holder30:
    slot (ua | uc | []ub | string)

model ub:
    kind string
";
        // pin, then a zero-item entry: the pin and both values survive
        let src = "\
holder30 base:
    slot:
        x = \"1\"

holder30 mid uses base:
    slot as ua:
        x = \"2\"

holder30 top uses mid:
    slot = []
";
        let (resolved, diags) = compose(S, src, "holder30", "top");
        assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
        let body = resolved.unwrap().body;
        assert_eq!(slot_annotation(&body).as_deref(), Some("ua"));
        assert_eq!(
            nested_scalar(&body, "slot", "x"),
            Some(&Value::String("2".into()))
        );

        // pin, then a discard: "established here" points at the PIN
        let src = "\
holder30 base:
    slot:
        x = \"1\"

holder30 mid uses base:
    slot as ua:
        x = \"2\"

holder30 top uses mid:
    slot = \"v\"
";
        let (_, diags) = compose(S, src, "holder30", "top");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        let pin_entry = src.find("slot as ua:").unwrap();
        let note = &diags[0].related[0];
        assert_eq!(
            note.span.start, pin_entry,
            "note at the pin entry, not the ambiguous base"
        );

        // a restated `as ua` above a pin keeps the FIRST pin's identifier
        let src = "\
holder30 base:
    slot:
        x = \"1\"

holder30 mid uses base:
    slot as ua:
        x = \"2\"

holder30 top uses mid:
    slot as ua:
        x = \"3\"
";
        let (resolved, diags) = compose(S, src, "holder30", "top");
        assert!(diags.is_empty(), "{diags:?}");
        let ann = sub_block(&resolved.unwrap().body, "slot")
            .and_then(|b| b.type_annotation.clone())
            .expect("annotated");
        let tok = src.find("as ua").unwrap() + 3;
        assert_eq!(
            (ann.span.start, ann.span.end),
            (tok, tok + 2),
            "the FIRST pin's `ua` token"
        );
    }

    #[test]
    fn mixed_spelling_lists_replace_and_the_replaced_list_is_not_judged() {
        let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"locked\"

holder7 mid uses base:
    |slot:
        - v:
            kind = \"k\"

holder7 top uses mid:
    slot as ua:
        x = \"1\"
";
        let (resolved, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "top");
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ua")
        );
    }

    #[test]
    fn shared_only_block_over_an_establishment_is_a_warned_no_op() {
        for src in [
            "holder13 base:\n    slot as ua:\n        x = \"1\"\n\nholder13 t uses base:\n    slot:\n        .note = \"m\"\n",
            "holder13 base:\n    slot:\n        - w:\n            note = \"n\"\n\nholder13 t uses base:\n    slot:\n        .note = \"m\"\n",
        ] {
            let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, src, "holder13", "t");
            assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY], "{src}");
            assert!(resolved.is_some());
        }
    }

    #[test]
    fn all_zero_item_modifier_survivor_keeps_its_spelling() {
        let src = "\
holder13 base:
    slot = []

holder13 t uses base:
    |slot:
";
        let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, src, "holder13", "t");
        assert_eq!(
            codes_of(&diags),
            [codes::ZERO_ITEM_LAYER_ENTRY, codes::ZERO_ITEM_LAYER_ENTRY]
        );
        let body = resolved.unwrap().body;
        assert!(
            body.entries.iter().any(|e| matches!(&e.kind,
                BodyEntryKind::Modifier(m) if m.name.name == "slot")),
            "a modifier survivor keeps its spelling: {body:?}"
        );
    }

    #[test]
    fn internal_invariant_wording_elides_the_root() {
        let root = internal_invariant_diag("", "a probe", InvariantOutcome::Dropped);
        assert!(
            root.message
                .starts_with("internal composition invariant violated (a probe)"),
            "{}",
            root.message
        );
        assert_eq!(root.code, Some(codes::INTERNAL_COMPOSE_INVARIANT));
        let nested = internal_invariant_diag("a.b", "a probe", InvariantOutcome::Refolded);
        assert!(
            nested.message.contains("violated at 'a.b' (a probe)")
                && nested.message.contains("fell back to a local fold"),
            "{}",
            nested.message
        );
    }

    #[test]
    fn the_judgment_memo_reuses_an_unchanged_group() {
        // A DoS defence has no behavioral signature: count the
        // judgments actually computed. Two rejected switches over one
        // unchanged list = ONE judgment; a new winner list = one more.
        let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"a\"

holder7 l1 uses base:
    slot as ua:
        x = \"1\"

holder7 l2 uses l1:
    slot as ua:
        x = \"2\"

holder7 l3 uses l2:
    slot:
        - v:
            kind = \"k\"
            secret = \"b\"

holder7 l4 uses l3:
    slot as ua:
        x = \"4\"
";
        JUDGMENT_MISSES.with(|c| c.set(0));
        let (_, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "l4");
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
                .count(),
            3
        );
        // The plan folds once (2 misses: base list, l3's list) and the
        // merge replays it — no refold, no extra judgment.
        assert_eq!(
            JUDGMENT_MISSES.with(|c| c.get()),
            2,
            "one judgment per list, reused across rejections"
        );
    }

    #[test]
    fn items_established_here_follows_the_effective_list() {
        // Under a list establishment the effective list is the highest
        // supplier: a later discard's note points there.
        let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"

holder7 mid uses base:
    slot:
        - v:
            kind = \"k\"

holder7 top uses mid:
    slot:
        x = \"1\"
";
        let (_, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "top");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        let mid_at = src.find("holder7 mid").unwrap();
        let top_at = src.find("holder7 top").unwrap();
        let note = diags[0].related[0].span.start;
        assert!(
            note > mid_at && note < top_at,
            "the winner list, not the first (nor the discard itself)"
        );
    }

    // ── union compose: round-17 battery (nothing under a seal is planned,
    //    plan/merge parity across the discriminator strip, the loud
    //    misalignment path, declarations everywhere, one sink) ─────────

    #[test]
    fn sealed_positions_are_not_planned() {
        // A `#sealed` position is owned by write-once alone: planning it
        // handed normalization the REJECTED upper layer's variant for the
        // surviving lowest body (its own findings vanished, its body was
        // normalized under a foreign vocabulary). Union and oneof twins.
        const S: &str = "\
model ua:
    x string

model ub:
    items []string

model holder31:
    slot (ua | ub) #sealed
";
        let src = "\
holder31 base:
    slot as ub:
        items = []

holder31 top uses base:
    slot as ua:
        x = \"1\"
";
        let (resolved, diags) = compose(S, src, "holder31", "top");
        assert_eq!(
            codes_of(&diags),
            [codes::ZERO_ITEM_LAYER_ENTRY, codes::SEALED_FIELD_VIOLATION],
            "the base normalizes under ITS variant (its NML2079 survives): {diags:?}"
        );
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ub")
        );
        // The direct observable: the sealed position has no plan.
        let index = index_from(S);
        let file = file_of(src);
        let instances = InstanceIndex::from_file("main.nml", &file);
        let plan = build_arm_plan(&index, "holder31", &layers_of(&instances, &["base", "top"]));
        assert!(
            plan.union_at("slot").is_none(),
            "a sealed union position is never planned"
        );

        const O: &str = "\
model arma:
    kind string
    items []string

model armb:
    kind string
    z string

oneof oo by kind:
    \"a\" -> arma
    \"b\" -> armb

model holder32:
    cfg oo #sealed
";
        let src = "\
holder32 base:
    cfg:
        kind = \"a\"
        items = []

holder32 top uses base:
    cfg:
        kind = \"b\"
        z = \"1\"
";
        let (_, diags) = compose(O, src, "holder32", "top");
        assert_eq!(
            codes_of(&diags),
            [codes::ZERO_ITEM_LAYER_ENTRY, codes::SEALED_FIELD_VIOLATION]
        );
        let index = index_from(O);
        let file = file_of(src);
        let instances = InstanceIndex::from_file("main.nml", &file);
        let plan = build_arm_plan(&index, "holder32", &layers_of(&instances, &["base", "top"]));
        assert!(
            plan.planned_arm("cfg").is_none() && !plan.decisions.contains_key("cfg"),
            "a sealed oneof position is never planned"
        );
    }

    #[test]
    fn a_union_field_named_like_the_discriminator_keeps_plan_and_merge_aligned() {
        // The merge strips a oneof's string discriminator before merging
        // an arm body; the plan must gather the same supply set (an
        // advisory NML2054 shape: a union field named `kind` under a
        // oneof keyed by `kind`), or the planned trace misaligns.
        const S: &str = "\
model va:
    p string

model vb:
    q string

model arma:
    kind (va | vb)

model armb:
    z string

oneof oo2 by kind = \"a\":
    \"a\" -> arma
    \"b\" -> armb

model holder33:
    cfg oo2
";
        let src = "\
holder33 base:
    cfg:
        kind = \"a\"

holder33 top uses base:
    cfg:
        kind as va:
            p = \"1\"
";
        let (resolved, diags) = compose(S, src, "holder33", "top");
        assert!(diags.is_empty(), "aligned, no NML2086: {diags:?}");
        let body = resolved.unwrap().body;
        let cfg = sub_block(&body, "cfg").expect("cfg");
        let kinds = cfg
            .entries
            .iter()
            .filter(|e| match &e.kind {
                BodyEntryKind::Property(p) => p.name.name == "kind",
                BodyEntryKind::NestedBlock(nb) => nb.name.name == "kind",
                _ => false,
            })
            .count();
        assert_eq!(kinds, 2, "the discriminator and the union field, once each");
    }

    #[test]
    fn a_planned_union_position_whose_kinds_drift_is_nml2086_and_still_refolds() {
        // Not constructible from the public API (one supply constructor
        // on both sides) — corrupt the plan directly: the merge must
        // notice the drift (NML2086 in every build — the debug assertion
        // lives at the compose boundary, `resolve_layers`, not on this
        // direct-`Merger` path) and still compose by a local fold.
        let index = index_from(UNION_SCHEMA);
        let file = file_of(
            "holder base:\n    slot as ua:\n        x = \"1\"\n\nholder t uses base:\n    slot:\n        x = \"2\"\n",
        );
        let instances = InstanceIndex::from_file("main.nml", &file);
        let layers = layers_of(&instances, &["base", "t"]);
        let mut plan = build_arm_plan(&index, "holder", &layers);
        plan.unions.get_mut("slot").expect("planned").kinds[1] = SupplyKind::Value;
        let mut diags = Vec::new();
        let body = Merger {
            index: &index,
            diags: &mut diags,
            origins: Vec::new(),
            plan: &plan,
        }
        .merge_root("holder", &layers);
        assert_eq!(codes_of(&diags), [codes::INTERNAL_COMPOSE_INVARIANT]);
        assert!(
            diags[0].message.contains("fell back to a local fold"),
            "{}",
            diags[0].message
        );
        assert_eq!(
            nested_scalar(&body, "slot", "x"),
            Some(&Value::String("2".into()))
        );
    }

    #[test]
    fn annotated_empty_blocks_are_writes_not_zero_item() {
        const S: &str = "\
model ua:
    x string

model uc:
    z string

model ub:
    kind string

model holder34:
    slot (ua | uc | []ub) #sealed
";
        let sealed = "\
holder34 base:
    slot as ua:
        x = \"1\"

holder34 top uses base:
    slot as ua:
";
        let (resolved, diags) = compose(S, sealed, "holder34", "top");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        assert_eq!(
            nested_scalar(&resolved.unwrap().body, "slot", "x"),
            Some(&Value::String("1".into()))
        );

        const OPEN: &str = "\
model ua:
    x string

model uc:
    z string

model ub:
    kind string

model holder35:
    slot (ua | uc | []ub)
";
        let restated = "\
holder35 base:
    slot as ua:
        x = \"1\"

holder35 top uses base:
    slot as ua:
";
        let (resolved, diags) = compose(OPEN, restated, "holder35", "top");
        assert!(
            diags.is_empty(),
            "an annotated restatement joins: {diags:?}"
        );
        assert_eq!(
            nested_scalar(&resolved.unwrap().body, "slot", "x"),
            Some(&Value::String("1".into()))
        );
        let switched = "\
holder35 base:
    slot as ua:
        x = \"1\"

holder35 top uses base:
    slot as uc:
";
        let (resolved, diags) = compose(OPEN, switched, "holder35", "top");
        assert!(
            diags.is_empty(),
            "an annotated empty block switches: {diags:?}"
        );
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("uc")
        );
    }

    #[test]
    fn rejection_after_a_new_winner_list_points_at_its_seal() {
        let src = "\
holder7 base:
    slot:
        - w:
            kind = \"k\"
            secret = \"a\"

holder7 mid uses base:
    slot:
        - v:
            kind = \"k\"
            secret = \"b\"

holder7 top uses mid:
    slot as ua:
        x = \"1\"
";
        let (_, diags) = compose(LIST_VARIANT_SCHEMA, src, "holder7", "top");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        assert!(
            diags[0].message.contains("slot[v].secret"),
            "{}",
            diags[0].message
        );
        let mid_at = src.find("holder7 mid").unwrap();
        let top_at = src.find("holder7 top").unwrap();
        let note = diags[0].related[0].span.start;
        assert!(
            note > mid_at && note < top_at,
            "sealed here points at the winner's seal"
        );
    }

    #[test]
    fn empty_spellings_on_a_declared_scalar_modifier_are_values() {
        // `|label:` / `|label = []` on a declared SCALAR modifier are
        // values (a type error the validator owns), never zero-item
        // no-ops that vanish from the composed view.
        const S: &str = "\
model m2:
    |label string
";
        for src in [
            "m2 base:\n    |label = \"a\"\n\nm2 t uses base:\n    |label:\n",
            "m2 base:\n    |label = \"a\"\n\nm2 t uses base:\n    |label = []\n",
        ] {
            let (resolved, diags) = compose(S, src, "m2", "t");
            assert!(diags.is_empty(), "{src}: {diags:?}");
            let body = resolved.unwrap().body;
            let label = body
                .entries
                .iter()
                .find(|e| matches!(&e.kind, BodyEntryKind::Modifier(m) if m.name.name == "label"))
                .expect("the composed modifier");
            let t_at = src.find("m2 t uses").unwrap();
            assert!(
                label.span.start > t_at,
                "the upper (empty) value wins: {src}"
            );
        }
    }

    #[test]
    fn distinct_items_sharing_a_non_disclosing_path_count_separately() {
        // Two scalar-keyed items render the same `slot[string]` path but
        // are two discarded seals — the sink dedups by (path, span).
        let src = "\
holder13 base:
    slot:
        - \"a\"
        - \"b\"

holder13 t uses base:
    slot as ua:
        x = \"1\"
";
        let (_, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, src, "holder13", "t");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        assert!(
            diags[0].message.contains("(and 1 more field)"),
            "{}",
            diags[0].message
        );

        let arms = "\
router base:
    route:
        \"a\" -> One:
            token = \"t1\"
        \"b\" -> One:
            token = \"t2\"

router t uses base:
    route:
        \"c\" -> Two:
            note = \"n\"
";
        let (_, diags) = compose(ARM_SET_SCHEMA, arms, "router", "t");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        assert!(
            diags[0].message.contains("(and 1 more field)"),
            "two arms' `token` seals are two FIELDS (Seg::Arm) that \
             render alike: {}",
            diags[0].message
        );
    }

    #[test]
    fn declarations_pass_through_on_every_policy() {
        // Beyond the scalar/union cells: identity lists, append
        // modifiers, oneof fields, sealed fields with a value — the
        // declaration passes through beside the composed value, and the
        // LAST declaration wins.
        const S: &str = "\
model step:
    name string+
    action string

model flow2:
    steps []step #identity
    |deny []string #append
    label string #sealed
";
        let src = "\
flow2 base:
    steps:
        - a:
            action = \"x\"
    |deny = [\"a\"]
    label = \"l\"
    |steps []step

flow2 t uses base:
    steps:
        - a:
            action = \"y\"
    |deny:
        - \"b\"
    |steps []step
    |deny []string
";
        let (resolved, diags) = compose(S, src, "flow2", "t");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        let decls: Vec<&BodyEntry> = body
            .entries
            .iter()
            .filter(|e| {
                matches!(&e.kind, BodyEntryKind::Modifier(m)
                if matches!(m.value, ModifierValue::TypeAnnotation { .. }))
            })
            .collect();
        assert_eq!(
            decls.len(),
            2,
            "one declaration per declared field: {body:?}"
        );
        let t_at = src.find("flow2 t uses").unwrap();
        assert!(
            decls.iter().all(|d| d.span.start > t_at),
            "the LAST declaration wins"
        );
        assert!(
            sub_block(&body, "steps").is_some(),
            "the identity list composed"
        );
        assert_eq!(scalar(&body, "label"), Some(&Value::String("l".into())));
    }

    #[test]
    fn the_first_list_variant_lints_wherever_it_sits() {
        for schema in [
            "model ua:\n    x string\n\nmodel ub:\n    s string #sealed\n\nmodel m:\n    slot (ua | string | []ub)\n",
            "model ua:\n    x string\n\nmodel ub:\n    s string #sealed\n\nmodel m:\n    slot (ua | set<ua> | []ub)\n",
        ] {
            let diags = validate_merge_policies(&index_from(schema));
            assert_eq!(codes_of(&diags), [codes::UNREACHABLE_SEAL], "{schema}");
            assert!(
                diags[0].message.contains("list variant `[]ub`"),
                "{}",
                diags[0].message
            );
        }
    }

    #[test]
    fn empty_block_modifiers_at_union_positions() {
        // Over a named establishment at a list-admitting union: a warned
        // no-op that keeps the value; at a listless union: a loud
        // whole-value discard (the modifier twin of the array case).
        let over_named = "\
holder13 base:
    slot as ua:
        x = \"1\"

holder13 t uses base:
    |slot:
";
        let (resolved, diags) = compose(LIST_VARIANT_SHARED_SCHEMA, over_named, "holder13", "t");
        assert_eq!(codes_of(&diags), [codes::ZERO_ITEM_LAYER_ENTRY]);
        assert_eq!(
            nested_scalar(&resolved.unwrap().body, "slot", "x"),
            Some(&Value::String("1".into()))
        );
        let listless = "\
holder base:
    slot as ua:
        x = \"1\"

holder t uses base:
    |slot:
";
        let (_, diags) = compose(UNION_SCHEMA, listless, "holder", "t");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
    }

    #[test]
    fn item_scope_notes_point_at_the_base_item() {
        // A merged identity item keeps the HEAD item's span — the base,
        // when nothing switches (as here) — so a three-layer chain's
        // discard note lands on the base entry, not the middle join.
        const S: &str = "\
model ua:
    x string

model ub:
    kind string

model holder36:
    items [](ua | []ub) #identity
";
        let src = "\
holder36 base:
    items:
        - w:
            x = \"1\"

holder36 mid uses base:
    items:
        - w:
            x = \"2\"

holder36 top uses mid:
    items:
        - w:
            - \"z\"
";
        let (_, diags) = compose(S, src, "holder36", "top");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        let mid_at = src.find("holder36 mid").unwrap();
        assert!(
            diags[0].related[0].span.start < mid_at,
            "the note points at the base item, not the middle join"
        );
    }

    #[test]
    fn item_scope_notes_follow_a_switched_head() {
        // The switching twin of `item_scope_notes_point_at_the_base_item`
        // (RFC 0019 E15): after MID's accepted `as` switch the
        // accumulated item carries MID's span, so TOP's discard note
        // lands on the establishment actually in force — the item that
        // produced the body — not on the displaced base. The base
        // authors `as ua` so it ESTABLISHES a named variant: over an
        // un-annotated (ambiguous) base, mid's `as` would be a PIN, and
        // a pin names without displacing — the head stays the base.
        const S: &str = "\
model ua:
    x string

model ub:
    kind string

model holder43:
    items [](ua | ub) #identity
";
        let src = "\
holder43 base:
    items:
        - w as ua:
            x = \"1\"

holder43 mid uses base:
    items:
        - w as ub:
            kind = \"k\"

holder43 top uses mid:
    items:
        - w:
            - \"z\"
";
        let (_, diags) = compose(S, src, "holder43", "top");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        let mid_item = src.find("- w as ub:").unwrap();
        assert_eq!(
            diags[0].related[0].span.start, mid_item,
            "the note points at the switching item: {diags:?}"
        );
    }

    #[test]
    fn structural_notes_follow_the_latest_supplier() {
        let src = "\
holder4 base:
    slot = \"a\"

holder4 mid uses base:
    slot = \"b\"

holder4 top uses mid:
    slot:
        x = \"1\"
";
        let (_, diags) = compose(SCALAR_UNION_SCHEMA, src, "holder4", "top");
        assert_eq!(codes_of(&diags), [codes::DISCARDED_UNION_CONTRIBUTION]);
        let note = &diags[0].related[0];
        assert_eq!(note.message, "in force here");
        let mid_at = src.find("holder4 mid").unwrap();
        let top_at = src.find("holder4 top").unwrap();
        assert!(note.span.start > mid_at && note.span.start < top_at);
    }

    #[test]
    fn set_first_union_still_judges_the_first_list_variants_seals() {
        // Round-17 regression: the displaced-list judgment took its
        // element from the first list-LIKE variant — a `set<string>`
        // ahead of the list variant judged under `string` (no
        // vocabulary, no scan) and the switch discarded sealed items
        // silently. Block items resolve to the first `List` everywhere
        // else; the judgment binds there, whatever precedes it.
        const S: &str = "\
model ua:
    x string

model ub:
    kind string
    secret string #sealed

model holder34:
    slot (ua | set<string> | []ub)

model holder35:
    slot (ua | []ub | set<string>)
";
        for root in ["holder34", "holder35"] {
            let src = format!(
                "{root} base:\n    slot:\n        - w:\n            kind = \"k\"\n            secret = \"s\"\n\n\
                 {root} top uses base:\n    slot as ua:\n        x = \"1\"\n"
            );
            let (_, diags) = compose(S, &src, root, "top");
            assert_eq!(
                codes_of(&diags),
                [codes::SEALED_FIELD_VIOLATION],
                "{root}: {diags:?}"
            );
            assert!(
                diags[0].message.contains("slot[w].secret"),
                "{}",
                diags[0].message
            );
            // The `.shared` twin: a list-level write distributed to the item.
            let src = format!(
                "{root} base:\n    slot:\n        .secret = \"s\"\n        - w:\n            kind = \"k\"\n\n\
                 {root} top uses base:\n    slot as ua:\n        x = \"1\"\n"
            );
            let (_, diags) = compose(S, &src, root, "top");
            assert_eq!(
                codes_of(&diags),
                [codes::SEALED_FIELD_VIOLATION],
                "{root} shared: {diags:?}"
            );
            assert!(
                diags[0].message.contains("slot[w].secret"),
                "{}",
                diags[0].message
            );
        }
    }

    #[test]
    fn union_list_items_normalize_like_a_plain_lists_items() {
        // Block-shaped items at a union position had NO normalization
        // vocabulary (an Items supply names no variant): a nested
        // `xs = []` went unwarned and an array-spelled list field kept
        // its Property spelling, unlike the same items under a plain
        // `[]ub` field. They normalize under the first `List` variant's
        // element model — the resolver's own selection.
        const S: &str = "\
model ua:
    x string

model ub:
    kind string
    tags []string
    xs []string

model holder36:
    slot (ua | []ub)

model holder37:
    slot []ub
";
        for root in ["holder36", "holder37"] {
            let src = format!(
                "{root} base:\n    slot:\n        - w:\n            kind = \"k\"\n            tags = [\"a\"]\n\n\
                 {root} top uses base:\n    slot:\n        - w:\n            kind = \"k\"\n            tags = [\"b\"]\n            xs = []\n"
            );
            let (resolved, diags) = compose(S, &src, root, "top");
            assert_eq!(
                codes_of(&diags),
                [codes::ZERO_ITEM_LAYER_ENTRY],
                "{root}: {diags:?}"
            );
            let body = resolved.unwrap().body;
            let slot = sub_block(&body, "slot").expect("the list");
            let item = slot
                .entries
                .iter()
                .find_map(|e| match &e.kind {
                    BodyEntryKind::ListItem(ListItem {
                        kind: ListItemKind::Named { body, .. },
                        ..
                    }) => Some(body),
                    _ => None,
                })
                .expect("the item");
            let tags = sub_block(item, "tags").expect("`tags` re-spelled as a block of items");
            assert_eq!(
                tags.entries
                    .iter()
                    .filter(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
                    .count(),
                1,
                "{root}: {tags:?}"
            );
        }
    }

    #[test]
    fn item_bodies_never_read_the_containers_plan() {
        // A discarded `[]ub` list under a winning `as ua` whose own `x` is
        // planned: the item's `x` (a different union, `(r | s)`) must
        // normalize under ITS variant, not `ua.x`'s plan at `slot.x` —
        // the item path is bracketed, a key the plan never writes.
        const S: &str = "\
model p:
    pa string
    xs string

model q:
    qa string

model r:
    ra string
    xs []string

model s:
    sa string

model ua:
    x (p | q)

model ub:
    kind string
    x (r | s)

model holder38:
    slot (ua | []ub)
";
        let src = "\
holder38 base:
    slot:
        - w:
            kind = \"k\"
            x as r:
                ra = \"1\"
                xs = []

holder38 top uses base:
    slot as ua:
        x as p:
            pa = \"2\"
            xs = \"a\"
";
        PLAN_LOOKUPS.with(|l| l.borrow_mut().clear());
        let (resolved, diags) = compose(S, src, "holder38", "top");
        // The base item's `xs = []` is zero-item under `r` (a list); under
        // `p` (a string) it would be a value and the warning would vanish.
        assert_eq!(
            codes_of(&diags),
            [codes::ZERO_ITEM_LAYER_ENTRY],
            "{diags:?}"
        );
        assert_eq!(
            diags[0].span.map(|s| s.start),
            src.find("xs = []"),
            "the base item's entry"
        );
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ua")
        );
        let looked: Vec<String> = PLAN_LOOKUPS.with(|l| l.borrow().clone());
        assert!(
            looked.iter().any(|p| p == "slot.x"),
            "the winner's `x` consults its plan: {looked:?}"
        );
        assert!(
            looked.iter().any(|p| p == "slot[w].x"),
            "the item's `x` asks under its own bracketed path: {looked:?}"
        );
    }

    #[test]
    fn every_planned_named_position_is_consulted_by_normalization() {
        // The prose invariant "normalization's vocabulary is the plan's
        // variant" needs both halves: the plan writes a key for every
        // named establishment, and normalization ASKS for each one (a
        // key spelled differently on either side would silently orphan
        // the plan). Spelling normalization alone is measured — the
        // positionalizer runs first and asks too.
        const S: &str = "\
model pa:
    a string

model pb:
    b string

model ua:
    inner (pa | pb)

model ub:
    y string

model holder39:
    slot (ua | ub)
";
        let src = "\
holder39 base:
    slot as ua:
        inner as pa:
            a = \"1\"

holder39 top uses base:
    slot:
        inner:
            a = \"2\"
";
        let lookups_of =
            |schema: &str, src: &str, root: &str| -> (Vec<String>, Vec<(String, bool)>) {
                let index = index_from(schema);
                let file = file_of(src);
                let instances = InstanceIndex::from_file("main.nml", &file);
                let layers = layers_of(&instances, &["base", "top"]);
                let plan = build_arm_plan(&index, root, &layers);
                let planned: Vec<(String, bool)> = plan
                    .unions
                    .iter()
                    .map(|(p, up)| {
                        (
                            p.clone(),
                            matches!(up.established, Establishment::Named { .. }),
                        )
                    })
                    .collect();
                let prepared: Vec<(InstanceId, Body)> = layers
                    .iter()
                    .map(|(id, b)| {
                        let positional =
                            crate::identity::apply_positional_planned(&index, root, b, &plan);
                        (*id, crate::resolve::apply_shared_properties(&positional))
                    })
                    .collect();
                PLAN_LOOKUPS.with(|l| l.borrow_mut().clear());
                let mut diags = Vec::new();
                let vocab = Vocab::Model(index.model(root).unwrap());
                for (id, b) in &prepared {
                    normalize_spellings(&index, vocab, b, "", &plan, id.source_path, &mut diags);
                }
                (PLAN_LOOKUPS.with(|l| l.borrow().clone()), planned)
            };
        let (looked, planned) = lookups_of(S, src, "holder39");
        let named: Vec<&String> = planned.iter().filter(|(_, n)| *n).map(|(p, _)| p).collect();
        assert!(
            named.iter().any(|p| *p == "slot") && named.iter().any(|p| *p == "slot.inner"),
            "{planned:?}"
        );
        for p in named {
            assert!(
                looked.contains(p),
                "planned position '{p}' never consulted: {looked:?}"
            );
        }
        // An AMBIGUOUS establishment is consulted too — and answered with
        // no vocabulary (never a guess).
        const AMB: &str = "\
model ua:
    note string

model ub:
    note string

model holder52:
    slot (ua | ub)
";
        let (looked, planned) = lookups_of(
            AMB,
            "holder52 base:\n    slot:\n        note = \"1\"\n\nholder52 top uses base:\n    slot:\n        note = \"2\"\n",
            "holder52",
        );
        assert_eq!(planned, [("slot".to_string(), false)], "ambiguous, planned");
        assert!(looked.iter().any(|p| p == "slot"), "{looked:?}");
        // An ITEMS establishment is planned but never asked: block-shaped
        // items are their own scope.
        const ITEMS: &str = "\
model ua:
    x string

model ub:
    kind string

model holder53:
    slot (ua | []ub)
";
        let (looked, planned) = lookups_of(
            ITEMS,
            "holder53 base:\n    slot:\n        - w:\n            kind = \"k\"\n\nholder53 top uses base:\n    slot:\n        - v:\n            kind = \"k\"\n",
            "holder53",
        );
        assert_eq!(planned, [("slot".to_string(), false)], "items, planned");
        assert!(
            !looked.iter().any(|p| p == "slot"),
            "never asked: {looked:?}"
        );
    }

    #[test]
    fn field_route_table_enumerates_every_cell() {
        // Ownership, cell by cell — a literal table, not a re-derivation:
        // seal beats union beats all-modifier beats policy.
        let index = index_from(UNION_SCHEMA);
        let union_ty = index
            .model("holder")
            .unwrap()
            .fields
            .iter()
            .find(|f| f.name == "slot")
            .map(|f| f.field_type.clone())
            .unwrap();
        use FieldRoute as R;
        use MergePolicy as P;
        type Is = fn(&FieldRoute<'_>) -> bool;
        let sealed: Is = |r| matches!(r, R::Sealed);
        let union: Is = |r| matches!(r, R::Union(_));
        let modifier: Is = |r| matches!(r, R::Modifier);
        let list: Is = |r| matches!(r, R::List);
        let overlay: Is = |r| matches!(r, R::Overlay);
        let table: [(&str, P, bool, bool, Is); 20] = [
            ("overlay", P::Overlay, false, false, overlay),
            ("overlay", P::Overlay, false, true, modifier),
            ("overlay", P::Overlay, true, false, union),
            ("overlay", P::Overlay, true, true, union),
            ("sealed", P::Sealed, false, false, sealed),
            ("sealed", P::Sealed, false, true, sealed),
            ("sealed", P::Sealed, true, false, sealed),
            ("sealed", P::Sealed, true, true, sealed),
            ("identity", P::Identity, false, false, list),
            ("identity", P::Identity, false, true, modifier),
            ("identity", P::Identity, true, false, union),
            ("identity", P::Identity, true, true, union),
            ("append", P::Append, false, false, list),
            ("append", P::Append, false, true, modifier),
            ("append", P::Append, true, false, union),
            ("append", P::Append, true, true, union),
            ("identity+append", P::IdentityAppend, false, false, list),
            ("identity+append", P::IdentityAppend, false, true, modifier),
            ("identity+append", P::IdentityAppend, true, false, union),
            ("identity+append", P::IdentityAppend, true, true, union),
        ];
        for (name, policy, has_union, all_modifiers, is) in table {
            let ty = has_union.then_some(&union_ty);
            let route = route_of(policy, ty, all_modifiers);
            assert!(
                is(&route),
                "cell ({name}, union={has_union}, all_modifiers={all_modifiers}): {route:?}"
            );
        }
        // The union route carries the position's own type.
        assert!(matches!(
            route_of(P::Overlay, Some(&union_ty), false),
            R::Union(t) if std::ptr::eq(t, &union_ty)
        ));
    }

    #[test]
    fn declarations_precede_their_value_and_the_last_one_wins_within_a_layer() {
        // Declare, then assign — as authored; two declarations in ONE
        // layer keep the last; an undeclared name's declaration passes
        // through ahead of its value too.
        const S: &str = "\
model box2:
    |label string
";
        let src = "\
box2 base:
    |label = \"a\"

box2 t uses base:
    |label string
    |label string?
    |label = \"b\"
";
        let (resolved, diags) = compose(S, src, "box2", "t");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        let is_decl = |e: &BodyEntry| {
            matches!(&e.kind, BodyEntryKind::Modifier(m)
                if matches!(m.value, ModifierValue::TypeAnnotation { .. }))
        };
        let decl_at = body.entries.iter().position(is_decl);
        let value_at = body
            .entries
            .iter()
            .position(|e| matches!(&e.kind, BodyEntryKind::Modifier(_)) && !is_decl(e));
        let (Some(d), Some(v)) = (decl_at, value_at) else {
            panic!("{body:?}");
        };
        assert!(d < v, "declare, then assign: {body:?}");
        assert_eq!(
            body.entries.iter().filter(|e| is_decl(e)).count(),
            1,
            "one declaration survives: {body:?}"
        );
        let BodyEntryKind::Modifier(m) = &body.entries[d].kind else {
            panic!("{body:?}");
        };
        let ModifierValue::TypeAnnotation { optional, .. } = &m.value else {
            panic!("{body:?}");
        };
        assert!(*optional, "the LAST declaration (`string?`) wins: {body:?}");

        let src = "\
box2 base:
    |label = \"a\"

box2 t uses base:
    |zz string
    |zz = \"v\"
";
        let (resolved, diags) = compose(S, src, "box2", "t");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        let zz: Vec<usize> = body
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(&e.kind, BodyEntryKind::Modifier(m) if m.name.name == "zz"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(zz.len(), 2, "declaration and value: {body:?}");
        assert!(is_decl(&body.entries[zz[0]]), "declaration first: {body:?}");
    }

    #[test]
    fn a_model_and_a_oneof_sharing_a_name_resolve_alike_on_every_pass() {
        // A colliding name (NML1000/NML2016 at schema load — composition
        // still runs over the loaded schema): the plan and normalization
        // resolved it model-first, the merge oneof-first, so a nested
        // union planned under `dup` the model merged under `dup` the
        // oneof's arm — a kinds mismatch, NML2086, a debug crash. One
        // resolution order (`resolve_ref`) on every pass.
        const S: &str = "\
model va:
    p string

model vb:
    q string

model arma:
    kind string
    slot (va | []vb)

model dup:
    slot (va | vb)

oneof dup by kind = \"a\":
    \"a\" -> arma

model ub:
    y string

model h40:
    pos (dup | ub)
    cfg dup
";
        for src in [
            "h40 base:\n    pos as dup:\n        slot as va:\n            p = \"1\"\n\n\
             h40 top uses base:\n    pos:\n        slot:\n",
            "h40 base:\n    cfg:\n        slot as va:\n            p = \"1\"\n\n\
             h40 top uses base:\n    cfg:\n        slot:\n",
        ] {
            let (resolved, diags) = compose(S, src, "h40", "top");
            assert!(
                !codes_of(&diags).contains(&codes::INTERNAL_COMPOSE_INVARIANT),
                "{src}: {diags:?}"
            );
            assert!(resolved.is_some(), "{src}");
        }
        // The ROOT too: the plan and the merge read a colliding root
        // oneof-first while normalization, the positionalizer and the
        // validator read the model — the identity merge under the model
        // never ran (a false "missing required field" on the composed
        // view). Model-first everywhere: the base item's `kind` survives
        // into the merged item.
        const ROOT: &str = "\
model vb:
    kind string
    q string

model arma:
    kind string
    note string

model dup2:
    slot []vb #identity

oneof dup2 by kind = \"a\":
    \"a\" -> arma
";
        let src = "\
dup2 base:
    slot:
        - w:
            kind = \"k\"
            q = \"1\"

dup2 top uses base:
    slot:
        - w:
            q = \"2\"
";
        let (resolved, diags) = compose(ROOT, src, "dup2", "top");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        let item = first_named_item_body(sub_block(&body, "slot").expect("the list"));
        assert_eq!(
            scalar(item, "kind"),
            Some(&Value::String("k".into())),
            "{body:?}"
        );
        assert_eq!(scalar(item, "q"), Some(&Value::String("2".into())));
    }

    #[test]
    fn the_annotation_source_follows_pins_and_switches() {
        // Pin then switch: the switching layer's token; switch then a
        // restated `as`: still the switch's token.
        const S: &str = "\
model ua:
    note string

model ub:
    note string

model uc:
    note string

model holder41:
    slot (ua | ub | uc)
";
        let src = "\
holder41 base:
    slot:
        note = \"1\"

holder41 mid uses base:
    slot as ua:
        note = \"2\"

holder41 top uses mid:
    slot as uc:
        note = \"3\"
";
        let (resolved, diags) = compose(S, src, "holder41", "top");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        let ann = sub_block(&body, "slot")
            .and_then(|b| b.type_annotation.clone())
            .expect("annotated");
        assert_eq!(ann.name, "uc");
        assert_eq!(
            ann.span.start,
            src.find("slot as uc").unwrap() + "slot as ".len()
        );

        let src = "\
holder41 base:
    slot as ua:
        note = \"1\"

holder41 mid uses base:
    slot as ub:
        note = \"2\"

holder41 top uses mid:
    slot as ub:
        note = \"3\"
";
        let (resolved, diags) = compose(S, src, "holder41", "top");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        let ann = sub_block(&body, "slot")
            .and_then(|b| b.type_annotation.clone())
            .expect("annotated");
        assert_eq!(ann.name, "ub");
        assert_eq!(
            ann.span.start,
            src.find("slot as ub").unwrap() + "slot as ".len(),
            "the switch's token, not the restatement's"
        );
    }

    #[test]
    fn three_layer_identity_item_provenance_is_the_base_span() {
        // A merged identity item keeps the HEAD item's position and
        // layer in the provenance table — the base, when nothing
        // switches (as here; its fields carry their own writers' rows)
        // — like a deep-merged object.
        const S: &str = "\
model step:
    name string+
    action string

model flow5:
    steps []step #identity
";
        let src = "\
flow5 base:
    steps:
        - a:
            action = \"x\"

flow5 mid uses base:
    steps:
        - a:
            action = \"y\"

flow5 top uses mid:
    steps:
        - a:
            action = \"z\"
";
        let (resolved, diags) = compose(S, src, "flow5", "top");
        assert!(diags.is_empty(), "{diags:?}");
        let origins = resolved.unwrap().origins;
        let item_rows: Vec<&Origin> = origins
            .iter()
            .filter(|(p, _)| p == "steps[a]")
            .map(|(_, o)| o)
            .collect();
        assert!(!item_rows.is_empty(), "{origins:?}");
        let base_item = src.find("- a:").unwrap();
        for o in item_rows {
            let Origin::File { span, .. } = o else {
                panic!("{o:?}");
            };
            assert_eq!(span.start, base_item, "the base item's span: {origins:?}");
        }
        let action = origins
            .iter()
            .filter(|(p, _)| p == "steps[a].action")
            .map(|(_, o)| o)
            .next_back()
            .expect("the field's row");
        let Origin::File { span, .. } = action else {
            panic!("{action:?}");
        };
        assert_eq!(
            span.start,
            src.rfind("action = \"z\"").unwrap(),
            "the field's own writer"
        );
    }

    #[test]
    fn a_switched_oneof_field_entry_follows_the_switching_layer() {
        // The head rule (RFC 0019 E15): after an accepted arm switch the
        // composed entry carries the SWITCHING layer's span, name and
        // provenance row — a finding on the composed block (the switched
        // arm's missing required field) then anchors at the layer that
        // produced the body, not at the displaced base, and two
        // switching dependents keep two distinct anchors instead of
        // collapsing onto the base's under the one-home dedup key.
        const S: &str = "\
model ga2:
    kind string
    path string

model gb2:
    kind string
    url string

oneof gcf2 by kind = \"a\":
    \"a\" -> ga2
    \"b\" -> gb2

model wrap2:
    cfg gcf2
";
        let src = "\
wrap2 base:
    cfg:
        kind = \"a\"
        path = \"p\"

wrap2 top uses base:
    cfg:
        kind = \"b\"
        url = \"u\"
";
        let (resolved, diags) = compose(S, src, "wrap2", "top");
        assert!(diags.is_empty(), "{diags:?}");
        let resolved = resolved.unwrap();
        let top_cfg = src.rfind("cfg:").unwrap();
        let entry = resolved
            .body
            .entries
            .iter()
            .find(|e| matches!(&e.kind, BodyEntryKind::NestedBlock(nb) if nb.name.name == "cfg"))
            .expect("composed cfg entry");
        assert_eq!(entry.span.start, top_cfg, "the switching layer's entry");
        let row = resolved
            .origins
            .iter()
            .find(|(p, _)| p == "cfg")
            .expect("cfg provenance row");
        let Origin::File { span, .. } = &row.1 else {
            panic!("{row:?}");
        };
        assert_eq!(span.start, top_cfg, "provenance follows the head");
    }

    #[test]
    fn an_arm_switch_inside_a_joined_union_variant_follows_the_switcher() {
        // Route 2 of the head rule (RFC 0019 E15): the base establishes
        // a union's ONEOF variant; the top joins the union (an
        // un-annotated body over a named establishment joins) but
        // switches the arm INSIDE it — the composed entry carries the
        // joining-then-switching layer's span, threaded through
        // `merge_variant_group`'s group-relative head and the `members`
        // index. The union annotation stays the establishment's.
        const S: &str = "\
model ua:
    x string

model va3:
    a string

model vb3:
    b string

oneof oo3 by kind = \"va\":
    \"va\" -> va3
    \"vb\" -> vb3

model holder50:
    slot (ua | oo3)
";
        let src = "\
holder50 base:
    slot as oo3:
        kind = \"va\"
        a = \"1\"

holder50 top uses base:
    slot:
        kind = \"vb\"
        b = \"2\"
";
        let (resolved, diags) = compose(S, src, "holder50", "top");
        assert!(diags.is_empty(), "{diags:?}");
        let resolved = resolved.unwrap();
        let top_slot = src.rfind("slot:").unwrap();
        let entry = resolved
            .body
            .entries
            .iter()
            .find(|e| matches!(&e.kind, BodyEntryKind::NestedBlock(nb) if nb.name.name == "slot"))
            .expect("composed slot entry");
        assert_eq!(entry.span.start, top_slot, "the switching layer's entry");
        let row = resolved
            .origins
            .iter()
            .find(|(p, _)| p == "slot")
            .expect("slot provenance row");
        let Origin::File { span, .. } = &row.1 else {
            panic!("{row:?}");
        };
        assert_eq!(span.start, top_slot, "provenance follows the head");
    }

    #[test]
    fn a_switched_identity_item_follows_the_switching_layers_span_and_owner() {
        // The head rule at ITEM scope — the switching twin of
        // `three_layer_identity_item_provenance_is_the_base_span`: an
        // identity item whose body switches arms carries the switching
        // item's span and provenance row.
        const S: &str = "\
model va2:
    kind string
    a string

model vb2:
    kind string
    b string

oneof oo2 by kind = \"va\":
    \"va\" -> va2
    \"vb\" -> vb2

model holder42:
    xs []oo2 #identity
";
        let src = "\
holder42 base:
    xs:
        - w:
            kind = \"va\"
            a = \"1\"

holder42 top uses base:
    xs:
        - w:
            kind = \"vb\"
            b = \"2\"
";
        let (resolved, diags) = compose(S, src, "holder42", "top");
        assert!(diags.is_empty(), "{diags:?}");
        let resolved = resolved.unwrap();
        let top_item = src.rfind("- w:").unwrap();
        let rows: Vec<&Origin> = resolved
            .origins
            .iter()
            .filter(|(p, _)| p == "xs[w]")
            .map(|(_, o)| o)
            .collect();
        assert!(!rows.is_empty(), "{:?}", resolved.origins);
        for o in rows {
            let Origin::File { span, .. } = o else {
                panic!("{o:?}");
            };
            assert_eq!(span.start, top_item, "the switching item's span");
        }
    }

    #[test]
    fn nested_sealed_list_items_count_separately_across_items() {
        // Two items, each with a nested sealed list item at the same
        // relative path: distinct paths, two hits.
        const S: &str = "\
model leaf:
    name string+
    a string #sealed

model ub:
    kind string
    steps []leaf #identity

model ua:
    x string

model holder42:
    slot (ua | []ub)
";
        let src = "\
holder42 base:
    slot:
        - w:
            kind = \"k\"
            steps:
                - x:
                    a = \"1\"
        - v:
            kind = \"k\"
            steps:
                - x:
                    a = \"2\"

holder42 top uses base:
    slot as ua:
        x = \"1\"
";
        let (_, diags) = compose(S, src, "holder42", "top");
        assert_eq!(
            codes_of(&diags),
            [codes::SEALED_FIELD_VIOLATION],
            "{diags:?}"
        );
        assert!(
            diags[0].message.contains("slot[w].steps[x].a")
                && diags[0].message.contains("(and 1 more field)"),
            "{}",
            diags[0].message
        );
    }

    #[test]
    fn discriminator_strip_parity_holds_at_every_plan_site() {
        // The plan hides a oneof's stated string discriminator from its
        // supply gather exactly where the merge strips it: at a oneof
        // ROOT, under a oneof-typed field, and under a oneof VARIANT of a
        // union — with a union field named like the discriminator at
        // each, a non-string `kind`, a twice-stated one, and a `.shared`
        // one. None misaligns (no NML2086 — a debug crash at the
        // boundary).
        const S: &str = "\
model va:
    p string

model vb:
    q string

model arma:
    kind (va | vb)

model armb:
    z string

oneof oo3 by kind = \"a\":
    \"a\" -> arma
    \"b\" -> armb

model ua:
    x string

model holder43:
    cfg oo3
    slot (oo3 | ua)
";
        for src in [
            "oo3 base:\n    kind = \"a\"\n\noo3 top uses base:\n    kind as va:\n        p = \"1\"\n",
            "holder43 base:\n    cfg:\n        kind = \"a\"\n\n\
             holder43 top uses base:\n    cfg:\n        kind as va:\n            p = \"1\"\n",
            "holder43 base:\n    slot as oo3:\n        kind = \"a\"\n\n\
             holder43 top uses base:\n    slot as oo3:\n        kind as va:\n            p = \"1\"\n",
            "holder43 base:\n    cfg:\n        kind = \"a\"\n\n\
             holder43 top uses base:\n    cfg:\n        kind = 5\n",
            "holder43 base:\n    cfg:\n        kind = \"a\"\n        kind = \"b\"\n\n\
             holder43 top uses base:\n    cfg:\n        kind as va:\n            p = \"1\"\n",
            "holder43 base:\n    cfg:\n        .kind = \"a\"\n\n\
             holder43 top uses base:\n    cfg:\n        kind as va:\n            p = \"1\"\n",
        ] {
            let root = if src.starts_with("oo3") {
                "oo3"
            } else {
                "holder43"
            };
            let (resolved, diags) = compose(S, src, root, "top");
            assert!(
                !codes_of(&diags).contains(&codes::INTERNAL_COMPOSE_INVARIANT),
                "{src}: {diags:?}"
            );
            assert!(resolved.is_some(), "{src}");
        }
    }

    #[test]
    fn set_variant_admits_the_empty_array_as_zero_item() {
        // `= []` at `(ua | set<string>)` is a warned no-op that keeps the
        // established body; an array literal under a set-first union
        // followed by a named switch composes clean (no list to judge).
        const S: &str = "\
model ua:
    x string

model holder44:
    slot (ua | set<string>)

model holder45:
    slot (set<string> | ua)
";
        let src = "holder44 base:\n    slot as ua:\n        x = \"1\"\n\nholder44 top uses base:\n    slot = []\n";
        let (resolved, diags) = compose(S, src, "holder44", "top");
        assert_eq!(
            codes_of(&diags),
            [codes::ZERO_ITEM_LAYER_ENTRY],
            "{diags:?}"
        );
        assert_eq!(
            nested_scalar(&resolved.unwrap().body, "slot", "x"),
            Some(&Value::String("1".into()))
        );
        let src = "holder45 base:\n    slot = [\"a\"]\n\nholder45 top uses base:\n    slot as ua:\n        x = \"1\"\n";
        let (resolved, diags) = compose(S, src, "holder45", "top");
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(
            slot_annotation(&resolved.unwrap().body).as_deref(),
            Some("ua")
        );
    }

    #[test]
    fn zero_item_spellings_at_a_declared_list_modifier_are_one_predicate() {
        // Every spelling of "nothing" above a declared list modifier is
        // the same warned no-op: the base's item survives.
        const S: &str = "\
model m3:
    |xs []string
";
        for upper in ["    xs = []\n", "    xs:\n", "    |xs:\n", "    |xs = []\n"] {
            let src = format!("m3 base:\n    |xs = [\"a\"]\n\nm3 t uses base:\n{upper}");
            let (resolved, diags) = compose(S, &src, "m3", "t");
            assert_eq!(
                codes_of(&diags),
                [codes::ZERO_ITEM_LAYER_ENTRY],
                "{upper:?}: {diags:?}"
            );
            let body = resolved.unwrap().body;
            let count = body.entries.iter().find_map(|e| match &e.kind {
                BodyEntryKind::Modifier(m) if m.name.name == "xs" => match &m.value {
                    ModifierValue::Block(items) => Some(items.len()),
                    ModifierValue::Inline(sv) => match &sv.value {
                        Value::Array(vs) => Some(vs.len()),
                        _ => None,
                    },
                    ModifierValue::TypeAnnotation { .. } => None,
                },
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "xs" => Some(
                    nb.body
                        .entries
                        .iter()
                        .filter(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
                        .count(),
                ),
                _ => None,
            });
            assert_eq!(
                count,
                Some(1),
                "{upper:?}: the base item survives: {body:?}"
            );
        }
    }

    #[test]
    fn two_files_with_identical_spans_count_their_seals_separately() {
        // The sink dedups by (path, FILE, span): byte-identical files
        // assigning one sealed field are two assignments, two hits.
        let index = index_from(
            "model ua:\n    x string\n\nmodel ub:\n    y string #sealed\n\n\
             model holder46:\n    slot (ua | ub)\n",
        );
        let same = "holder46 base:\n    slot as ub:\n        y = \"1\"\n";
        let fa = file_of(same);
        let fb = file_of(same);
        let fc = file_of("holder46 top:\n    slot as ua:\n        x = \"2\"\n");
        let ia = InstanceIndex::from_file("a.nml", &fa);
        let ib = InstanceIndex::from_file("b.nml", &fb);
        let ic = InstanceIndex::from_file("c.nml", &fc);
        let mut layers = layers_of(&ia, &["base"]);
        layers.extend(layers_of(&ib, &["base"]));
        layers.extend(layers_of(&ic, &["top"]));
        let plan = build_arm_plan(&index, "holder46", &layers);
        let mut diags = Vec::new();
        Merger {
            index: &index,
            diags: &mut diags,
            origins: Vec::new(),
            plan: &plan,
        }
        .merge_root("holder46", &layers);
        // The middle file's restatement is itself NML2060; the rejection
        // is ONE field assigned TWICE — `(2 assignments)`, a note per
        // assignment, each carrying its own file structurally
        // (`Related.source`, E17 — the parenthetical left the message).
        assert_eq!(
            codes_of(&diags),
            [codes::SEALED_FIELD_VIOLATION, codes::SEALED_FIELD_VIOLATION],
            "{diags:?}"
        );
        let rejection = diags
            .iter()
            .find(|d| d.message.contains("(2 assignments)"))
            .unwrap_or_else(|| panic!("one field, two files: {diags:?}"));
        assert_eq!(rejection.related.len(), 2, "{rejection:?}");
        for r in &rejection.related {
            assert_eq!(r.message, "sealed here", "{rejection:?}");
        }
        let sources: Vec<&str> = rejection
            .related
            .iter()
            .filter_map(|r| rejection.related_source(r))
            .collect();
        assert!(
            sources.contains(&"a.nml") && sources.contains(&"b.nml"),
            "{rejection:?}"
        );
    }

    #[test]
    fn arm_switch_backstop_combines_field_and_assignment_counts() {
        // The fourth suffix arm — more fields than one AND more
        // assignments than fields: base assigns secret+token, mid
        // RESTATES secret (the scan is value-blind: an equal restatement
        // is still its own (file, span) site), so the displaced group
        // [base, mid] carries 3 sites over 2 identities. The arms
        // declare no `kind` — a declared discriminator-named field is
        // the NML2054 shape, not this fixture's.
        let schema = "\
model arma:
    secret string #sealed
    token string #sealed

model armb:
    note string?

oneof svc by kind = \"a\":
    \"a\" -> arma
    \"b\" -> armb
";
        let src = "\
svc base:
    kind = \"a\"
    secret = \"s\"
    token = \"t\"

svc mid uses base:
    secret = \"s\"

svc top uses mid:
    kind = \"b\"
";
        let (_, diags) = compose(schema, src, "svc", "top");
        // Exactly two NML2060s: the backstop rejection plus mid's own
        // equal-value restatement (merge_sealed). Unit-level order is
        // NOT the CLI's span-sorted print — locate by message, never by
        // index.
        assert_eq!(
            codes_of(&diags),
            [codes::SEALED_FIELD_VIOLATION, codes::SEALED_FIELD_VIOLATION],
            "{diags:?}"
        );
        let rejection = diags
            .iter()
            .find(|d| d.message.starts_with("arm switch to `kind = \"b\"`"))
            .unwrap_or_else(|| panic!("backstop rejection: {diags:?}"));
        assert!(
            rejection.message.contains(
                "would discard the assigned `#sealed` field 'secret' \
                 (and 1 more field; 3 assignments)"
            ),
            "{}",
            rejection.message
        );
        assert_eq!(
            rejection.related.len(),
            3,
            "one note per site: {rejection:?}"
        );
        assert!(
            rejection.related.iter().all(|r| r.message == "sealed here"),
            "{rejection:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("already sealed to this same value")),
            "the incidental restatement is the deletion-fix flavor: {diags:?}"
        );
    }

    #[test]
    fn a_wide_backstop_judgment_counts_in_linear_time() {
        // Regression (DoS class): the emitter's field/assignment dedup
        // was a `Vec::contains` per hit — quadratic, measured near a
        // minute for 64k hits from a 1.1 MB input. Sets keep it linear;
        // the bound below is ~500x the linear cost and far under the
        // quadratic one, so it pins the complexity class, not the
        // machine.
        let file = file_of("spec base:\n    x = \"1\"\n");
        let ia = InstanceIndex::from_file("a.nml", &file);
        let id = ia.resolve_ref("base").unwrap();
        let seals: SealHits<'_> = (0..60_000usize)
            .map(|i| SealHit {
                id: FieldIdentity::default()
                    .child(Seg::Item(ItemKey::Scalar(Value::number(i as i64))))
                    .child(Seg::Field("secret".into())),
                span: Span::new(i, i + 1),
                layer: id,
            })
            .collect();
        let t0 = std::time::Instant::now();
        let d = seal_backstop_rejection(
            BackstopFace::ArmSetReplacement,
            "xs",
            &seals,
            Span::new(0, 1),
            id,
        );
        assert!(
            d.message.contains("(and 59999 more fields)"),
            "{}",
            d.message
        );
        assert_eq!(d.related.len(), RELATED_SEALS, "notes stay capped");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "quadratic emitter: {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn oneof_and_union_element_items_normalize_like_model_element_items() {
        // Oneof- and union-element items had NO normalization vocabulary
        // (only a model element resolved): a nested `xs = []` went
        // unwarned and an array-spelled list kept its Property spelling
        // — under plain lists and union positions alike, and under the
        // block-modifier spelling. Every element kind is a peer now.
        const S: &str = "\
model ua:
    x string

model arma:
    kind string
    tags []string
    xs []string

model armb:
    kind string
    z string

oneof oo by kind = \"a\":
    \"a\" -> arma
    \"b\" -> armb

model ra:
    tags []string
    xs []string

model rb:
    y string

model h47:
    steps []oo

model h48:
    slot (ua | []oo)

model h49:
    steps [](ra | rb)

model h50:
    slot (ua | [](ra | rb))

model h51:
    |steps []arma
";
        let cases = [
            ("h47", "steps", "- w:\n            kind = \"a\"", false),
            ("h48", "slot", "- w:\n            kind = \"a\"", false),
            ("h49", "steps", "- w as ra:", false),
            ("h50", "slot", "- w as ra:", false),
            ("h51", "steps", "- w:\n            kind = \"a\"", true),
        ];
        for (root, field, item, modifier) in cases {
            let spell = |extra: &str| {
                let head = if modifier {
                    format!("    |{field}:")
                } else {
                    format!("    {field}:")
                };
                format!("{head}\n        {item}\n            tags = [\"a\"]{extra}\n")
            };
            let src = format!(
                "{root} base:\n{}\n{root} top uses base:\n{}",
                spell(""),
                spell("\n            xs = []")
            );
            let (resolved, diags) = compose(S, &src, root, "top");
            assert_eq!(
                codes_of(&diags),
                [codes::ZERO_ITEM_LAYER_ENTRY],
                "{root}: {diags:?}"
            );
            let body = resolved.unwrap().body;
            let item_body: &Body = if modifier {
                body.entries
                    .iter()
                    .find_map(|e| match &e.kind {
                        BodyEntryKind::Modifier(m) if m.name.name == field => match &m.value {
                            ModifierValue::Block(items) => {
                                items.iter().find_map(|i| match &i.kind {
                                    ListItemKind::Named { body, .. } => Some(body),
                                    _ => None,
                                })
                            }
                            _ => None,
                        },
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("{root}: the modifier's item: {body:?}"))
            } else {
                first_named_item_body(sub_block(&body, field).expect("the list"))
            };
            let tags = sub_block(item_body, "tags")
                .unwrap_or_else(|| panic!("{root}: `tags` re-spelled as a block: {item_body:?}"));
            assert_eq!(
                tags.entries
                    .iter()
                    .filter(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
                    .count(),
                1,
                "{root}: {tags:?}"
            );
        }
    }

    #[test]
    fn union_list_items_normalize_under_the_first_list_variant_only() {
        // Two list variants: items resolve to the FIRST `List` everywhere
        // (the validator's reading), so `tags = ["b"]` is ub's list, not
        // uc's string.
        const S: &str = "\
model ua:
    x string

model ub:
    kind string
    tags []string

model uc:
    kind string
    tags string

model h54:
    slot (ua | []ub | []uc)
";
        let src = "\
h54 base:
    slot:
        - w:
            kind = \"k\"
            tags = [\"a\"]

h54 top uses base:
    slot:
        - w:
            kind = \"k\"
            tags = [\"b\"]
";
        let (resolved, diags) = compose(S, src, "h54", "top");
        assert!(diags.is_empty(), "{diags:?}");
        let body = resolved.unwrap().body;
        let item = first_named_item_body(sub_block(&body, "slot").expect("the list"));
        assert!(
            sub_block(item, "tags").is_some(),
            "ub's list, re-spelled: {item:?}"
        );
    }

    #[test]
    fn item_scopes_are_never_planned_and_ask_under_bracketed_paths() {
        // Three shapes: nested unions inside union-list items under an
        // ITEMS establishment (no plan at the container's `slot.x` at
        // all), a plain identity list (never a plan key), and scalar-
        // keyed items (non-disclosing type segments in the lookup path).
        const S: &str = "\
model r:
    ra string

model s:
    sa string

model ua:
    x string

model ub:
    kind string
    x (r | s)

model h55:
    slot (ua | []ub)

model h56:
    steps []ub #identity

model h57:
    slot []ub
";
        let run = |root: &str, src: &str| -> (Vec<String>, Vec<String>) {
            let index = index_from(S);
            let file = file_of(src);
            let instances = InstanceIndex::from_file("main.nml", &file);
            let layers = layers_of(&instances, &["base", "top"]);
            let plan = build_arm_plan(&index, root, &layers);
            let mut keys: Vec<String> = plan.unions.keys().cloned().collect();
            keys.extend(plan.arms.keys().cloned());
            keys.extend(plan.decisions.keys().cloned());
            PLAN_LOOKUPS.with(|l| l.borrow_mut().clear());
            let mut diags = Vec::new();
            for (id, b) in &layers {
                normalize_inlined(&index, root, b, &plan, id.source_path, &mut diags);
            }
            (keys, PLAN_LOOKUPS.with(|l| l.borrow().clone()))
        };
        let (keys, looked) = run(
            "h55",
            "h55 base:\n    slot:\n        - w:\n            kind = \"k\"\n            x as r:\n                ra = \"1\"\n\n\
             h55 top uses base:\n    slot:\n        - v:\n            kind = \"k\"\n            x as s:\n                sa = \"2\"\n",
        );
        assert_eq!(
            keys,
            ["slot"],
            "the ITEMS establishment, nothing beneath it"
        );
        assert!(
            looked.iter().any(|p| p == "slot[w].x") && looked.iter().any(|p| p == "slot[v].x"),
            "{looked:?}"
        );
        assert!(!looked.iter().any(|p| p == "slot.x"), "{looked:?}");
        let (keys, looked) = run(
            "h56",
            "h56 base:\n    steps:\n        - a:\n            kind = \"k\"\n            x as r:\n                ra = \"1\"\n\n\
             h56 top uses base:\n    steps:\n        - a:\n            x as r:\n                ra = \"2\"\n",
        );
        assert!(
            keys.is_empty(),
            "a plain list is never a plan key: {keys:?}"
        );
        assert_eq!(looked, ["steps[a].x", "steps[a].x"]);
        let (_, looked) = run(
            "h57",
            "h57 base:\n    slot:\n        - \"k1\":\n            x as r:\n                ra = \"1\"\n        - 7:\n            x as s:\n                sa = \"2\"\n\n\
             h57 top uses base:\n    slot:\n        - \"k2\":\n            x as r:\n                ra = \"3\"\n",
        );
        assert!(
            looked.iter().any(|p| p == "slot[string].x")
                && looked.iter().any(|p| p == "slot[number].x"),
            "{looked:?}"
        );
        assert!(
            !looked.iter().any(|p| p.contains("k1") || p.contains("k2")),
            "a scalar key is never disclosed: {looked:?}"
        );
    }

    #[test]
    fn a_positional_token_under_a_set_first_union_is_judged_non_disclosingly() {
        const S: &str = "\
model ua:
    x string

model ub:
    kind string+
    secret string #sealed

model h58:
    slot (ua | set<string> | []ub)
";
        let src = "\
h58 base:
    slot:
        - \"k\":
            secret = \"s\"

h58 top uses base:
    slot as ua:
        x = \"1\"
";
        let (_, diags) = compose(S, src, "h58", "top");
        assert_eq!(
            codes_of(&diags),
            [codes::SEALED_FIELD_VIOLATION],
            "{diags:?}"
        );
        assert!(
            diags[0].message.contains("slot[string].secret") && !diags[0].message.contains("\"k\""),
            "{}",
            diags[0].message
        );
    }

    #[test]
    fn a_colliding_name_is_judged_model_first_at_every_vocab_site() {
        // The seal lives on the MODEL `dup`; a list element `[]dup` and a
        // union element `(dup | ub)` are both judged under it (the
        // validator's reading) — never the oneof of the same name.
        const S: &str = "\
model arma:
    kind string

oneof dup by kind = \"a\":
    \"a\" -> arma

model dup:
    kind string
    secret string #sealed

model ua:
    x string

model ub:
    y string

model h59:
    slot (ua | []dup)

model h60:
    slot (ua | [](dup | ub))
";
        for (root, item) in [("h59", "- w:"), ("h60", "- w as dup:")] {
            let src = format!(
                "{root} base:\n    slot:\n        {item}\n            kind = \"k\"\n            secret = \"s\"\n\n\
                 {root} top uses base:\n    slot as ua:\n        x = \"1\"\n"
            );
            let (resolved, diags) = compose(S, &src, root, "top");
            assert_eq!(
                codes_of(&diags),
                [codes::SEALED_FIELD_VIOLATION],
                "{root}: {diags:?}"
            );
            assert!(
                diags[0].message.contains("slot[w].secret"),
                "{}",
                diags[0].message
            );
            assert_eq!(
                slot_annotation(&resolved.unwrap().body),
                None,
                "{root}: the switch was rejected, the list survives"
            );
        }
    }

    #[test]
    fn a_declaration_from_a_lower_layer_still_precedes_the_upper_value() {
        const S: &str = "\
model box3:
    |label string
";
        for src in [
            "box3 base:\n    |label string\n\nbox3 t uses base:\n    |label = \"b\"\n",
            "box3 base:\n    |label = \"a\"\n\nbox3 t uses base:\n    |label string\n",
            "box3 base:\n    |zz string\n\nbox3 t uses base:\n    |zz = \"v\"\n",
            "box3 base:\n    |label string\n\nbox3 mid uses base:\n    |label = \"a\"\n\nbox3 t uses mid:\n    |label string?\n",
        ] {
            let (resolved, diags) = compose(S, src, "box3", "t");
            assert!(diags.is_empty(), "{src}: {diags:?}");
            let body = resolved.unwrap().body;
            let is_decl = |e: &BodyEntry| {
                matches!(&e.kind, BodyEntryKind::Modifier(m)
                    if matches!(m.value, ModifierValue::TypeAnnotation { .. }))
            };
            let decls: Vec<usize> = body
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| is_decl(e))
                .map(|(i, _)| i)
                .collect();
            let value = body
                .entries
                .iter()
                .position(|e| matches!(&e.kind, BodyEntryKind::Modifier(_)) && !is_decl(e))
                .unwrap_or_else(|| panic!("{src}: {body:?}"));
            assert_eq!(decls.len(), 1, "{src}: one declaration: {body:?}");
            assert!(decls[0] < value, "{src}: declare, then assign: {body:?}");
        }
    }

    #[test]
    fn a_model_typed_discriminator_named_field_keeps_block_and_string_apart() {
        // The plan hides only the STRING discriminator entries; a `kind:`
        // block under a model-typed `kind` field is a value on both
        // sides — no misalignment.
        const S: &str = "\
model km:
    z string

model arma:
    kind km

model armb:
    y string

oneof oo4 by kind = \"a\":
    \"a\" -> arma
    \"b\" -> armb

model h61:
    cfg oo4
";
        let src = "\
h61 base:
    cfg:
        kind = \"a\"

h61 top uses base:
    cfg:
        kind = \"a\"
        kind:
            z = \"2\"
";
        let (resolved, diags) = compose(S, src, "h61", "top");
        assert!(
            !codes_of(&diags).contains(&codes::INTERNAL_COMPOSE_INVARIANT),
            "{diags:?}"
        );
        let body = resolved.unwrap().body;
        let cfg = sub_block(&body, "cfg").expect("cfg");
        assert_eq!(scalar(cfg, "kind"), Some(&Value::String("a".into())));
        assert_eq!(
            nested_scalar(cfg, "kind", "z"),
            Some(&Value::String("2".into()))
        );
    }

    #[test]
    fn surviving_indexes_is_the_one_survivorship_rule() {
        use ArmDecision as D;
        let id = InstanceId {
            source_path: "m.nml",
            name: "b",
        };
        let trace = |ds: Vec<ArmDecision<'static>>| -> Vec<Decision<'static>> {
            ds.into_iter().map(|d| (id, d)).collect()
        };
        let rejected = || D::Rejected { seals: Vec::new() };
        let discarded = || D::Discarded {
            over: Establishment::Value,
            lost: Establishment::Items,
        };
        assert_eq!(
            surviving_indexes(&trace(vec![
                D::Join,
                D::Pinned,
                D::Switch,
                rejected(),
                discarded(),
                D::Join
            ])),
            [2, 5],
            "a switch restarts; a rejection and a discard contribute nothing"
        );
        assert_eq!(surviving_indexes(&trace(vec![D::Join, D::Pinned])), [0, 1]);
        assert_eq!(surviving_indexes(&trace(vec![D::Switch, D::Switch])), [1]);
        assert!(surviving_indexes(&trace(vec![rejected(), discarded()])).is_empty());
        assert!(surviving_indexes(&[]).is_empty());
    }

    fn tamper_slot_kinds(plan: &mut ArmPlan<'_>) {
        plan.unions.get_mut("slot").expect("planned").kinds[1] = SupplyKind::Value;
    }

    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "internal composition invariant violated")
    )]
    fn a_compose_driven_invariant_is_loud_at_the_boundary() {
        // The one way to reach an internal invariant through the public
        // API — the test-only tamper seam on the plan `resolve_layers`
        // just built — proves the boundary assertion is LIVE in debug
        // builds; in release the diagnostic is NML2086 and composition
        // refolds.
        PLAN_TAMPER.with(|c| c.set(Some(tamper_slot_kinds)));
        let (resolved, diags) = compose(
            UNION_SCHEMA,
            "holder base:\n    slot as ua:\n        x = \"1\"\n\nholder t uses base:\n    slot:\n        x = \"2\"\n",
            "holder",
            "t",
        );
        assert_eq!(codes_of(&diags), [codes::INTERNAL_COMPOSE_INVARIANT]);
        assert_eq!(
            nested_scalar(&resolved.unwrap().body, "slot", "x"),
            Some(&Value::String("2".into()))
        );
    }

    #[test]
    fn dependent_composes_do_not_rereport_a_clause_finding() {
        // The declaring-clause NML2059 goes through the one-home dedup
        // like every other finding — a dependent block's compose
        // re-encounters the same clause and must not re-report it.
        let schema = "model thing:\n    v string\n";
        let src = "\
thing bad uses missing:
    v = \"b\"

thing good2 uses bad:
    v = \"g\"
";
        let index = index_from(schema);
        let file = file_of(src);
        let out = compose_file(&index, "main.nml", &file, &OpenContext);
        let n = out
            .diagnostics
            .iter()
            .filter(|d| d.code == Some(codes::UNRESOLVED_LAYER_REF))
            .count();
        assert_eq!(n, 1, "one defect, one finding: {:?}", out.diagnostics);
    }

    #[test]
    fn unindexed_refs_never_fail_silently() {
        // `resolve_layers` is the documented embedder entry point: every
        // failure carries a diagnostic — a bare (None, []) is a vanishing
        // instance with no explanation.
        let schema = "model thing:\n    v string\n";
        let index = index_from(schema);
        let file = file_of("thing t:\n    v = \"t\"\n");
        let instances = InstanceIndex::from_file("main.nml", &file);
        let declaring = instances.resolve_ref("t").unwrap();
        let ghost = InstanceId {
            source_path: "main.nml",
            name: "doesNotExist",
        };
        let (resolved, diags) = resolve_layers(
            &index,
            &instances,
            declaring,
            "thing",
            &[ghost],
            &instances.get(declaring).unwrap().body,
            &OpenContext,
        );
        assert!(resolved.is_none());
        assert!(
            codes_of(&diags).contains(&codes::UNRESOLVED_LAYER_REF),
            "the failure explains itself: {diags:?}"
        );
    }

    #[test]
    fn type_annotation_modifiers_survive_composition() {
        // An all-annotation group is the field's authored declaration —
        // deleting the entry from the composed body was silent data loss.
        let src = "\
box base:
    |x []string
    label = \"a\"

box t uses base:
    label = \"b\"
";
        let (resolved, _) = compose("", src, "box", "t");
        let body = resolved.unwrap().body;
        assert!(
            body.entries.iter().any(|e| matches!(&e.kind,
                BodyEntryKind::Modifier(m) if m.name.name == "x")),
            "the annotation entry survives: {body:?}"
        );
    }

    #[test]
    fn block_form_empty_modifier_draws_nml2079() {
        // The one zero-item spelling that escaped: `|deny:` with no items
        // — "always diagnosed, never silently ignored" admits no spelling
        // exception.
        let schema = "\
model m:
    |deny []string #append
";
        let src = "\
m base:
    |deny:
        - \"a\"

m t uses base:
    |deny:
";
        let (resolved, diags) = compose(schema, src, "m", "t");
        assert!(
            codes_of(&diags).contains(&codes::ZERO_ITEM_LAYER_ENTRY),
            "block-form empty modifier is a warned no-op: {diags:?}"
        );
        let body = resolved.unwrap().body;
        let items = body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::Modifier(m) if m.name.name == "deny" => match &m.value {
                    ModifierValue::Block(items) => Some(items.len()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or(0);
        assert_eq!(items, 1, "and the base's items survive: {body:?}");
    }

    #[test]
    fn equal_value_seal_detection_spans_spellings() {
        let schema = "\
model m:
    a string #sealed
";
        let src = "\
m base:
    a = \"x\"

m t uses base:
    |a = \"x\"
";
        let (_, diags) = compose(schema, src, "m", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("seal fires");
        assert!(
            d.message.contains("delete this assignment"),
            "the equal-value form (and its machine fix) survives the \
             modifier spelling: {}",
            d.message
        );
        assert!(!d.suggestions.is_empty());
    }

    #[test]
    fn same_layer_sealed_duplicates_read_correctly() {
        let schema = "\
model m:
    a string #sealed
";
        let src = "\
m base:
    v0 = \"x\"

m t uses base:
    a = \"x\"
    a = \"y\"
";
        let (_, diags) = compose(schema, src, "m", "t");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .expect("seal fires");
        assert!(
            d.message.contains("in this same layer"),
            "same-body duplicates name the real relationship: {}",
            d.message
        );
    }

    #[test]
    fn schemaless_item_bearing_groups_replace_wholesale() {
        // The bare-list rule binds structural mode: deep-merging an
        // item-bearing nested group concatenates layers' items and
        // duplicates restated identities (base a,b + overlay a → a,b,a).
        let src = "\
box base:
    steps:
        - \"a\"
        - \"b\"

box t uses base:
    steps:
        - \"a\"
";
        let (resolved, _) = compose("", src, "box", "t");
        let body = resolved.unwrap().body;
        assert_eq!(
            list_names(&body, "steps").len(),
            1,
            "the supplying overlay replaces wholesale — no concatenation, \
             no duplicated identity: {body:?}"
        );
        // Named restatement: the overlay's item wins alone, never both.
        let src2 = "\
box base:
    steps:
        - s1:
            v = \"1\"

box t uses base:
    steps:
        - s1:
            v = \"2\"
";
        let (resolved, _) = compose("", src2, "box", "t");
        let body = resolved.unwrap().body;
        assert_eq!(list_names(&body, "steps"), vec!["s1"]);
    }

    #[test]
    fn nml2077_names_a_rotation_across_three_clauses() {
        // Neither a transitive-base pair nor an opposed shared pair — the
        // orders rotate. The fallback used to assert exactly the two
        // causes that were just ruled out; now it names the cycle.
        let schema = "model thing:\n    v string\n";
        let src = "\
thing p:
    v = \"p\"

thing q:
    v = \"q\"

thing r:
    v = \"r\"

thing a uses q, p:
    v = \"a\"

thing b uses r, q:
    v = \"b\"

thing c uses p, r:
    v = \"c\"

thing top uses a, b, c:
    v = \"t\"
";
        let (_, diags) = compose(schema, src, "thing", "top");
        let d = diags
            .iter()
            .find(|d| d.code == Some(codes::INCONSISTENT_LINEARIZATION))
            .expect("NML2077 fires");
        assert!(
            d.message.contains("orders rotate"),
            "the rotation shape is named, not the ruled-out patterns: {}",
            d.message
        );
        assert!(
            d.message.contains("above"),
            "renders the cycle's pairwise steps: {}",
            d.message
        );
    }

    #[test]
    fn duplicate_field_names_are_coherent_everywhere() {
        // ONE policy governs a duplicate field name — the FIRST
        // declaration's — in the merge AND the backstop scan. Previously
        // the scan read any-sealed: the engine refused switches to
        // protect a seal it did not itself enforce.
        let schema = "\
model ara:
    kind string
    v string
    v string #sealed

model arb:
    kind string
    other string

oneof cf by kind = \"a\":
    \"a\" -> ara
    \"b\" -> arb

model box3:
    cfg cf
";
        // Open-first duplicate: restating v composes (first-wins, open)…
        let src = "\
box3 base:
    cfg:
        v = \"x\"

box3 t uses base:
    cfg:
        kind = \"b\"
        other = \"y\"
";
        let (resolved, diags) = compose(schema, src, "box3", "t");
        assert!(
            !codes_of(&diags).contains(&codes::SEALED_FIELD_VIOLATION),
            "…so the switch over it must be legal too: {diags:?}"
        );
        assert_eq!(
            nested_scalar(&resolved.unwrap().body, "cfg", "kind"),
            Some(&Value::String("b".into()))
        );
        // Sealed-first duplicate: both sides enforce.
        let schema_sealed_first = schema.replace(
            "    v string\n    v string #sealed",
            "    v string #sealed\n    v string",
        );
        let (_, diags) = compose(&schema_sealed_first, src, "box3", "t");
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)
                    && d.message.contains("cannot launder")),
            "sealed-first governs in the backstop too: {diags:?}"
        );
    }

    #[test]
    fn rejected_parent_switch_keeps_nested_plans_aligned() {
        // A REJECTED parent switch (not just an accepted one) must leave
        // nested positions planned over the true surviving membership.
        let src = "\
po base:
    pk = \"b\"
    sub:
        sk = \"sx\"
        v = \"one\"

po mid uses base:
    pk = \"a\"
    note = \"n\"

po top uses mid:
    sub:
        sk = \"sx\"
        v = \"two\"
";
        // mid's switch a→ (from b) discards base's sub carrying the
        // sealed v — rejected. Survivors: base, top (mid contributes
        // nothing). top's restatement of v must then hit base's seal.
        let (_, diags) = compose(NESTED_UNDER_PARENT_SCHEMA, src, "po", "top");
        let seals: Vec<_> = diags
            .iter()
            .filter(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION))
            .collect();
        assert!(
            seals.iter().any(|d| d.message.contains("cannot launder")),
            "mid's switch is rejected: {diags:?}"
        );
        assert!(
            seals
                .iter()
                .any(|d| d.message.contains("sub.v") && !d.message.contains("launder")),
            "top's restatement hits base's seal through the surviving \
             group: {diags:?}"
        );
    }

    #[test]
    fn token_prehash_covers_every_scalar_kind() {
        use crate::duration::Duration;
        let h = |v: Value| token_prehash(&ItemKey::Scalar(v));
        // Durations: semantic equals collide, distinct scatter.
        let d = |s: &str| Value::Duration(Duration::parse_text(s).unwrap());
        assert_eq!(h(d("90m")), h(d("1h30m")), "equal durations share a bucket");
        assert_ne!(h(d("90m")), h(d("91m")), "distinct durations scatter");
        // Bools.
        assert_ne!(h(Value::Bool(true)), h(Value::Bool(false)));
        // Money: (amount, currency) is the identity.
        let m = |amount, cur: &str| {
            Value::Money(crate::money::Money {
                amount,
                currency: cur.into(),
                exponent: 2,
            })
        };
        assert_eq!(h(m(150, "USD")), h(m(150, "USD")));
        assert_ne!(h(m(150, "USD")), h(m(151, "USD")));
        assert_ne!(h(m(150, "USD")), h(m(150, "EUR")));
        // Numbers: the normalized pair — `1` and `1.0` are one identity.
        let n = |s: &str| Value::Number(s.parse().unwrap());
        assert_eq!(h(n("1")), h(n("1.0")), "semantic equals share a bucket");
        assert_ne!(h(n("1")), h(n("2")), "distinct numbers scatter");
        // Strings.
        assert_eq!(h(Value::String("a".into())), h(Value::String("a".into())));
        assert_ne!(h(Value::String("a".into())), h(Value::String("b".into())));
    }

    #[test]
    fn item_key_hash_is_coherent_with_same() {
        // `ItemKey: Eq + Hash` (Part D identity): eq is `same`, and
        // equal keys MUST hash equal — the `token_prehash` rule, tagged
        // by kind (`same` requires `same_kind`, so cross-kind
        // token-equals need not collide).
        use crate::duration::Duration;
        use std::hash::{Hash, Hasher};
        let hash_of = |k: &ItemKey| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            k.hash(&mut h);
            h.finish()
        };
        let d = |s: &str| ItemKey::Scalar(Value::Duration(Duration::parse_text(s).unwrap()));
        assert!(d("90m").same(&d("1h30m")));
        assert_eq!(hash_of(&d("90m")), hash_of(&d("1h30m")), "eq ⇒ hash-eq");
        assert_eq!(d("90m"), d("1h30m"), "PartialEq IS `same`");
        assert_ne!(
            ItemKey::Named("a".into()),
            ItemKey::Reference("a".into()),
            "cross-kind token-equals are not the same identity"
        );
    }

    #[test]
    fn a_scalar_keyed_identitys_debug_never_contains_the_token() {
        // RFC 0019 requirement 4, enforced at the token holder: the
        // redacting `Debug` on `ItemKey` keeps every derived `Debug`
        // above it (Seg, FieldIdentity) non-disclosing.
        let key = ItemKey::Scalar(Value::String("hunter2-secret".into()));
        let id = FieldIdentity::default()
            .child(Seg::Item(key))
            .child(Seg::Field("secret".into()));
        let debug = format!("{id:?}");
        assert!(
            !debug.contains("hunter2"),
            "the token leaked through Debug: {debug}"
        );
        assert!(debug.contains("string"), "the TYPE renders: {debug}");
        assert_eq!(id.at("slot"), "slot[string].secret", "the ONE join");
    }

    #[test]
    fn a_shared_line_distributed_into_named_items_is_two_fields_one_assignment() {
        // Part D (E17): one list-level `.shared` line distributed into
        // two NAMED items is TWO fields (distinct identities) written by
        // ONE assignment — `(and 1 more field)`, one note at the one
        // authored site.
        const S: &str = "\
model ua:
    x string

model ub:
    name string+
    secret string #sealed

model holder47:
    slot (ua | []ub)
";
        let src = "\
holder47 base:
    slot:
        .secret = \"s\"
        - w:
        - v:

holder47 t uses base:
    slot as ua:
        x = \"1\"
";
        let (_, diags) = compose(S, src, "holder47", "t");
        assert_eq!(codes_of(&diags), [codes::SEALED_FIELD_VIOLATION]);
        assert!(
            diags[0]
                .message
                .contains("'slot[w].secret' (and 1 more field)"),
            "{}",
            diags[0].message
        );
        assert_eq!(
            diags[0].related.len(),
            1,
            "one note per distinct assignment: {:?}",
            diags[0]
        );
    }

    #[test]
    fn strip_resolves_the_stated_arm_for_oneof_items() {
        // The + token strip must follow the item's STATED (non-default)
        // arm — resolving only the default arm would miss the token and
        // draw a spurious dead-delta.
        let schema = "\
model spArm:
    ikind string
    note string

model ptArm:
    name string+
    ikind string
    note string

oneof istep by ikind = \"sp\":
    \"sp\" -> spArm
    \"pt\" -> ptArm

model flow:
    steps []istep #identity
";
        let src = "\
flow base:
    steps:
        - \"s1\":
            ikind = \"pt\"
            note = \"a\"

flow t uses base:
    steps:
        - \"s1\":
            ikind = \"pt\"
            note = \"b\"
";
        let (_, diags) = compose(schema, src, "flow", "t");
        assert!(
            !codes_of(&diags).contains(&codes::DEAD_DELTA),
            "the stated arm's + token is pairing machinery: {diags:?}"
        );
    }

    #[test]
    fn mixed_spelling_sibling_pools_group_item_seals() {
        // Base block-spelled, overlay modifier-spelled — one field, one
        // identity pool: the backstop must still see the sealed item
        // write across the spellings.
        let src = "\
box base:
    cfg:
        steps:
            - s1:
                act = \"x\"

box t uses base:
    cfg:
        skind = \"b\"
        other = \"y\"
";
        let (_, diags) = compose(MODIFIER_ITEM_SEAL_SCHEMA, src, "box", "t");
        assert!(
            diags
                .iter()
                .any(|d| d.code == Some(codes::SEALED_FIELD_VIOLATION)
                    && d.message.contains("cannot launder")),
            "block-spelled item seal blocks the switch: {diags:?}"
        );
    }

    #[test]
    fn misaligned_plan_traces_fall_back_to_a_local_refold() {
        // Defensive rail: a planned trace that does not align
        // entry-for-entry with the merge's contributions (impossible
        // today by construction; possible if normalization ever changes
        // entry counts) must be DISCARDED in favor of recomputing — a
        // misapplied stale decision is a silent wrong arm.
        let schema = "\
model ga:
    kind string
    path string

model gb:
    kind string
    url string

oneof gcf by kind = \"a\":
    \"a\" -> ga
    \"b\" -> gb
";
        let index = index_from(schema);
        let file = file_of(
            "gcf base:\n    path = \"p\"\n\ngcf t uses base:\n    kind = \"b\"\n    url = \"u\"\n",
        );
        let instances = InstanceIndex::from_file("main.nml", &file);
        let base = instances.resolve_ref("base").unwrap();
        let t = instances.resolve_ref("t").unwrap();
        let layers: Vec<(InstanceId, Body)> = vec![
            (base, instances.get(base).unwrap().body.clone()),
            (t, instances.get(t).unwrap().body.clone()),
        ];
        // A plan whose root trace has the WRONG length (one bogus entry).
        let mut bogus = ArmPlan::default();
        bogus
            .decisions
            .insert(String::new(), vec![(base, ArmDecision::Join)]);
        let mut diags = Vec::new();
        let mut merger = Merger {
            index: &index,
            diags: &mut diags,
            origins: Vec::new(),
            plan: &bogus,
        };
        let oneof = index.oneof("gcf").unwrap().clone();
        let merged = merger.merge_oneof_bodies("", &oneof, &layers).body;
        let kind = merged.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::Property(p) if p.name.name == "kind" => Some(&p.value.value),
            _ => None,
        });
        assert_eq!(
            kind,
            Some(&Value::String("b".into())),
            "the local refold applies the real switch, not the bogus Join"
        );
    }

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
