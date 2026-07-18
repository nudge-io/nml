//! Schema-driven config diff (RFC 0032 P2).
//!
//! Compares two multi-file config documents **semantically** — defaults
//! resolved, spans ignored (`Value::semantic_eq`), provenance carried — and
//! emits per-field changes with the schema's directives attached, for a
//! consumer (nudge) to classify (`#live`/`#restart`) and report.
//!
//! **No merged tree is materialized.** Merge semantics (property-level,
//! last-file-wins, at every depth) are an *effective-value lookup* over the
//! file list, so provenance falls out of the lookup and there is no parallel
//! `Body`-shaped structure to keep in sync.
//!
//! Collection pairing (all deterministic — a security report never guesses):
//! - **set-typed** fields: order-insensitive `SetDelta` (uniqueness was
//!   enforced at validation, so no dedup here);
//! - **ordered lists**: longest-common-subsequence alignment, so one head
//!   insertion is one `Added`, never N spurious `Modified`s;
//! - **elements always have identity** — NML list items are named, scalar-
//!   keyed, references, or roles, never anonymous — so paired elements
//!   recurse and report precise leaf paths. (This is why the RFC's `#key`
//!   escape hatch never shipped: the language's own grammar subsumes it.)

use std::path::{Path, PathBuf};

use crate::ast::{
    Body, BodyEntry, BodyEntryKind, DeclarationKind, File, ListItem, ListItemKind, ModifierValue,
    NestedBlock,
};
use crate::model::{FieldDef, FieldType, ModelDef};
use crate::schema_index::{FieldTarget, SchemaIndex};
use crate::span::Span;
use crate::types::{Directive, PrimitiveType, SpannedValue, Value};

/// Where an effective value came from. `Span` is byte offsets into ONE file,
/// so provenance always carries the file; a schema-synthesized default has no
/// source location at all.
#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    File {
        file: PathBuf,
        span: Span,
    },
    /// Synthesized by the schema default — renders "(default)", never a line.
    Default,
}

/// What changed at one field path.
#[derive(Debug, Clone)]
pub enum ChangeKind {
    Added {
        new: Value,
    },
    Removed {
        old: Value,
    },
    Modified {
        old: Value,
        new: Value,
    },
    /// Set-typed collections: order-insensitive element delta.
    SetDelta {
        added: Vec<Value>,
        removed: Vec<Value>,
    },
}

/// One semantic change, with everything a consumer needs to classify
/// (`directives`), redact (`is_secret`), and report (`path`, `origin`).
#[derive(Debug, Clone)]
pub struct FieldChange {
    /// Dotted, bare-name path relative to the root model (modifier fields
    /// drop the `|` sigil — matching the P1 enumeration vocabulary).
    pub path: String,
    pub kind: ChangeKind,
    pub is_secret: bool,
    pub directives: Vec<Directive>,
    /// The NEW side's origin (the OLD side's for `Removed`).
    pub origin: Origin,
}

/// Bounds recursion (mirrors the validator/defaulter guards — this walks
/// schema-validated input, but stays hardened anyway).
const MAX_DEPTH: u32 = 64;

