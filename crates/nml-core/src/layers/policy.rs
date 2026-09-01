//! Merge-policy directives (`#sealed`/`#identity`/`#append`/`#overlay`): parsing off the schema and the NML2068/NML2076 validation lints.

use std::collections::{HashMap, HashSet};

use crate::diagnostic::{Diagnostic, codes};
use crate::model::{FieldDef, FieldType, ModelDef, OneOfDef};
use crate::schema_index::SchemaIndex;

use super::normalize::*;

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

pub(in crate::layers) fn is_list_like(ty: &FieldType) -> bool {
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
pub(in crate::layers) fn effective_type(ty: &FieldType) -> &FieldType {
    match ty {
        FieldType::Modifier(inner) => inner,
        other => other,
    }
}

pub(in crate::layers) fn list_inner(ty: &FieldType) -> Option<&FieldType> {
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