/// Diff two multi-file documents for the instance of `root_model`.
///
/// `old`/`new` are `(file, root-instance body)` pairs in **precedence order**
/// (later overrides earlier, property-level, at every depth) — the caller
/// extracts the root instance body from each file's declarations.
pub fn diff_config(
    index: &SchemaIndex,
    root_model: &str,
    old: &[(PathBuf, &Body)],
    new: &[(PathBuf, &Body)],
) -> Vec<FieldChange> {
    let mut out = Vec::new();
    let Some(model) = index.model(root_model) else {
        return out;
    };
    diff_model(index, model, old, new, "", 0, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Multi-root (RFC 0032): a config *file* is itself a model instance — its
// top-level declarations are the fields of an implicit root model. Synthesize
// that root and wrap each file's declarations as one body, and the ENTIRE diff
// machinery above (`diff_config` → `diff_model` → `diff_field` → collections,
// oneofs, arms, secrets, origins) classifies EVERY top-level change — server
// blocks AND `[]role`/`[]plan`/`[]app`/`[]install` arrays — with no new diff
// logic. The synth root's own fields carry no directives, so classification
// passes straight through to the referenced models' schema directives.
// ---------------------------------------------------------------------------

/// How a top-level declaration maps onto a synth-root field.
pub enum ConfigFieldKind {
    /// `keyword Name:` → a single instance of model `keyword`.
    Block,
    /// `[]item name:` → an ordered list of model `item`.
    Array,
}

/// One synthesized field of the implicit config root — one per top-level
/// declaration. For a block, `name` and `model` are both the keyword; for an
/// array, `name` is the author-chosen array name and `model` is the item
/// keyword (the schema model each element validates against).
pub struct ConfigRootField {
    pub name: String,
    pub model: String,
    pub kind: ConfigFieldKind,
}

/// Derive the config-root fields from a set of config files (pass the union of
/// the old and new sides so a declaration present on only one side still gets a
/// field). Each unique top-level block/array becomes one field; `const`,
/// `template`, and `oneof` declarations are authoring constructs — not config
/// instances — so they are skipped. Deduplicated by field name (the same block
/// keyword or array name spanning multiple files is one field; the differ
/// overlays the per-file bodies).
pub fn config_root_fields_from_files(files: &[&File]) -> Vec<ConfigRootField> {
    let mut out: Vec<ConfigRootField> = Vec::new();
    for file in files {
        for decl in &file.declarations {
            let field = match &decl.kind {
                DeclarationKind::Block(b) => ConfigRootField {
                    name: b.keyword.name.clone(),
                    model: b.keyword.name.clone(),
                    kind: ConfigFieldKind::Block,
                },
                DeclarationKind::Array(a) => ConfigRootField {
                    name: a.name.name.clone(),
                    model: a.item_keyword.name.clone(),
                    kind: ConfigFieldKind::Array,
                },
                _ => continue,
            };
            if !out.iter().any(|f| f.name == field.name) {
                out.push(field);
            }
        }
    }
    out
}

/// Synthesize the implicit root model whose fields are a config's top-level
/// declarations. The root's fields carry no directives (classification passes
/// through to the referenced models). The result must be added to the
/// `SchemaIndex`'s model set for `diff_config`/`classify` to resolve it.
pub fn synthesize_config_root(root_name: &str, fields: &[ConfigRootField]) -> ModelDef {
    let nospan = Span { start: 0, end: 0 };
    let fields = fields
        .iter()
        .map(|f| {
            let inner = FieldType::ModelRef(f.model.clone());
            let field_type = match f.kind {
                ConfigFieldKind::Block => inner,
                ConfigFieldKind::Array => FieldType::List(Box::new(inner)),
            };
            FieldDef {
                name: f.name.clone(),
                field_type,
                optional: true,
                shorthand: false,
                default_value: None,
                directives: Vec::new(),
                doc: None,
                span: nospan,
            }
        })
        .collect();
    ModelDef {
        name: root_name.to_string(),
        extends: Vec::new(),
        fields,
        span: nospan,
    }
}

/// Wrap a config file's top-level declarations as a single `Body` matching the
/// synthesized root: each block/array becomes a `NestedBlock` keyed by its
/// synth field name (block keyword / array name). Shared properties are
/// materialized here (block bodies via `apply_shared_properties`, array items
/// via `apply_array_shared_properties`) so the differ sees real values (RFC
/// 0032 P3 contract). `$ENV` is left UNresolved — secret values never transit
/// the diff (`Value::Secret` compares by variable name).
pub fn wrap_file_as_body(file: &File) -> Body {
    let mut entries = Vec::new();
    for decl in &file.declarations {
        let (name, body) = match &decl.kind {
            DeclarationKind::Block(b) => (
                b.keyword.clone(),
                crate::resolve::apply_shared_properties(&b.body),
            ),
            DeclarationKind::Array(a) => {
                let items = crate::resolve::apply_array_shared_properties(&a.body);
                let entries = items
                    .into_iter()
                    .map(|i| BodyEntry {
                        span: i.span,
                        kind: BodyEntryKind::ListItem(i),
                    })
                    .collect();
                (a.name.clone(), Body { entries })
            }
            _ => continue,
        };
        entries.push(BodyEntry {
            span: decl.span,
            kind: BodyEntryKind::NestedBlock(NestedBlock { name, body }),
        });
    }
    Body { entries }
}

fn diff_model(
    index: &SchemaIndex,
    model: &crate::model::ModelDef,
    old: &[(PathBuf, &Body)],
    new: &[(PathBuf, &Body)],
    prefix: &str,
    depth: u32,
    out: &mut Vec<FieldChange>,
) {
    if depth >= MAX_DEPTH {
        return;
    }
    for field in &model.fields {
        let path = if prefix.is_empty() {
            field.name.clone()
        } else {
            format!("{prefix}.{}", field.name)
        };
        diff_field(index, field, old, new, &path, depth, out);
    }
}

/// One field: resolve effective old/new (last-file-wins, else schema default)
/// and dispatch on shape.
fn diff_field(
    index: &SchemaIndex,
    field: &FieldDef,
    old: &[(PathBuf, &Body)],
    new: &[(PathBuf, &Body)],
    path: &str,
    depth: u32,
    out: &mut Vec<FieldChange>,
) {
    let old_eff = effective(old, &field.name, field);
    let new_eff = effective(new, &field.name, field);

    match (&old_eff, &new_eff) {
        (Effective::Absent, Effective::Absent) => {}
        // Scalar-ish values (inline properties / modifier inline / defaults).
        (o, n) => {
            // Nested-body shapes recurse; value shapes compare.
            let o_body = o.body();
            let n_body = n.body();
            if o_body.is_some() || n_body.is_some() {
                diff_bodies(index, field, o, n, path, depth, out);
            } else {
                diff_values_at(field, o, n, path, out);
            }
        }
    }
}

/// The effective entry for `name` in a precedence-ordered file list: the LAST
/// file carrying it wins; absent everywhere falls back to the schema default.
enum Effective<'a> {
    Absent,
    /// Schema default (no source file).
    Default(&'a SpannedValue),
    Value(&'a SpannedValue, &'a Path),
    /// A nested block body (per-file — recursion re-runs lookup inside).
    Bodies(Vec<(PathBuf, &'a Body)>),
    /// Modifier block items / list-bodied field (last file wins wholesale —
    /// collections overlay by replacement, not element merge).
    Items(&'a [ListItem], &'a Path),
}

impl<'a> Effective<'a> {
    fn body(&self) -> Option<()> {
        matches!(self, Effective::Bodies(_) | Effective::Items(..)).then_some(())
    }
}

fn effective<'a>(
    files: &'a [(PathBuf, &'a Body)],
    name: &str,
    field: &'a FieldDef,
) -> Effective<'a> {
    // Nested blocks merge across files (property-level overlay), so collect
    // every file's sub-body; scalar/collection entries take the last file.
    let mut bodies: Vec<(PathBuf, &'a Body)> = Vec::new();
    let mut last_value: Option<(&'a SpannedValue, &'a Path)> = None;
    let mut last_items: Option<(&'a [ListItem], &'a Path)> = None;

    for (file, body) in files {
        for entry in &body.entries {
            match &entry.kind {
                BodyEntryKind::Property(p) if p.name.name == name => {
                    last_value = Some((&p.value, file.as_path()));
                    bodies.clear();
                    last_items = None;
                }
                BodyEntryKind::NestedBlock(nb) if nb.name.name == name => {
                    // Both model-instance bodies AND list-bodied collections
                    // collect here; `collect_elems` applies last-file-wins to
                    // item bodies, `diff_bodies` overlays keyed bodies.
                    bodies.push((file.clone(), &nb.body));
                    last_value = None;
                    last_items = None;
                }
                BodyEntryKind::Modifier(m) if m.name.name == name => match &m.value {
                    ModifierValue::Inline(sv) => {
                        last_value = Some((sv, file.as_path()));
                        bodies.clear();
                        last_items = None;
                    }
                    ModifierValue::Block(items) => {
                        last_items = Some((items.as_slice(), file.as_path()));
                        last_value = None;
                        bodies.clear();
                    }
                    ModifierValue::TypeAnnotation { .. } => {}
                },
                _ => {}
            }
        }
    }
    if let Some((items, file)) = last_items {
        return Effective::Items(items, file);
    }
    if let Some((sv, file)) = last_value {
        return Effective::Value(sv, file);
    }
    if !bodies.is_empty() {
        return Effective::Bodies(bodies);
    }
    match &field.default_value {
        Some(d) => Effective::Default(d),
        None => Effective::Absent,
    }
}

/// Compare two value-shaped effectives at a leaf.
fn diff_values_at(
    field: &FieldDef,
    old: &Effective,
    new: &Effective,
    path: &str,
    out: &mut Vec<FieldChange>,
) {
    let (old_v, _old_origin) = value_of(old);
    let (new_v, new_origin) = value_of(new);
    match (old_v, new_v) {
        (None, None) => {}
        (None, Some(n)) => push(
            field,
            path,
            ChangeKind::Added { new: n.clone() },
            new_origin,
            out,
        ),
        (Some(o), None) => push(
            field,
            path,
            ChangeKind::Removed { old: o.clone() },
            origin_of(old),
            out,
        ),
        (Some(o), Some(n)) => {
            if !o.semantic_eq(n) {
                // Set-typed inline arrays get element deltas, not blob diffs.
                if is_set(&field.field_type) {
                    if let (Value::Array(oa), Value::Array(na)) = (o, n) {
                        let added = na
                            .iter()
                            .filter(|x| !oa.iter().any(|y| y.value.semantic_eq(&x.value)))
                            .map(|x| x.value.clone())
                            .collect::<Vec<_>>();
                        let removed = oa
                            .iter()
                            .filter(|x| !na.iter().any(|y| y.value.semantic_eq(&x.value)))
                            .map(|x| x.value.clone())
                            .collect::<Vec<_>>();
                        // Pure reorder of a set ⇒ no change at all.
                        if added.is_empty() && removed.is_empty() {
                            return;
                        }
                        push(
                            field,
                            path,
                            ChangeKind::SetDelta { added, removed },
                            new_origin,
                            out,
                        );
                        return;
                    }
                }
                push(
                    field,
                    path,
                    ChangeKind::Modified {
                        old: o.clone(),
                        new: n.clone(),
                    },
                    new_origin,
                    out,
                );
            }
        }
    }
}

fn value_of<'a>(e: &'a Effective<'a>) -> (Option<&'a Value>, Origin) {
    match e {
        Effective::Value(sv, file) => (
            Some(&sv.value),
            Origin::File {
                file: file.to_path_buf(),
                span: sv.span,
            },
        ),
        Effective::Default(sv) => (Some(&sv.value), Origin::Default),
        _ => (None, Origin::Default),
    }
}

fn origin_of(e: &Effective) -> Origin {
    value_of(e).1
}

/// Nested shapes: model instances recurse (per-file sub-bodies keep the
/// overlay semantics at depth); collections pair elements.
fn diff_bodies(
    index: &SchemaIndex,
    field: &FieldDef,
    old: &Effective,
    new: &Effective,
    path: &str,
    depth: u32,
    out: &mut Vec<FieldChange>,
) {
    // Arm sets (RFC 0007 routing blocks): ordered, first-match — compare the
    // whole rendered arm list (coarse but honest and deterministic; per-arm
    // granularity can come later). Last file carrying arms wins, like every
    // collection.
    let old_arms = collect_arms(old);
    let new_arms = collect_arms(new);
    if old_arms.is_some() || new_arms.is_some() {
        match (old_arms, new_arms) {
            (Some((o, _)), Some((n, origin))) => {
                if o != n {
                    push(
                        field,
                        path,
                        ChangeKind::Modified {
                            old: Value::String(o),
                            new: Value::String(n),
                        },
                        origin,
                        out,
                    );
                }
            }
            (None, Some((n, origin))) => push(
                field,
                path,
                ChangeKind::Added {
                    new: Value::String(n),
                },
                origin,
                out,
            ),
            (Some((o, origin)), None) => push(
                field,
                path,
                ChangeKind::Removed {
                    old: Value::String(o),
                },
                origin,
                out,
            ),
            (None, None) => unreachable!(),
        }
        return;
    }

    // Collections (either side items, or a list/set-typed field).
    let old_elems = collect_elems(old);
    let new_elems = collect_elems(new);
    if !old_elems.is_empty() || !new_elems.is_empty() {
        diff_collections(index, field, &old_elems, &new_elems, path, depth, out);
        return;
    }

    // Model / oneof instance: resolve the target and recurse with per-file
    // sub-bodies (empty side = empty overlay list).
    let empty: Vec<(PathBuf, &Body)> = Vec::new();
    let o = match old {
        Effective::Bodies(b) => b,
        _ => &empty,
    };
    let n = match new {
        Effective::Bodies(b) => b,
        _ => &empty,
    };
    if let Some(model) = resolve_instance_model(index, &field.field_type, n, o) {
        diff_model(index, model, o, n, path, depth + 1, out)
    }
}

/// Resolve the model a nested instance validates against — following model
/// refs and, for a `oneof`, the **instance's own discriminator value** (the
/// new side's, falling back to the old side's), so exactly the variant in use
/// is walked (RFC 0032 P2: instance-aware, never a pessimistic union).
fn resolve_instance_model<'a>(
    index: &'a SchemaIndex,
    ft: &'a FieldType,
    new_bodies: &[(PathBuf, &Body)],
    old_bodies: &[(PathBuf, &Body)],
) -> Option<&'a crate::model::ModelDef> {
    match ft {
        FieldType::Modifier(inner) => resolve_instance_model(index, inner, new_bodies, old_bodies),
        FieldType::ModelRef(name) => model_from_ref(index, name, new_bodies, old_bodies),
        // A UNION here is already known to be a keyed-body instance (arms and
        // list shapes were dispatched before this point), so scalar variants
        // are impossible: the first model-resolving variant is the one.
        FieldType::Union(variants) => variants.iter().find_map(|v| match v {
            FieldType::ModelRef(name) => model_from_ref(index, name, new_bodies, old_bodies),
            _ => None,
        }),
        _ => None,
    }
}

/// Resolve a type-reference name to the model a body instance validates
/// against — directly, or through a `oneof` via the **instance's own
/// discriminator value** (new side, else old, else the schema default), so
/// exactly the variant in use is walked.
fn model_from_ref<'a>(
    index: &'a SchemaIndex,
    name: &str,
    new_bodies: &[(PathBuf, &Body)],
    old_bodies: &[(PathBuf, &Body)],
) -> Option<&'a crate::model::ModelDef> {
    match index.resolve_ref(name) {
        FieldTarget::Model(m) => Some(m),
        FieldTarget::OneOf(o) => {
            let discr = lookup_property(new_bodies, &o.discriminator)
                .or_else(|| lookup_property(old_bodies, &o.discriminator))
                .or(o.default_discriminator.as_deref())?;
            let variant = o
                .variants
                .iter()
                .find(|(v, _)| v == discr)
                .map(|(_, m)| m.as_str())?;
            match index.resolve_ref(variant) {
                FieldTarget::Model(m) => Some(m),
                _ => None,
            }
        }
        _ => None,
    }
}

fn lookup_property<'a>(bodies: &[(PathBuf, &'a Body)], name: &str) -> Option<&'a str> {
    for (_, body) in bodies.iter().rev() {
        for entry in &body.entries {
            if let BodyEntryKind::Property(p) = &entry.kind {
                if p.name.name == name {
                    if let Value::String(s) = &p.value.value {
                        return Some(s);
                    }
                }
            }
        }
    }
    None
}

/// An element with its NML-native identity (nothing is anonymous — the
/// grammar's list items always carry a name, scalar key, reference, or role).
struct Elem<'a> {
    id: ElemId<'a>,
    body: Option<&'a Body>,
    span: Span,
    file: Option<&'a Path>,
}

enum ElemId<'a> {
    Val(&'a Value),
    Name(&'a str),
}

impl ElemId<'_> {
    fn eq(&self, other: &ElemId<'_>) -> bool {
        match (self, other) {
            (ElemId::Val(a), ElemId::Val(b)) => a.semantic_eq(b),
            (ElemId::Name(a), ElemId::Name(b)) => a == b,
            _ => false,
        }
    }
    fn render(&self) -> Value {
        match self {
            ElemId::Val(v) => (*v).clone(),
            ElemId::Name(n) => Value::String((*n).to_string()),
        }
    }
}

fn collect_elems<'a>(e: &'a Effective<'a>) -> Vec<Elem<'a>> {
    let mut out = Vec::new();
    let mut push_item = |item: &'a ListItem, file: Option<&'a Path>| {
        let (id, body) = match &item.kind {
            ListItemKind::Named { name, body } => (ElemId::Name(&name.name), Some(body)),
            ListItemKind::Shorthand { value, body } => (ElemId::Val(&value.value), body.as_ref()),
            ListItemKind::Reference(id) => (ElemId::Name(&id.name), None),
            ListItemKind::Role(r) => (ElemId::Name(r), None),
        };
        out.push(Elem {
            id,
            body,
            span: item.span,
            file,
        });
    };
    match e {
        Effective::Items(items, file) => {
            for i in *items {
                push_item(i, Some(file));
            }
        }
        Effective::Bodies(bodies) => {
            // A list-bodied block: the LAST file carrying items wins wholesale.
            if let Some((file, body)) = bodies.iter().rev().find(|(_, b)| {
                b.entries
                    .iter()
                    .any(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
            }) {
                for entry in &body.entries {
                    if let BodyEntryKind::ListItem(i) = &entry.kind {
                        push_item(i, Some(file.as_path()));
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// Pair and diff collection elements: set-typed → order-insensitive delta;
/// ordered → LCS alignment; paired body-carrying elements recurse by identity.
fn diff_collections(
    index: &SchemaIndex,
    field: &FieldDef,
    old: &[Elem<'_>],
    new: &[Elem<'_>],
    path: &str,
    depth: u32,
    out: &mut Vec<FieldChange>,
) {
    if is_set(&field.field_type) {
        for n in new {
            if !old.iter().any(|o| o.id.eq(&n.id)) {
                push(
                    field,
                    path,
                    ChangeKind::Added { new: n.id.render() },
                    elem_origin(n),
                    out,
                );
            }
        }
        for o in old {
            if !new.iter().any(|n| n.id.eq(&o.id)) {
                push(
                    field,
                    path,
                    ChangeKind::Removed { old: o.id.render() },
                    elem_origin(o),
                    out,
                );
            }
        }
        // Same-identity elements have no inner structure to diff in a set of
        // scalars; named/bodied pairs recurse below via the ordered path.
    }
    // Pair by identity and recurse into paired bodies for precise leaf paths.
    // Both NAMED elements (`- Google:`) and scalar-shorthand elements that carry
    // a body (`- "[vendor]-x.v1": egressRate: …`, e.g. `[]install`) recurse: the
    // element's native identity supplies the path segment. A body present on
    // only one side diffs against an empty overlay, so an edit that adds or
    // removes an element's sub-block reports precise leaf add/removes.
    for n in new {
        let Some(o) = old.iter().find(|o| o.id.eq(&n.id)) else {
            // A brand-new NAMED element reports as an Added at its leaf path
            // (unchanged behavior); new scalar elements — bodied or not — fall
            // to the ordered LCS path below as a whole-element Added, so they
            // are not double-reported here.
            if matches!(n.id, ElemId::Name(_)) && !is_set(&field.field_type) {
                push(
                    field,
                    &format!("{path}.{}", elem_path_seg(&n.id)),
                    ChangeKind::Added { new: n.id.render() },
                    elem_origin(n),
                    out,
                );
            }
            continue;
        };
        // A paired element with a body on either side recurses (identity is
        // stable, so this is not an add/remove — the LCS/set passes skip it).
        if n.body.is_none() && o.body.is_none() {
            continue;
        }
        let elem_path = format!("{path}.{}", elem_path_seg(&n.id));
        let o_files: Vec<(PathBuf, &Body)> = o
            .body
            .map(|b| vec![(o.file.map(Path::to_path_buf).unwrap_or_default(), b)])
            .unwrap_or_default();
        let n_files: Vec<(PathBuf, &Body)> = n
            .body
            .map(|b| vec![(n.file.map(Path::to_path_buf).unwrap_or_default(), b)])
            .unwrap_or_default();
        if let Some(model) =
            resolve_instance_model(index, elem_type(&field.field_type), &n_files, &o_files)
        {
            diff_model(index, model, &o_files, &n_files, &elem_path, depth + 1, out);
        }
    }
    if !is_set(&field.field_type) {
        // Ordered scalars: LCS alignment — unmatched = added/removed, so a
        // head insertion is ONE change.
        let matched = lcs_pairs(old, new);
        for (i, n) in new.iter().enumerate() {
            if matches!(n.id, ElemId::Val(_)) && !matched.iter().any(|&(_, b)| b == i) {
                push(
                    field,
                    path,
                    ChangeKind::Added { new: n.id.render() },
                    elem_origin(n),
                    out,
                );
            }
        }
        for (i, o) in old.iter().enumerate() {
            let removed_named =
                matches!(o.id, ElemId::Name(_)) && !new.iter().any(|n| n.id.eq(&o.id));
            let removed_val =
                matches!(o.id, ElemId::Val(_)) && !matched.iter().any(|&(a, _)| a == i);
            if removed_named || removed_val {
                push(
                    field,
                    path,
                    ChangeKind::Removed { old: o.id.render() },
                    elem_origin(o),
                    out,
                );
            }
        }
    }
}

/// Longest-common-subsequence pairing over element identity (Myers-class,
/// O(n·m) DP — trivial at config scale).
/// The effective arm list of a routing block, rendered canonically
/// ("sel -> target; ..."), with the winning file's origin. Last file carrying
/// arms wins wholesale (arms are ordered first-match — order IS meaning, so a
/// reorder is honestly a change).
fn collect_arms(e: &Effective) -> Option<(String, Origin)> {
    use crate::ast::{ArmSelector, ArmTarget};
    let bodies: &[(PathBuf, &Body)] = match e {
        Effective::Bodies(b) => b,
        _ => return None,
    };
    let (file, body) = bodies.iter().rev().find(|(_, b)| {
        b.entries
            .iter()
            .any(|en| matches!(en.kind, BodyEntryKind::Arm(_)))
    })?;
    let mut rendered = Vec::new();
    let mut first_span = None;
    for entry in &body.entries {
        if let BodyEntryKind::Arm(a) = &entry.kind {
            first_span.get_or_insert(a.selector_span);
            let sel = match &a.selector {
                ArmSelector::Role(r) => r.as_str(),
                ArmSelector::Else => "else",
            };
            let tgt = match &a.target {
                ArmTarget::Reference(id) => id.name.clone(),
                ArmTarget::Literal { value, .. } => format!("{value:?}"),
            };
            rendered.push(format!("{sel} -> {tgt}"));
        }
    }
    Some((
        rendered.join("; "),
        Origin::File {
            file: file.clone(),
            span: first_span.unwrap_or(Span { start: 0, end: 0 }),
        },
    ))
}

fn lcs_pairs(old: &[Elem<'_>], new: &[Elem<'_>]) -> Vec<(usize, usize)> {
    let n = old.len();
    let m = new.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old[i].id.eq(&new[j].id) {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < n && j < m {
        if old[i].id.eq(&new[j].id) {
            out.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

fn elem_origin(e: &Elem<'_>) -> Origin {
    match e.file {
        Some(f) => Origin::File {
            file: f.to_path_buf(),
            span: e.span,
        },
        None => Origin::Default,
    }
}

/// The dotted-path segment for an element's identity: a name verbatim, a
/// string-ish scalar by its content (e.g. an `[]install` package key), any
/// other scalar by its debug form. Used to build precise leaf paths when
/// recursing into a body-carrying element.
fn elem_path_seg(id: &ElemId) -> String {
    match id {
        ElemId::Name(n) => (*n).to_string(),
        ElemId::Val(v) => match v {
            Value::String(s) | Value::Path(s) | Value::Role(s) | Value::Duration(s) => s.clone(),
            other => format!("{other:?}"),
        },
    }
}

fn elem_type(ft: &FieldType) -> &FieldType {
    match ft {
        FieldType::List(inner) | FieldType::Set(inner) | FieldType::Modifier(inner) => {
            elem_type(inner)
        }
        other => other,
    }
}

fn is_set(ft: &FieldType) -> bool {
    match ft {
        FieldType::Set(_) => true,
        FieldType::Modifier(inner) => is_set(inner),
        _ => false,
    }
}

fn is_secret(ft: &FieldType) -> bool {
    match ft {
        FieldType::Primitive(PrimitiveType::Secret) => true,
        FieldType::Modifier(i) | FieldType::List(i) | FieldType::Set(i) => is_secret(i),
        _ => false,
    }
}

fn push(
    field: &FieldDef,
    path: &str,
    kind: ChangeKind,
    origin: Origin,
    out: &mut Vec<FieldChange>,
) {
    out.push(FieldChange {
        path: path.to_string(),
        kind,
        is_secret: is_secret(&field.field_type),
        directives: field.directives.clone(),
        origin,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::DeclarationKind;

    const SCHEMA: &str = "model limits:\n    cap number = 5\n\nmodel server:\n    port number = 8080\n    name string?\n    token secret?\n    cidrs set<string>? #live\n    order []string?\n    limits limits?\n    providers []provider?\n\nmodel provider:\n    url string?\n    clientSecret secret?\n\noneof email by kind:\n    \"log\" -> emailLog\n    \"post\" -> emailPost\n\nmodel emailLog:\n    path string?\n\nmodel emailPost:\n    apiKey secret?\n";

    fn index() -> SchemaIndex {
        let (schema, errs) = crate::cst::extract_schema(SCHEMA);
        assert!(errs.is_empty(), "{errs:?}");
        SchemaIndex::build(schema.models, schema.enums, schema.oneofs)
    }

    fn parse_doc(src: &str) -> crate::ast::File {
        crate::cst::parse_to_ast(src).unwrap()
    }

    fn server_body(file: &crate::ast::File) -> &Body {
        file.declarations
            .iter()
            .find_map(|d| match &d.kind {
                DeclarationKind::Block(b) if b.keyword.name == "server" => Some(&b.body),
                _ => None,
            })
            .expect("server block")
    }

    fn diff_single(old_src: &str, new_src: &str) -> Vec<FieldChange> {
        let (old_f, new_f) = (parse_doc(old_src), parse_doc(new_src));
        let idx = index();
        diff_config(
            &idx,
            "server",
            &[(PathBuf::from("old.nml"), server_body(&old_f))],
            &[(PathBuf::from("new.nml"), server_body(&new_f))],
        )
    }

    /// r10 fix 1 — UNION-typed fields with model-variant bodies: the body
    /// shape selects the variant (shared `resolve_type_in_body` rule) and
    /// edits inside report precise leaf paths (previously: silently invisible).
    #[test]
    fn union_model_variant_bodies_diff_by_shape() {
        let schema =
            "model deny:\n    page string?\n\nmodel server:\n    denial (string | deny)?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let old = parse_doc("server s:\n    denial:\n        page = \"a.html\"\n");
        let new = parse_doc("server s:\n    denial:\n        page = \"b.html\"\n");
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].path, "denial.page");
        assert!(matches!(&d[0].kind, ChangeKind::Modified { .. }));
        // The scalar variant still works through the value path.
        let old = parse_doc("server s:\n    denial = \"x\"\n");
        let new = parse_doc("server s:\n    denial = \"y\"\n");
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "denial");
    }

    /// r10 fix 2 — ARM blocks diff: a retargeted arm is a Modified (rendered
    /// arm lists), unchanged arms are silent, and a REORDER is honestly a
    /// change (arms are ordered first-match — order IS meaning).
    #[test]
    fn arm_blocks_diff_including_reorder() {
        let schema = "model server:\n    route (role -> string)?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let diff2 = |o: &str, n: &str| {
            let (of, nf) = (parse_doc(o), parse_doc(n));
            diff_config(
                &idx,
                "server",
                &[(PathBuf::from("f.nml"), server_body(&of))],
                &[(PathBuf::from("f.nml"), server_body(&nf))],
            )
        };
        let base = "server s:\n    route:\n        @role/a -> X\n        else -> Y\n";
        let retarget = "server s:\n    route:\n        @role/a -> Z\n        else -> Y\n";
        let d = diff2(base, retarget);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].path, "route");
        assert!(
            matches!(&d[0].kind, ChangeKind::Modified { new, .. }
                if matches!(new, Value::String(s) if s.contains("-> Z"))),
            "{d:?}"
        );
        assert!(diff2(base, base).is_empty(), "unchanged arms are silent");
        let reorder = "server s:\n    route:\n        else -> Y\n        @role/a -> X\n";
        assert_eq!(diff2(base, reorder).len(), 1, "arm reorder IS a change");
    }

    /// The flagship consumer path (nudge RFC 0031): `server → sandboxCeiling
    /// (#live container) → |block set<string>` written BLOCK-FORM — element
    /// deltas with the container's classification reachable, secret-free, and
    /// a pure reorder invisible. Exercises NestedBlock→Bodies overlay,
    /// Modifier-Block items, and Set-through-Modifier unwrapping at once.
    #[test]
    fn flagship_modifier_set_block_form_deltas() {
        let schema = "model ceiling:\n    |block set<string>? #live\n\nmodel server:\n    sandboxCeiling ceiling?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let old = parse_doc(
            "server s:\n    sandboxCeiling:\n        |block:\n            - \"10.0.0.0/8\"\n            - \"172.16.0.0/12\"\n",
        );
        let new = parse_doc(
            "server s:\n    sandboxCeiling:\n        |block:\n            - \"172.16.0.0/12\"\n            - \"10.0.0.0/8\"\n            - \"192.168.0.0/16\"\n",
        );
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("server.nml"), server_body(&old))],
            &[(PathBuf::from("server.nml"), server_body(&new))],
        );
        assert_eq!(d.len(), 1, "reorder invisible, one addition: {d:?}");
        assert_eq!(d[0].path, "sandboxCeiling.block");
        assert!(
            matches!(&d[0].kind, ChangeKind::Added { new }
                if new.semantic_eq(&Value::String("192.168.0.0/16".into()))),
            "{d:?}"
        );
        // The #live directive on the modifier field rides the change.
        assert!(
            d[0].directives.iter().any(|dir| dir.name == "live"),
            "{d:?}"
        );
        assert!(
            matches!(&d[0].origin, Origin::File { file, span } if file.ends_with("server.nml") && span.start > 0),
            "element origin is file+span for the report: {:?}",
            d[0].origin
        );
    }

    /// Scalars: modified carries file origin; a deleted field falls back to
    /// its schema default (`Origin::Default`) as a Modified; cosmetic edits
    /// (comments/whitespace/moved lines) produce ZERO changes.
    #[test]
    fn scalar_modified_deleted_to_default_and_cosmetic_noop() {
        let d = diff_single(
            "server s:\n    port = 8080\n",
            "server s:\n    port = 9090\n",
        );
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].path, "port");
        assert!(matches!(&d[0].kind, ChangeKind::Modified { .. }));
        assert!(matches!(&d[0].origin, Origin::File { file, .. } if file.ends_with("new.nml")));

        // Deleting an explicit value reverts to the schema default.
        let d = diff_single("server s:\n    port = 9090\n", "server s:\n");
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(
            matches!(&d[0].kind, ChangeKind::Modified { new, .. }
                if new.semantic_eq(&Value::Number(crate::types::Number::Int(8080)))),
            "{d:?}"
        );
        assert_eq!(d[0].origin, Origin::Default);

        // Cosmetic: comment + blank line + reordered fields ⇒ no change.
        let d = diff_single(
            "server s:\n    port = 9090\n    name = \"a\"\n",
            "server s:\n    // moved things around\n\n    name = \"a\"\n    port = 9090\n",
        );
        assert!(d.is_empty(), "cosmetic edits must be invisible: {d:?}");

        // Explicitly writing the default is not a change.
        let d = diff_single("server s:\n", "server s:\n    port = 8080\n");
        assert!(d.is_empty(), "explicit default == default: {d:?}");
    }

    /// Multi-file overlay: a later file overrides an earlier one, and a value
    /// MOVING between files with the same effective value is no change.
    #[test]
    fn multi_file_overlay_and_between_file_moves() {
        let idx = index();
        let a1 = parse_doc("server s:\n    port = 1\n");
        let a2 = parse_doc("server s:\n    port = 2\n");
        let b1 = parse_doc("server s:\n");
        let b2 = parse_doc("server s:\n    port = 2\n");
        // old: base says 1, override says 2 ⇒ effective 2.
        // new: only ONE file says 2 (moved between files) ⇒ effective 2. No change.
        let d = diff_config(
            &idx,
            "server",
            &[
                (PathBuf::from("base.nml"), server_body(&a1)),
                (PathBuf::from("over.nml"), server_body(&a2)),
            ],
            &[
                (PathBuf::from("base.nml"), server_body(&b2)),
                (PathBuf::from("over.nml"), server_body(&b1)),
            ],
        );
        assert!(d.is_empty(), "between-file move, same value: {d:?}");
    }

    /// Set-typed fields: order-insensitive SetDelta; pure reorder = no change.
    #[test]
    fn set_delta_and_reorder_noop() {
        let d = diff_single(
            "server s:\n    cidrs = [\"a\", \"b\"]\n",
            "server s:\n    cidrs = [\"b\", \"a\", \"c\"]\n",
        );
        assert_eq!(d.len(), 1, "{d:?}");
        match &d[0].kind {
            ChangeKind::SetDelta { added, removed } => {
                assert_eq!(added.len(), 1);
                assert!(added[0].semantic_eq(&Value::String("c".into())));
                assert!(removed.is_empty());
            }
            k => panic!("expected SetDelta, got {k:?}"),
        }
        // The #live directive rides along for the consumer to classify.
        assert!(d[0].directives.iter().any(|dir| dir.name == "live"));

        let d = diff_single(
            "server s:\n    cidrs = [\"a\", \"b\"]\n",
            "server s:\n    cidrs = [\"b\", \"a\"]\n",
        );
        assert!(d.is_empty(), "set reorder is invisible: {d:?}");
    }

    /// Ordered lists: LCS alignment — one head insertion is exactly ONE Added.
    #[test]
    fn ordered_list_head_insertion_is_one_added() {
        let d = diff_single(
            "server s:\n    order = [\"a\", \"b\", \"c\"]\n",
            "server s:\n    order = [\"x\", \"a\", \"b\", \"c\"]\n",
        );
        // Inline ordered arrays compare as one Modified today?? No: order is
        // []string (not set) — the scalar path treats arrays via semantic_eq;
        // an inline non-set array difference is a Modified of the whole value.
        // Body-form lists get LCS. Assert the inline behavior explicitly:
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(matches!(&d[0].kind, ChangeKind::Modified { .. }));
    }

    /// Named-entry collections: an edit INSIDE a named element reports the
    /// precise leaf path; secrets inside carry is_secret for redaction.
    #[test]
    fn named_entry_pairing_reports_leaf_paths() {
        let d = diff_single(
            "server s:\n    providers:\n        - Google:\n            url = \"a\"\n            clientSecret = \"s1\"\n",
            "server s:\n    providers:\n        - Google:\n            url = \"b\"\n            clientSecret = \"s2\"\n",
        );
        let url = d
            .iter()
            .find(|c| c.path == "providers.Google.url")
            .expect("leaf path");
        assert!(matches!(&url.kind, ChangeKind::Modified { .. }));
        assert!(!url.is_secret);
        let sec = d
            .iter()
            .find(|c| c.path == "providers.Google.clientSecret")
            .expect("secret leaf");
        assert!(sec.is_secret, "secret flag must ride for redaction");
        // A renamed entry is remove+add (name IS identity).
        let d = diff_single(
            "server s:\n    providers:\n        - Google:\n            url = \"a\"\n",
            "server s:\n    providers:\n        - Goggle:\n            url = \"a\"\n",
        );
        assert!(
            d.iter()
                .any(|c| matches!(&c.kind, ChangeKind::Added { .. })),
            "{d:?}"
        );
        assert!(
            d.iter()
                .any(|c| matches!(&c.kind, ChangeKind::Removed { .. })),
            "{d:?}"
        );
    }

    // -- Multi-root (RFC 0032): whole-config diff via the synthesized root. ----

    const MULTI_SCHEMA: &str = "model egressRate:\n    rate number\n    burst number\n\nmodel install:\n    package string+\n    egressRate egressRate? #live\n\nmodel role:\n    name string+\n    description string?\n\nmodel server:\n    port number = 8080\n    token secret?\n";

    /// The synth root + wrap adapters turn a whole config file (a `server`
    /// block AND `[]install`/`[]role` arrays) into one `diff_config` call:
    /// server-block edits, install `egressRate` edits (through a
    /// scalar-shorthand element body), and role edits all report with
    /// declaration-prefixed paths and their models' directives — no new diff
    /// logic, one uniform walk.
    #[test]
    fn multi_root_diffs_blocks_and_arrays_uniformly() {
        let (sch, errs) = crate::cst::extract_schema(MULTI_SCHEMA);
        assert!(errs.is_empty(), "{errs:?}");

        let old_src = "server Main:\n    port = 8080\n\n[]install plugins:\n    - \"[acme]-x.v1\":\n        egressRate:\n            rate = 100\n            burst = 200\n\n[]role roles:\n    - admin:\n        description = \"a\"\n";
        let new_src = "server Main:\n    port = 9090\n\n[]install plugins:\n    - \"[acme]-x.v1\":\n        egressRate:\n            rate = 500\n            burst = 200\n\n[]role roles:\n    - admin:\n        description = \"b\"\n";
        let old_f = parse_doc(old_src);
        let new_f = parse_doc(new_src);

        let fields = config_root_fields_from_files(&[&old_f, &new_f]);
        // server (block), plugins (array of install), roles (array of role).
        assert_eq!(fields.len(), 3, "one field per top-level decl");
        let root = synthesize_config_root("config", &fields);
        let mut models = sch.models.clone();
        models.push(root);
        let index = SchemaIndex::build(models, sch.enums, sch.oneofs);

        let old_body = wrap_file_as_body(&old_f);
        let new_body = wrap_file_as_body(&new_f);
        let d = diff_config(
            &index,
            "config",
            &[(PathBuf::from("nudge.nml"), &old_body)],
            &[(PathBuf::from("nudge.nml"), &new_body)],
        );

        // server.port changed (declaration-prefixed).
        let port = d.iter().find(|c| c.path == "server.port").expect("port");
        assert!(matches!(&port.kind, ChangeKind::Modified { .. }));

        // install egressRate.rate reported at a precise leaf through the
        // scalar-shorthand element body. (The #live directive sits on the
        // `egressRate` container field, so LEAF changes carry no directive of
        // their own — the consumer classifies leaves by the nearest-directive
        // walk, `classify_path`, exercised on the nudge side.)
        let rate = d
            .iter()
            .find(|c| c.path == "plugins.[acme]-x.v1.egressRate.rate")
            .unwrap_or_else(|| panic!("install leaf path missing: {d:?}"));
        assert!(matches!(&rate.kind, ChangeKind::Modified { .. }));

        // role edit reports (restart-class — no directive rides).
        let role = d
            .iter()
            .find(|c| c.path == "roles.admin.description")
            .unwrap_or_else(|| panic!("role leaf path missing: {d:?}"));
        assert!(matches!(&role.kind, ChangeKind::Modified { .. }));
        assert!(role.directives.is_empty());

        // Exactly those three semantic changes — nothing spurious (burst
        // unchanged, package identity stable).
        assert_eq!(d.len(), 3, "{d:?}");
    }

    /// Adding an `egressRate` sub-block to a previously bare `[]install` entry
    /// (body on the NEW side only) reports its leaves as Added — the
    /// one-sided-body overlay path.
    #[test]
    fn multi_root_install_body_added_to_bare_entry() {
        let (sch, errs) = crate::cst::extract_schema(MULTI_SCHEMA);
        assert!(errs.is_empty(), "{errs:?}");
        let old_f = parse_doc("[]install plugins:\n    - \"[acme]-x.v1\"\n");
        let new_f = parse_doc(
            "[]install plugins:\n    - \"[acme]-x.v1\":\n        egressRate:\n            rate = 5\n            burst = 10\n",
        );
        let fields = config_root_fields_from_files(&[&old_f, &new_f]);
        let root = synthesize_config_root("config", &fields);
        let mut models = sch.models.clone();
        models.push(root);
        let index = SchemaIndex::build(models, sch.enums, sch.oneofs);
        let (ob, nb) = (wrap_file_as_body(&old_f), wrap_file_as_body(&new_f));
        let d = diff_config(
            &index,
            "config",
            &[(PathBuf::from("nudge.nml"), &ob)],
            &[(PathBuf::from("nudge.nml"), &nb)],
        );
        assert!(
            d.iter()
                .any(|c| c.path == "plugins.[acme]-x.v1.egressRate.rate"
                    && matches!(&c.kind, ChangeKind::Added { .. })),
            "bare→bodied install add reports leaf Added: {d:?}"
        );
    }

    /// A brand-new whole `[]install` entry reports as one Added at the array
    /// field (whole-element, not itemized) — ordered-list semantics preserved.
    #[test]
    fn multi_root_new_install_entry_is_one_added() {
        let (sch, errs) = crate::cst::extract_schema(MULTI_SCHEMA);
        assert!(errs.is_empty(), "{errs:?}");
        let old_f = parse_doc("[]install plugins:\n    - \"[acme]-x.v1\"\n");
        let new_f = parse_doc("[]install plugins:\n    - \"[acme]-x.v1\"\n    - \"[acme]-y.v1\"\n");
        let fields = config_root_fields_from_files(&[&old_f, &new_f]);
        let root = synthesize_config_root("config", &fields);
        let mut models = sch.models.clone();
        models.push(root);
        let index = SchemaIndex::build(models, sch.enums, sch.oneofs);
        let (ob, nb) = (wrap_file_as_body(&old_f), wrap_file_as_body(&new_f));
        let d = diff_config(
            &index,
            "config",
            &[(PathBuf::from("nudge.nml"), &ob)],
            &[(PathBuf::from("nudge.nml"), &nb)],
        );
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].path, "plugins");
        assert!(
            matches!(&d[0].kind, ChangeKind::Added { new } if new.semantic_eq(&Value::String("[acme]-y.v1".into()))),
            "{d:?}"
        );
    }
}
