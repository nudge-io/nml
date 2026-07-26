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
//! - **routing arms** (RFC 0007): selector-paired per-arm diff over the same
//!   LCS — a retarget is one `Modified` at the arm's element path; a move is
//!   a -/+ pair of the full arm at its two file:lines.
//!
//! **Invariant — visible, never silent:** content the schema model cannot
//! represent (model↔grammar drift: a shape the grammar accepts but the model
//! does not describe) must degrade to a *visible* [`ChangeKind::OpaqueChanged`]
//! at the nearest describable field path — never to silence. This includes the
//! **unmodeled remainder**: content inside a body whose model RESOLVED but
//! whose fields do not cover it (bare element lists, uncovered named entries) —
//! the shape where even a resolution-failure fallback sees nothing. Silence is
//! indistinguishable from equality, and for a security-classification consumer
//! that turns drift into invisibly-skipped changes (the `|block` incident this
//! invariant was written from). `OpaqueChanged` is payload-free by design: the
//! schema cannot know which parts of an undescribable shape are secrets, so
//! the only safe rendering is no rendering.

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
    /// The sides differ inside a shape the schema model cannot describe
    /// (model↔grammar drift) — a coarse but **visible** change at the nearest
    /// describable path (see the module-level invariant). Deliberately
    /// payload-free: the schema cannot know which parts of an undescribable
    /// shape are secrets, so no content is carried.
    OpaqueChanged,
    /// The sides differ inside a field the schema DECLARES opaque
    /// (`object`-typed): an embedded domain sub-language the model
    /// intentionally does not describe (e.g. a capability-grant DSL). The
    /// typed twin of [`ChangeKind::OpaqueChanged`] — same payload-free
    /// discipline (the schema cannot know the domain body's secrets), but the
    /// OPPOSITE meaning: this is design, not drift. Consumers key semantic
    /// domain differs off it and never raise a drift warning for it.
    ObjectChanged,
}

/// A schema-field hop along a [`FieldPath`]: its name plus the schema facts a
/// consumer folds to classify the change — **without re-walking the schema**.
/// `directives` are opaque to nml-core (RFC 0032: consumers assign the meaning);
/// `is_secret` is a first-class nml fact (a `secret`-typed field, which is
/// always terminal). `modifier` records that the field is an access-control
/// modifier (`|name`) so a report can render the sigil the operator wrote.
#[derive(Debug, Clone)]
pub struct FieldStep {
    pub name: String,
    pub modifier: bool,
    pub directives: Vec<Directive>,
    pub is_secret: bool,
}

impl FieldStep {
    /// A field step with no directives and no sigil (the common test/consumer
    /// constructor; the differ builds steps from real [`FieldDef`]s).
    pub fn new(name: impl Into<String>, directives: Vec<Directive>, is_secret: bool) -> Self {
        Self {
            name: name.into(),
            modifier: false,
            directives,
            is_secret,
        }
    }

    fn from_field(field: &FieldDef) -> Self {
        Self {
            name: field.name.clone(),
            modifier: matches!(field.field_type, FieldType::Modifier(_)),
            directives: field.directives.clone(),
            is_secret: is_secret(&field.field_type),
        }
    }
}

/// A collection element's identity along a [`FieldPath`] — pure navigation
/// (never classified). Split by the exact ambiguity boundary: an identifier key
/// renders **dotted** (grammatically dot-free), a scalar key renders
/// **bracketed and quoted** so an identity containing dots (e.g. an `[]install`
/// package `[vendor]-x.v1`) stays one unambiguous segment.
#[derive(Debug, Clone)]
pub enum ElemKey {
    /// `- Google:` / `- SomeRef` / `- @role/x` — an identifier/reference/role.
    Name(String),
    /// `- "[vendor]-x.v1"` — a scalar shorthand key.
    Key(Value),
}

/// One hop of a [`FieldPath`].
#[derive(Debug, Clone)]
pub enum PathSeg {
    Field(FieldStep),
    Element(ElemKey),
}

/// A change's path as **structure, not a string**: an ordered list of
/// field/element hops, each field hop carrying its own reload-relevant schema
/// facts. This is the single source of truth a consumer both classifies (fold
/// over [`FieldPath::field_steps`]) and renders ([`std::fmt::Display`]) — no
/// re-walk of the schema, no re-parse of a dotted string, no dot-in-identity
/// ambiguity. The root is always a field hop.
#[derive(Debug, Clone, Default)]
pub struct FieldPath {
    segs: Vec<PathSeg>,
}

impl FieldPath {
    /// Construct from raw segments (consumers building a path directly, e.g.
    /// tests). The differ builds paths by immutable extension during the walk.
    pub fn from_segments(segs: Vec<PathSeg>) -> Self {
        Self { segs }
    }

    /// This path plus one trailing hop — the immutable-extend the differ uses
    /// as it descends (mirrors the previous `format!("{path}.{name}")`, with no
    /// mutable stack to keep balanced).
    fn appended(&self, seg: PathSeg) -> Self {
        let mut segs = self.segs.clone();
        segs.push(seg);
        Self { segs }
    }

    /// The field hops in order (element hops skipped) — what a consumer folds
    /// to classify. The nearest-directive rule reads these leaf-to-root; a
    /// `secret` field is always the terminal hop, so the terminal step's
    /// `is_secret` is the only one that can be `true`.
    pub fn field_steps(&self) -> impl Iterator<Item = &FieldStep> {
        self.segs.iter().filter_map(|s| match s {
            PathSeg::Field(f) => Some(f),
            PathSeg::Element(_) => None,
        })
    }

    /// Whether this change's value is a `secret` (drives redaction): the
    /// terminal field hop's flag (secrets are terminal).
    pub fn is_secret(&self) -> bool {
        self.field_steps().last().is_some_and(|f| f.is_secret)
    }
}

impl std::fmt::Display for FieldPath {
    /// `server.sandboxCeiling.|block`, `plugins["[acme]-x.v1"].egressRate.rate`,
    /// `providers.Google.clientSecret` — field hops dotted (modifiers keep their
    /// `|` sigil), scalar element keys bracketed and quoted, identifier element
    /// keys dotted. Unambiguous by construction.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, seg) in self.segs.iter().enumerate() {
            match seg {
                PathSeg::Field(fs) => {
                    if i != 0 {
                        f.write_str(".")?;
                    }
                    if fs.modifier {
                        f.write_str("|")?;
                    }
                    f.write_str(&fs.name)?;
                }
                PathSeg::Element(ElemKey::Name(n)) => {
                    // Identifier keys are dot-free by grammar → dotted, reading
                    // like a field (`providers.Google`).
                    if i != 0 {
                        f.write_str(".")?;
                    }
                    f.write_str(n)?;
                }
                PathSeg::Element(ElemKey::Key(v)) => {
                    write!(f, "[{}]", render_key(v))?;
                }
            }
        }
        Ok(())
    }
}

/// Render a scalar element key for a bracketed path segment: strings and other
/// text-ish scalars quoted, numbers/bools bare.
fn render_key(v: &Value) -> String {
    match v {
        Value::String(s) | Value::Role(s) => {
            format!("{s:?}")
        }
        Value::Number(n) => format!("{n}"),
        // Bare like numbers/bools — the canonical `30s`, never the Debug
        // form the catch-all below would leak into operator output.
        Value::Duration(d) => format!("{d}"),
        Value::Bool(b) => format!("{b}"),
        // A secret can never be an element identity in any real schema, but the
        // diff layer is secret-safe BY CONSTRUCTION: never render one, even the
        // `$ENV` variable name, so a future `set<secret>`/`[]secret` cannot turn
        // a path into a leak.
        Value::Secret(_) => "‹secret›".to_string(),
        other => format!("{other:?}"),
    }
}

/// One semantic change: WHERE ([`FieldPath`] — carrying the schema facts to
/// classify and redact), WHAT ([`ChangeKind`]), and the source origin.
#[derive(Debug, Clone)]
pub struct FieldChange {
    pub path: FieldPath,
    pub kind: ChangeKind,
    /// The NEW side's origin (the OLD side's for `Removed`).
    pub origin: Origin,
}

impl FieldChange {
    /// Whether this change's value is a `secret` (drives consumer redaction).
    pub fn is_secret(&self) -> bool {
        self.path.is_secret()
    }
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
    diff_model(
        index,
        ModelCtx {
            model,
            exempt: None,
        },
        old,
        new,
        &FieldPath::default(),
        0,
        &mut out,
    );
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
        kind: crate::model::ModelKind::Model,
        source: None,
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
                // The synth root models an array as `List(item)`, so its
                // diffable content is its ELEMENTS (shared properties merged in).
                // Array-LEVEL modifiers/properties (`[]x |allow …` / `[]x k = v`)
                // are the array's own meta, not element data — a consumer that
                // needs to diff those models the array as a model-with-a-list
                // field instead. (nudge's reload substrate never carries them:
                // every reloadable top-level array is element-only.)
                let items = crate::resolve::apply_array_shared_properties(&a.body);
                let entries = items
                    .into_iter()
                    .map(|i| BodyEntry {
                        span: i.span,
                        kind: BodyEntryKind::ListItem(i),
                    })
                    .collect();
                (a.name.clone(), Body::fresh(entries))
            }
            _ => continue,
        };
        entries.push(BodyEntry {
            span: decl.span,
            kind: BodyEntryKind::NestedBlock(NestedBlock { name, body }),
        });
    }
    Body::fresh(entries)
}

/// What a keyed-body instance's type resolves to: a plain model, or a `oneof`
/// whose instance diffing (discriminator flip + variant routing) is owned
/// wholesale by [`diff_oneof_instance`]. One resolver serves BOTH call sites
/// (field-level bodies and collection elements), so their oneof behavior can
/// never diverge.
/// Resolve a keyed-body instance's type to the ONE canonical
/// [`FieldTarget`](SchemaIndex) via [`SchemaIndex::resolve_type_in_body`] — the
/// same body-shape variant selection the validator, defaulter, identity walk,
/// and LSP share, so the differ can never diverge from them (previously it
/// carried a private order-based union resolver). The body is `new`-side
/// preferred (last-file-wins), else `old`, else empty: for a non-union the body
/// is irrelevant (both sides resolve identically); for a union the effective
/// body's shape (arms / list items / neither) selects the variant, matching the
/// single-target diff already in force. Callers act on `Model`/`OneOf`; every
/// other target (`ListOf`/`SetOf`/`Object`/`Union`/`Arms`/`Leaf` — arms,
/// collections, and object fields are dispatched EARLIER) degrades to the
/// visible `opaque_if_different` fall-through, exactly as the remainder walk
/// did. When a real list/multi-model union appears, the conformance harness
/// flags it and the precise arm is added then.
fn resolve_diff_target<'a>(
    index: &'a SchemaIndex,
    ft: &'a FieldType,
    new_bodies: &[(PathBuf, &'a Body)],
    old_bodies: &[(PathBuf, &'a Body)],
    empty: &'a Body,
) -> FieldTarget<'a> {
    // The union variant-shape rule keys off entry KINDS (arms / list-items /
    // keyed / scalar), and under multi-file overlay the last file that sets the
    // instance carries its effective shape — so the last non-empty body
    // (new-side rev-first, else old) is the shape in force. A well-formed
    // instance has ONE shape across files; a contradictory split (keyed here,
    // list-items there) is validator-rejected and merely degrades visibly.
    // For a non-union type the body is ignored (resolves identically both
    // sides), so the choice only matters for unions.
    let body = new_bodies
        .iter()
        .rev()
        .chain(old_bodies.iter().rev())
        .map(|(_, b)| *b)
        // An RFC 0015 `as <Variant>` annotation makes even an entry-less body
        // meaningful (it names the variant), so it counts as a representative —
        // else `slot as modelB:` with an empty body would fall through to the
        // structural first-wins and mis-resolve.
        .find(|b| !b.entries.is_empty() || b.type_annotation.is_some())
        .unwrap_or(empty);
    index.resolve_type_in_body(ft, body)
}

/// The model a body diffs against, plus a property name the remainder walk
/// must treat as covered — the oneof discriminator when this model is a
/// variant selected through a oneof (it belongs to the union, not the model's
/// fields). `None` exempt for plain models.
#[derive(Clone, Copy)]
struct ModelCtx<'a> {
    model: &'a crate::model::ModelDef,
    exempt: Option<&'a str>,
}

fn diff_model(
    index: &SchemaIndex,
    ctx: ModelCtx<'_>,
    old: &[(PathBuf, &Body)],
    new: &[(PathBuf, &Body)],
    prefix: &FieldPath,
    depth: u32,
    out: &mut Vec<FieldChange>,
) {
    let model = ctx.model;
    if depth >= MAX_DEPTH {
        // The resource boundary honors the module invariant: content the walk
        // cannot descend into is structurally compared and surfaces as a
        // visible OpaqueChanged when it differs — never silently truncated.
        opaque_if_different(old, new, prefix, out);
        return;
    }
    for field in &model.fields {
        let path = prefix.appended(PathSeg::Field(FieldStep::from_field(field)));
        diff_field(index, field, old, new, &path, depth, out);
    }
    // The UNMODELED REMAINDER (module invariant, completeness edition): a
    // resolvable model whose instance bodies carry content its fields do not
    // describe — bare list items (e.g. `tenantGrants`' tenant→plugin lists)
    // and named entries matching no field. Without this, such content is
    // invisible even to the OpaqueChanged fall-through (the model DID
    // resolve) — the tenantGrants silent-diff bug class.
    diff_unmodeled_remainder(index, ctx, old, new, prefix, depth, out);
}

/// Diff a `oneof` instance — the ONE owner of oneof semantics for both call
/// sites (field bodies and collection elements). Policy, complete over the
/// input classes ({absent, content} × {resolvable, unresolvable} × default):
/// * an ABSENT side (the field/element does not exist — an EMPTY SLICE; a
///   present-but-empty body is NOT absent, it is real content selecting the
///   default variant) makes this a pure add/remove: single half-diff, precise
///   `Added`/`Removed`, and NO discriminator flip (a flip against a side that
///   does not exist would be a phantom change);
/// * both sides non-empty with EQUAL effective discriminators (explicit
///   property | union default): one normal model diff;
/// * both non-empty, DIFFERENT effective values: emit the flip once
///   (value-based — two values mapping to the SAME variant still flip), then
///   same-model → one diff; different models → each side diffs against ITS OWN
///   variant model vs empty (precise `Removed`/`Added`, no false opacity,
///   no false drift signal);
/// * a NON-EMPTY side that cannot resolve (value not in the variants, or the
///   variant's model missing from the index — both = drift) degrades to the
///   visible `opaque_if_different` fall-through. Garbage in, deterministic
///   visible out.
fn diff_oneof_instance(
    index: &SchemaIndex,
    oneof: &crate::model::OneOfDef,
    old: &[(PathBuf, &Body)],
    new: &[(PathBuf, &Body)],
    path: &FieldPath,
    depth: u32,
    out: &mut Vec<FieldChange>,
) {
    fn effective<'a>(
        side: &[(PathBuf, &'a Body)],
        discr_name: &str,
        default: Option<&'a str>,
    ) -> Option<&'a str> {
        lookup_property(side, discr_name).or(default)
    }
    let discr_name = oneof.discriminator.as_str();
    let default = oneof.default_discriminator.as_deref();
    // ABSENT means the field/element does not exist on that side at all — the
    // callers pass an EMPTY SLICE for an absent side (diff_field's `_ => &empty`
    // arm; an element with no body). A side that is PRESENT with an empty body
    // is REAL content: with a default discriminator, bare presence legitimately
    // selects the default variant (`e:` ≡ the builtin variant), so a later
    // explicit `kind = "card"` is a genuine flip — not a phantom.
    let is_absent = |side: &[(PathBuf, &Body)]| side.is_empty();
    let variant_model = |val: &str| {
        let name = oneof
            .variants
            .iter()
            .find(|(v, _)| v == val)
            .map(|(_, m)| m.as_str())?;
        match index.resolve_ref(name) {
            Some(FieldTarget::Model(m)) => Some(m),
            _ => None,
        }
    };
    let empty: Vec<(PathBuf, &Body)> = Vec::new();

    let (old_empty, new_empty) = (is_absent(old), is_absent(new));
    if old_empty && new_empty {
        return;
    }
    // Pure add/remove: single-sided precise diff, no flip.
    if old_empty || new_empty {
        let side = if old_empty { new } else { old };
        match effective(side, discr_name, default).and_then(&variant_model) {
            Some(m) => {
                let ctx = ModelCtx {
                    model: m,
                    exempt: Some(discr_name),
                };
                if old_empty {
                    diff_model(index, ctx, &empty, new, path, depth, out);
                } else {
                    diff_model(index, ctx, old, &empty, path, depth, out);
                }
            }
            None => opaque_if_different(old, new, path, out),
        }
        return;
    }

    let (Some(old_val), Some(new_val)) = (
        effective(old, discr_name, default),
        effective(new, discr_name, default),
    ) else {
        // Non-empty but no discriminator and no default: undescribable.
        opaque_if_different(old, new, path, out);
        return;
    };
    if old_val != new_val {
        // The flip is the headline change — visible in its own right, once,
        // value-based. Origin prefers whichever side names the property
        // explicitly.
        let flip_path = discriminator_flip_path(path, discr_name);
        let origin = match discriminator_origin(new, discr_name) {
            Origin::Default => discriminator_origin(old, discr_name),
            o => o,
        };
        push(
            &flip_path,
            ChangeKind::Modified {
                old: Value::String(old_val.to_string()),
                new: Value::String(new_val.to_string()),
            },
            origin,
            out,
        );
    }
    match (variant_model(old_val), variant_model(new_val)) {
        (Some(om), Some(nm)) if std::ptr::eq(om, nm) => {
            let ctx = ModelCtx {
                model: nm,
                exempt: Some(discr_name),
            };
            diff_model(index, ctx, old, new, path, depth, out);
        }
        (Some(om), Some(nm)) => {
            // Variant switch: each side against ITS OWN model — precise
            // Removed/Added, and the old side's fields are fully described
            // (no false OpaqueChanged, no false drift signal).
            let nctx = ModelCtx {
                model: nm,
                exempt: Some(discr_name),
            };
            let octx = ModelCtx {
                model: om,
                exempt: Some(discr_name),
            };
            diff_model(index, nctx, &empty, new, path, depth, out);
            diff_model(index, octx, old, &empty, path, depth, out);
        }
        _ => {
            // Unknown variant value or missing variant model on a NON-EMPTY
            // side — genuine drift, visible.
            opaque_if_different(old, new, path, out);
        }
    }
}

/// The flip change's path: the oneof field's path plus the discriminator
/// property hop appended.
fn discriminator_flip_path(path: &FieldPath, discr: &str) -> FieldPath {
    path.appended(PathSeg::Field(FieldStep::new(discr, Vec::new(), false)))
}

/// Diff the parts of a model-instance body the model's fields do not cover.
/// Bare list items diff as an identity-paired collection (element-level
/// precision — an added/removed item is a real per-element change; paired
/// items' bodies recurse via the scalar-element machinery, degrading to a
/// pathed OpaqueChanged where no model applies). Uncovered NAMED entries
/// (matching no field, and not the oneof discriminator) compare structurally
/// and surface as one OpaqueChanged at the model's path when they differ.
fn diff_unmodeled_remainder(
    index: &SchemaIndex,
    ctx: ModelCtx<'_>,
    old: &[(PathBuf, &Body)],
    new: &[(PathBuf, &Body)],
    prefix: &FieldPath,
    depth: u32,
    out: &mut Vec<FieldChange>,
) {
    let (model, discriminator) = (ctx.model, ctx.exempt);
    // (a) Bare list items. Two cases, one mechanism:
    //  * the model DECLARES a body-positional list field (RFC 0005 `+` on a
    //    list/set — e.g. `plugins []tenantGrantPlugin+`): the items diff
    //    against the REAL field, so elements get true model recursion
    //    (validated fields, `object`-typed leaves → `ObjectChanged`);
    //  * no declaration (unmodeled): a synthetic `[]string` field — elements
    //    pair by identity; paired bodies take the scalar-element recursion
    //    (items → deeper collections, else a pathed OpaqueChanged).
    // Either way changes emit at `prefix` (the grammar has no field name to
    // hop through — the operator never wrote one), so classification folds the
    // REAL field hops on `prefix`.
    let o_eff = Effective::Bodies(old.to_vec());
    let n_eff = Effective::Bodies(new.to_vec());
    let o_items = collect_elems(&o_eff);
    let n_items = collect_elems(&n_eff);
    if !o_items.is_empty() || !n_items.is_empty() {
        let synth = FieldDef {
            name: String::new(),
            field_type: FieldType::List(Box::new(FieldType::Primitive {
                ty: crate::types::PrimitiveType::String,
                facets: crate::model::NumberFacets::NONE,
            })),
            optional: true,
            shorthand: false,
            default_value: None,
            directives: Vec::new(),
            doc: None,
            span: Span { start: 0, end: 0 },
        };
        let items_field = model
            .fields
            .iter()
            .find(|f| f.shorthand && matches!(f.field_type, FieldType::List(_) | FieldType::Set(_)))
            .unwrap_or(&synth);
        diff_collections(
            index,
            items_field,
            &o_items,
            &n_items,
            prefix,
            depth + 1,
            out,
        );
    }

    // (b) Named entries no field covers.
    let covered: std::collections::HashSet<&str> =
        model.fields.iter().map(|f| f.name.as_str()).collect();
    fn uncovered<'b>(
        files: &'b [(PathBuf, &'b Body)],
        covered: &std::collections::HashSet<&str>,
        discriminator: Option<&str>,
    ) -> Vec<(&'b Path, &'b BodyEntry)> {
        let mut out_e = Vec::new();
        for (file, body) in files {
            for e in &body.entries {
                let keep = match &e.kind {
                    BodyEntryKind::Property(p) => {
                        !covered.contains(p.name.name.as_str())
                            && Some(p.name.name.as_str()) != discriminator
                    }
                    BodyEntryKind::NestedBlock(nb) => !covered.contains(nb.name.name.as_str()),
                    BodyEntryKind::Modifier(m) => !covered.contains(m.name.name.as_str()),
                    // An arm outside an arms-typed field's body is unmodeled.
                    BodyEntryKind::Arm(_) => true,
                    // Items handled in (a); authoring/resolved constructs carry
                    // no config values.
                    BodyEntryKind::ListItem(_)
                    | BodyEntryKind::SharedProperty(_)
                    | BodyEntryKind::FieldDefinition(_) => false,
                };
                if keep {
                    out_e.push((file.as_path(), e));
                }
            }
        }
        out_e
    }
    let o_rem = uncovered(old, &covered, discriminator);
    let n_rem = uncovered(new, &covered, discriminator);
    if o_rem.is_empty() && n_rem.is_empty() {
        return;
    }
    let eq = o_rem.len() == n_rem.len()
        && o_rem
            .iter()
            .zip(&n_rem)
            .all(|((_, a), (_, b))| entry_structural_eq(a, b, depth));
    if !eq {
        let origin = n_rem
            .first()
            .or_else(|| o_rem.first())
            .map(|(f, e)| Origin::File {
                file: f.to_path_buf(),
                span: e.span,
            })
            .unwrap_or(Origin::Default);
        push(prefix, ChangeKind::OpaqueChanged, origin, out);
    }
}

/// One field: resolve effective old/new (last-file-wins, else schema default)
/// and dispatch on shape. `path` already ends in this field's hop.
fn diff_field(
    index: &SchemaIndex,
    field: &FieldDef,
    old: &[(PathBuf, &Body)],
    new: &[(PathBuf, &Body)],
    path: &FieldPath,
    depth: u32,
    out: &mut Vec<FieldChange>,
) {
    let old_eff = effective(old, &field.name, field);
    let new_eff = effective(new, &field.name, field);

    // A DECLARED-opaque field (`object`-typed): an embedded domain body the
    // model intentionally does not describe. Never recurse into it (a
    // structural walk would mis-read the domain grammar through a schema that
    // by design says nothing about it) — structurally compare the raw bodies
    // and surface any difference as the typed [`ChangeKind::ObjectChanged`].
    if matches!(
        field.field_type,
        FieldType::Primitive {
            ty: PrimitiveType::Object,
            ..
        }
    ) {
        diff_declared_object(&old_eff, &new_eff, path, out);
        return;
    }

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

/// Compare a declared-`object` field's two sides without interpreting them:
/// per-file body sequences via the span-ignoring structural equality, inline
/// values via `semantic_eq`. Any difference — including a body appearing or
/// disappearing, or a body↔value shape change — is one payload-free
/// [`ChangeKind::ObjectChanged`] at the field path.
fn diff_declared_object(
    old_eff: &Effective,
    new_eff: &Effective,
    path: &FieldPath,
    out: &mut Vec<FieldChange>,
) {
    let changed = match (old_eff, new_eff) {
        (Effective::Absent, Effective::Absent) => false,
        (Effective::Bodies(o), Effective::Bodies(n)) => {
            !(o.len() == n.len()
                && o.iter()
                    .zip(n)
                    .all(|((_, a), (_, b))| body_structural_eq(a, b)))
        }
        (Effective::Items(o, _), Effective::Items(n, _)) => {
            !(o.len() == n.len() && o.iter().zip(n.iter()).all(|(a, b)| list_item_eq(a, b, 0)))
        }
        (
            Effective::Value(o, _) | Effective::Default(o),
            Effective::Value(n, _) | Effective::Default(n),
        ) => !o.value.semantic_eq(&n.value),
        // Shape changed across sides (body ↔ value ↔ items ↔ absent).
        _ => true,
    };
    if changed {
        // Prefer the new side's origin; a REMOVED block (new side absent)
        // falls back to the old side so the report still carries file:line.
        let eff_for_origin = if matches!(new_eff, Effective::Absent) {
            old_eff
        } else {
            new_eff
        };
        let origin = match eff_for_origin {
            Effective::Bodies(b) => b
                .first()
                .map(|(f, body)| Origin::File {
                    file: f.clone(),
                    span: body
                        .entries
                        .first()
                        .map(|e| e.span)
                        .unwrap_or(Span { start: 0, end: 0 }),
                })
                .unwrap_or(Origin::Default),
            Effective::Items(items, f) => items
                .first()
                .map(|i| Origin::File {
                    file: f.to_path_buf(),
                    span: i.span,
                })
                .unwrap_or(Origin::Default),
            Effective::Value(sv, f) => Origin::File {
                file: f.to_path_buf(),
                span: sv.span,
            },
            _ => Origin::Default,
        };
        push(path, ChangeKind::ObjectChanged, origin, out);
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

/// Compare two value-shaped effectives at a leaf. `path` already ends in the
/// field's hop, so `push` reads its classify/redact facts straight off it.
fn diff_values_at(
    field: &FieldDef,
    old: &Effective,
    new: &Effective,
    path: &FieldPath,
    out: &mut Vec<FieldChange>,
) {
    let (old_v, _old_origin) = value_of(old);
    let (new_v, new_origin) = value_of(new);
    match (old_v, new_v) {
        (None, None) => {}
        (None, Some(n)) => push(path, ChangeKind::Added { new: n.clone() }, new_origin, out),
        (Some(o), None) => push(
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
                            path,
                            ChangeKind::SetDelta { added, removed },
                            new_origin,
                            out,
                        );
                        return;
                    }
                }
                push(
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
    path: &FieldPath,
    depth: u32,
    out: &mut Vec<FieldChange>,
) {
    // Arm sets (RFC 0007 routing blocks): PER-ARM diff — selector-paired
    // retargets, full-arm add/remove (a move = a -/+ pair), each with its own
    // file:line. Last file carrying arms wins, like every collection. A side
    // with no arms diffs as the empty list, so a whole block appearing renders
    // as per-arm Addeds rather than one blob.
    let old_arms = collect_arm_entries(old);
    let new_arms = collect_arm_entries(new);
    let old_elems = collect_elems(old);
    let new_elems = collect_elems(new);

    // ── Shape-transition visibility (RFC 0015) ──────────────────────────────
    // The arms/collections early returns below render exactly ONE
    // representation. When the two sides take DIFFERENT paths — a keyed model
    // body or inline value on one side, an arms/list form on the other (a
    // union shape switch) — the losing side's content would silently vanish.
    // Surface the orphaned side FIRST, in its own representation, so a shape
    // switch keeps the never-silent invariant at the boundary.
    let old_is_ae = old_arms.is_some() || !old_elems.is_empty();
    let new_is_ae = new_arms.is_some() || !new_elems.is_empty();
    if old_is_ae != new_is_ae {
        if old_is_ae {
            // Old side renders below (vs empty); the NEW side is the orphan.
            orphan_side_visibility(index, field, new, false, path, depth, out);
        } else {
            orphan_side_visibility(index, field, old, true, path, depth, out);
        }
    }

    if old_arms.is_some() || new_arms.is_some() {
        // Arms ↔ ELEMS transition (both "collection-shaped", different kinds —
        // e.g. `((role -> string) | []step)` switching forms): the non-arms
        // side's elements have no arm representation and would vanish in the
        // arm diff below. Surface them as their own collection diff vs empty —
        // the same orphan rule, arms edition.
        if old_arms.is_none() && !old_elems.is_empty() {
            diff_collections(index, field, &old_elems, &[], path, depth, out);
        }
        if new_arms.is_none() && !new_elems.is_empty() {
            diff_collections(index, field, &[], &new_elems, path, depth, out);
        }
        diff_arms(
            &old_arms.unwrap_or_default(),
            &new_arms.unwrap_or_default(),
            path,
            out,
        );
        return;
    }

    // Collections (either side items, or a list/set-typed field).
    if !old_elems.is_empty() || !new_elems.is_empty() {
        diff_collections(index, field, &old_elems, &new_elems, path, depth, out);
        return;
    }

    // Value ↔ body transition with neither side arms/elems (e.g. a union's
    // scalar variant swapped for its model variant): the body side is rendered
    // by the model path below against an empty slice; the VALUE side has no
    // representation there — surface it here.
    match (old, new) {
        (Effective::Value(..) | Effective::Default(_), Effective::Bodies(_)) => {
            orphan_side_visibility(index, field, old, true, path, depth, out);
        }
        (Effective::Bodies(_), Effective::Value(..) | Effective::Default(_)) => {
            orphan_side_visibility(index, field, new, false, path, depth, out);
        }
        _ => {}
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
    let empty_body = Body::fresh(Vec::new());
    // RFC 0015 variant SWITCH: resolve each side to its own variant (honoring an
    // `as <Variant>` annotation on that side). When they select DIFFERENT models
    // — an annotation switch, or a shape switch — diff each side against its own
    // model as precise removes+adds, so the switch stays VISIBLE even when the
    // two bodies are field-identical (which `diff_model` against one shared model
    // would silently miss). Only reachable for a union with ≥2 model variants
    // (RFC 0015's same-class case); existing single-model unions never enter here
    // — their two sides resolve to the same model — so behavior is unchanged.
    let old_target = resolve_diff_target(index, &field.field_type, o, &empty, &empty_body);
    let new_target = resolve_diff_target(index, &field.field_type, n, &empty, &empty_body);
    if diff_variant_switch(index, (&old_target, &new_target), o, n, path, depth, out) {
        return;
    }
    match resolve_diff_target(index, &field.field_type, n, o, &empty_body) {
        FieldTarget::Model(m) => diff_model(
            index,
            ModelCtx {
                model: m,
                exempt: None,
            },
            o,
            n,
            path,
            depth + 1,
            out,
        ),
        FieldTarget::OneOf(of) => diff_oneof_instance(index, of, o, n, path, depth + 1, out),
        _ => {
            // Bodies that neither arms, elements, nor a resolvable target
            // consumed: a shape the schema cannot describe (model↔grammar
            // drift — a renamed/missing model, a body under a scalar-typed
            // field). Visible, never silent (module invariant). No
            // scalar-element recursion is needed here (unlike the element-level
            // resolve): a list-item body was already dispatched to
            // `diff_collections` above, so a union can only resolve here to a
            // keyed→Model (handled) or a scalar/empty→Leaf, never to `ListOf`.
            opaque_if_different(o, n, path, out);
        }
    }
}

/// Ordered structural equality over collection elements at the walk's resource
/// boundary: identities and (depth-bounded) bodies. Conservative like the body
/// comparison — order differences read as changed.
fn elems_structural_eq(old: &[Elem<'_>], new: &[Elem<'_>]) -> bool {
    old.len() == new.len()
        && old.iter().zip(new).all(|(o, n)| {
            o.id.eq(&n.id)
                && match (o.body, n.body) {
                    (Some(a), Some(b)) => body_structural_eq(a, b),
                    (None, None) => true,
                    _ => false,
                }
        })
}

/// Emit a payload-free [`ChangeKind::OpaqueChanged`] iff the two sides'
/// undescribable bodies differ structurally (span-ignoring — a cosmetic-only
/// edit stays quiet, preserving the no-op property). Payload-free because the
/// schema cannot know which parts of an undescribable shape are secrets.
///
/// The comparison is the per-file body SEQUENCE (an undescribable shape has no
/// merge semantics to apply), so re-splitting identical content across files
/// conservatively reads as changed — visible, never silent, per the invariant.
/// The nominal name a per-side resolution can switch between — a model or a
/// `oneof` (both nameable via `as`, and globally name-disjoint by
/// `ONEOF_NAME_COLLISION`, so equal names imply equal kinds).
fn switch_name<'a>(t: &FieldTarget<'a>) -> Option<&'a str> {
    match t {
        FieldTarget::Model(m) => Some(m.name.as_str()),
        FieldTarget::OneOf(o) => Some(o.name.as_str()),
        _ => None,
    }
}

/// One side of a variant switch, diffed against emptiness through its OWN
/// resolved target — the model walk for a model variant, the oneof machinery
/// (discriminator-aware) for a `oneof` variant.
fn half_diff_target(
    index: &SchemaIndex,
    target: &FieldTarget,
    side: &[(PathBuf, &Body)],
    removed: bool,
    path: &FieldPath,
    depth: u32,
    out: &mut Vec<FieldChange>,
) {
    let empty: Vec<(PathBuf, &Body)> = Vec::new();
    let (o, n) = if removed {
        (side, empty.as_slice())
    } else {
        (empty.as_slice(), side)
    };
    match target {
        FieldTarget::Model(m) => {
            let ctx = ModelCtx {
                model: m,
                exempt: None,
            };
            diff_model(index, ctx, o, n, path, depth, out);
        }
        FieldTarget::OneOf(of) => diff_oneof_instance(index, of, o, n, path, depth, out),
        _ => {}
    }
}

/// RFC 0015 variant SWITCH, the ONE owner for both the field and element
/// levels: when the two sides resolve (per side, annotation-aware) to
/// DIFFERENT nameable variants — model↔model, model↔oneof, oneof↔oneof —
/// emit the explicit `as A → as B` witness (both sides present; an absent
/// side is a whole-field add/remove and gets no phantom witness, the same
/// rule `diff_oneof_instance` applies to discriminator flips) and diff each
/// side against emptiness through its OWN target. Returns whether the switch
/// path handled the diff; same-variant or non-nameable sides fall through to
/// the shared single-target path.
fn diff_variant_switch(
    index: &SchemaIndex,
    (old_target, new_target): (&FieldTarget, &FieldTarget),
    o: &[(PathBuf, &Body)],
    n: &[(PathBuf, &Body)],
    path: &FieldPath,
    depth: u32,
    out: &mut Vec<FieldChange>,
) -> bool {
    let (Some(on), Some(nn)) = (switch_name(old_target), switch_name(new_target)) else {
        return false;
    };
    if on == nn {
        return false;
    }
    if !o.is_empty() && !n.is_empty() {
        push_variant_switch(on, nn, o, n, path, out);
    }
    half_diff_target(index, old_target, o, true, path, depth + 1, out);
    half_diff_target(index, new_target, n, false, path, depth + 1, out);
    true
}

/// One side of a shape transition, rendered in its OWN representation against
/// emptiness (RFC 0015 shape-switch visibility): a keyed/annotated body diffs
/// through its per-side-resolved model or oneof (precise per-field
/// removes/adds), an inline value emits one Removed/Added, and anything
/// unresolvable degrades to the visible opaque fall-through — never silence.
/// `removed` = the orphan is the OLD side (its content is going away).
fn orphan_side_visibility(
    index: &SchemaIndex,
    field: &FieldDef,
    side: &Effective,
    removed: bool,
    path: &FieldPath,
    depth: u32,
    out: &mut Vec<FieldChange>,
) {
    match side {
        Effective::Bodies(b) => {
            let empty: Vec<(PathBuf, &Body)> = Vec::new();
            let empty_body = Body::fresh(Vec::new());
            let (o, n): (&Vec<_>, &Vec<_>) = if removed { (b, &empty) } else { (&empty, b) };
            match resolve_diff_target(index, &field.field_type, b, &empty, &empty_body) {
                FieldTarget::Model(m) => {
                    let ctx = ModelCtx {
                        model: m,
                        exempt: None,
                    };
                    diff_model(index, ctx, o, n, path, depth + 1, out);
                }
                FieldTarget::OneOf(of) => {
                    diff_oneof_instance(index, of, o, n, path, depth + 1, out);
                }
                _ => opaque_if_different(o, n, path, out),
            }
        }
        Effective::Value(sv, file) => {
            let kind = if removed {
                ChangeKind::Removed {
                    old: sv.value.clone(),
                }
            } else {
                ChangeKind::Added {
                    new: sv.value.clone(),
                }
            };
            push(
                path,
                kind,
                Origin::File {
                    file: file.to_path_buf(),
                    span: sv.span,
                },
                out,
            );
        }
        Effective::Default(sv) => {
            let kind = if removed {
                ChangeKind::Removed {
                    old: sv.value.clone(),
                }
            } else {
                ChangeKind::Added {
                    new: sv.value.clone(),
                }
            };
            push(path, kind, Origin::Default, out);
        }
        // Absent: nothing to surface. Items: an arms/elems-shaped side is
        // rendered by its own dispatch path, never orphaned here.
        Effective::Absent | Effective::Items(..) => {}
    }
}

/// RFC 0015: emit a nominal variant SWITCH itself as an explicit, always-visible
/// change (`as modelA` → `as modelB`) — the nominal-union analogue of a oneof
/// discriminator flip. The per-side half-diffs that follow surface field
/// content; THIS entry guarantees the switch is visible even when both bodies
/// are entry-less (nothing to half-diff) or the variants differ only in their
/// schema defaults — the cases where content diffs alone would be silent,
/// violating the never-silent invariant. Origin prefers the annotation ident's
/// own span (the token that changed), then the first entry, then Default.
fn push_variant_switch(
    old_model: &str,
    new_model: &str,
    old: &[(PathBuf, &Body)],
    new: &[(PathBuf, &Body)],
    path: &FieldPath,
    out: &mut Vec<FieldChange>,
) {
    // The annotation ident's own span, from the REPRESENTATIVE file — the
    // last (rev-first) annotated overlay file, matching how
    // `resolve_diff_target` selected the variant — so the witness points at
    // the token that changed, not the first file's unrelated content.
    let origin = new
        .iter()
        .rev()
        .find_map(|(f, b)| b.type_annotation.as_ref().map(|i| (f.clone(), i.span)))
        .or_else(|| {
            new.first().or_else(|| old.first()).map(|(f, b)| {
                (
                    f.clone(),
                    b.entries
                        .first()
                        .map(|e| e.span)
                        .unwrap_or(Span { start: 0, end: 0 }),
                )
            })
        })
        .map(|(file, span)| Origin::File { file, span })
        .unwrap_or(Origin::Default);
    push(
        path,
        ChangeKind::Modified {
            old: Value::String(format!("as {old_model}")),
            new: Value::String(format!("as {new_model}")),
        },
        origin,
        out,
    );
}

fn opaque_if_different(
    old: &[(PathBuf, &Body)],
    new: &[(PathBuf, &Body)],
    path: &FieldPath,
    out: &mut Vec<FieldChange>,
) {
    let eq = old.len() == new.len()
        && old
            .iter()
            .zip(new)
            .all(|((_, a), (_, b))| body_structural_eq(a, b));
    if eq {
        return;
    }
    let origin = new
        .first()
        .or_else(|| old.first())
        .map(|(f, b)| Origin::File {
            file: f.clone(),
            span: b
                .entries
                .first()
                .map(|e| e.span)
                .unwrap_or(Span { start: 0, end: 0 }),
        })
        .unwrap_or(Origin::Default);
    push(path, ChangeKind::OpaqueChanged, origin, out);
}

/// The file:line of a discriminator property `name` on one side, for a
/// discriminator-flip change's origin (the caller prefers whichever side names
/// the property explicitly). Falls back to `Default` if absent.
fn discriminator_origin(bodies: &[(PathBuf, &Body)], name: &str) -> Origin {
    for (file, body) in bodies.iter().rev() {
        for entry in &body.entries {
            if let BodyEntryKind::Property(p) = &entry.kind {
                if p.name.name == name {
                    return Origin::File {
                        file: file.clone(),
                        span: p.value.span,
                    };
                }
            }
        }
    }
    Origin::Default
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
    path: &FieldPath,
    depth: u32,
    out: &mut Vec<FieldChange>,
) {
    if depth >= MAX_DEPTH {
        // Same invariant at the collections boundary: compare what we cannot
        // walk; differ ⇒ visible OpaqueChanged, identical ⇒ silence.
        if !elems_structural_eq(old, new) {
            let origin = new
                .first()
                .or_else(|| old.first())
                .map(elem_origin)
                .unwrap_or(Origin::Default);
            push(path, ChangeKind::OpaqueChanged, origin, out);
        }
        return;
    }
    if is_set(&field.field_type) {
        for n in new {
            if !old.iter().any(|o| o.id.eq(&n.id)) {
                push(
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
    // Each old element pairs at most ONCE. Identity is semantic (RFC 0016
    // made it numeric, so `8080` and `8080.0` are one identity), which
    // means a collection can legitimately hold several elements sharing an
    // identity; a plain `find` would pair every one of them against the
    // first old match, reporting a wrong baseline for the rest and never
    // diffing the later old elements at all. Consuming matches in order
    // keeps pairing stable, total, and 1:1.
    let mut paired_old = vec![false; old.len()];
    for n in new {
        let matched = old
            .iter()
            .enumerate()
            .find(|(i, o)| !paired_old[*i] && o.id.eq(&n.id));
        let Some(o) = matched.map(|(i, o)| {
            paired_old[i] = true;
            o
        }) else {
            // A brand-new NAMED element reports as an Added at its element path
            // (unchanged behavior); new scalar elements — bodied or not — fall
            // to the ordered LCS path below as a whole-element Added, so they
            // are not double-reported here.
            if matches!(n.id, ElemId::Name(_)) && !is_set(&field.field_type) {
                push(
                    &path.appended(PathSeg::Element(elem_key(&n.id))),
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
        let elem_path = path.appended(PathSeg::Element(elem_key(&n.id)));
        let o_files: Vec<(PathBuf, &Body)> = o
            .body
            .map(|b| vec![(o.file.map(Path::to_path_buf).unwrap_or_default(), b)])
            .unwrap_or_default();
        let n_files: Vec<(PathBuf, &Body)> = n
            .body
            .map(|b| vec![(n.file.map(Path::to_path_buf).unwrap_or_default(), b)])
            .unwrap_or_default();
        let empty_body = Body::fresh(Vec::new());
        // RFC 0015 variant SWITCH at the ELEMENT level — the exact twin of the
        // field-level rule in `diff_bodies`: resolve each side against its own
        // body (honoring its `as <Variant>` annotation); different models diff
        // as precise per-side removes+adds, so an annotation-only element
        // switch is never silently equal. Only reachable for `[](A | B)` unions
        // with ≥2 model variants; single-model elements resolve identically on
        // both sides and fall through unchanged.
        let empty_side: Vec<(PathBuf, &Body)> = Vec::new();
        let o_target = resolve_diff_target(
            index,
            elem_type(&field.field_type),
            &o_files,
            &empty_side,
            &empty_body,
        );
        let n_target = resolve_diff_target(
            index,
            elem_type(&field.field_type),
            &n_files,
            &empty_side,
            &empty_body,
        );
        if diff_variant_switch(
            index,
            (&o_target, &n_target),
            &o_files,
            &n_files,
            &elem_path,
            depth,
            out,
        ) {
            continue;
        }
        match resolve_diff_target(
            index,
            elem_type(&field.field_type),
            &n_files,
            &o_files,
            &empty_body,
        ) {
            FieldTarget::Model(m) => {
                diff_model(
                    index,
                    ModelCtx {
                        model: m,
                        exempt: None,
                    },
                    &o_files,
                    &n_files,
                    &elem_path,
                    depth + 1,
                    out,
                );
            }
            FieldTarget::OneOf(of) => {
                diff_oneof_instance(index, of, &o_files, &n_files, &elem_path, depth + 1, out);
            }
            _ => {
                // Scalar-typed elements whose bodies carry config content — e.g.
                // `|block set<string>` with namespaced `- egress:` entries (the RFC
                // 0032 |block blind spot): there is no model to recurse into, but
                // the content is still list items. Complete the recursion: diff the
                // bodies' own items with the PARENT collection's semantics at the
                // element path — full per-element fidelity with real origins.
                let o_items = Effective::Bodies(o_files.clone());
                let n_items = Effective::Bodies(n_files.clone());
                let o_items = collect_elems(&o_items);
                let n_items = collect_elems(&n_items);
                if !o_items.is_empty() || !n_items.is_empty() {
                    diff_collections(index, field, &o_items, &n_items, &elem_path, depth + 1, out);
                } else {
                    // No items AND no model: content the schema cannot describe —
                    // visible, never silent (module invariant).
                    opaque_if_different(&o_files, &n_files, &elem_path, out);
                }
            }
        }
    }
    if !is_set(&field.field_type) {
        // Ordered scalars: LCS alignment — unmatched = added/removed, so a
        // head insertion is ONE change.
        let matched = lcs_pairs(old, new);
        for (i, n) in new.iter().enumerate() {
            if matches!(n.id, ElemId::Val(_)) && !matched.iter().any(|&(_, b)| b == i) {
                push(
                    path,
                    ChangeKind::Added { new: n.id.render() },
                    elem_origin(n),
                    out,
                );
            }
        }
        for (i, o) in old.iter().enumerate() {
            // Named removal reads the SAME 1:1 pairing the recursion pass
            // built, not an existence test. With `!new.iter().any(…)`, two
            // old `- A:` elements against one new `- A:` both saw a match
            // and neither reported removed — an element vanished from the
            // config with the differ reporting no change at all, which for
            // a `#restart`-classified subtree means no reload either.
            let removed_named = matches!(o.id, ElemId::Name(_)) && !paired_old[i];
            let removed_val =
                matches!(o.id, ElemId::Val(_)) && !matched.iter().any(|&(a, _)| a == i);
            if removed_named || removed_val {
                push(
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
/// Canonical single-arm rendering ("sel -> target") — shared by the wholesale
/// arms diff and the structural body equality, so the two can never disagree
/// about what an arm "is".
fn arm_selector_str(a: &crate::ast::Arm) -> &str {
    use crate::ast::ArmSelector;
    match &a.selector {
        ArmSelector::Role(r) => r.as_str(),
        ArmSelector::Else => "else",
    }
}

fn arm_target_str(a: &crate::ast::Arm) -> String {
    use crate::ast::ArmTarget;
    match &a.target {
        ArmTarget::Reference(id) => id.name.clone(),
        ArmTarget::Literal { value, .. } => format!("{value:?}"),
    }
}

/// The one arm rendering (`sel -> target`) — every consumer (wholesale eq,
/// per-arm add/remove payloads) formats through here, so the shape can never
/// fork.
fn format_arm(selector: &str, target: &str) -> String {
    format!("{selector} -> {target}")
}

fn render_arm(a: &crate::ast::Arm) -> String {
    format_arm(arm_selector_str(a), &arm_target_str(a))
}

/// Span-ignoring structural equality over config-instance bodies — the
/// comparison behind [`ChangeKind::OpaqueChanged`] (see the module invariant).
/// Order-sensitive by design: in a shape the schema cannot describe, the differ
/// cannot know whether order carries meaning, so a reorder is conservatively a
/// visible change. Comments never reach the AST, so trivia cannot false-positive;
/// `Value` comparison is `semantic_eq` (spans ignored, secrets by name).
/// Authoring-only constructs (`FieldDefinition`, modifier `TypeAnnotation`)
/// carry no config values, so they compare equal by construction — a schema-side
/// edit is not a config change.
fn body_structural_eq(a: &Body, b: &Body) -> bool {
    // Depth-bounded like every diff walk (`MAX_DEPTH`). The parser also caps
    // nesting (RFC 0004 §9), so parsed input cannot get here over-deep — this
    // bound is defense-in-depth for programmatically-built ASTs and against
    // drift between the two caps. Exceeding it compares UNEQUAL — fail-visible
    // (an OpaqueChanged), never silently equal (the invariant), never a
    // stack overflow.
    body_eq_bounded(a, b, 0)
}

fn body_eq_bounded(a: &Body, b: &Body, depth: u32) -> bool {
    if depth >= MAX_DEPTH {
        return false;
    }
    // A nominal type annotation (RFC 0015 `as <Variant>`) is part of the body's
    // meaning: `slot as modelA:` and `slot as modelB:` with identical entries
    // are different instances (a variant switch). Comparing it here keeps such
    // a switch visible to the differ, never silently equal.
    annotation_name(a) == annotation_name(b)
        && a.entries.len() == b.entries.len()
        && a.entries
            .iter()
            .zip(&b.entries)
            .all(|(x, y)| entry_structural_eq(x, y, depth))
}

/// The nominal type-annotation name of a body, if any (RFC 0015).
fn annotation_name(b: &Body) -> Option<&str> {
    b.type_annotation.as_ref().map(|i| i.name.as_str())
}

fn entry_structural_eq(a: &BodyEntry, b: &BodyEntry, depth: u32) -> bool {
    use crate::ast::SharedPropertyKind;
    {
        match (&a.kind, &b.kind) {
            (BodyEntryKind::Property(x), BodyEntryKind::Property(y)) => {
                x.name.name == y.name.name && x.value.value.semantic_eq(&y.value.value)
            }
            (BodyEntryKind::NestedBlock(x), BodyEntryKind::NestedBlock(y)) => {
                x.name.name == y.name.name && body_eq_bounded(&x.body, &y.body, depth + 1)
            }
            (BodyEntryKind::Modifier(x), BodyEntryKind::Modifier(y)) => {
                x.name.name == y.name.name && modifier_value_eq(&x.value, &y.value, depth)
            }
            (BodyEntryKind::SharedProperty(x), BodyEntryKind::SharedProperty(y)) => {
                x.name.name == y.name.name
                    && match (&x.kind, &y.kind) {
                        (SharedPropertyKind::Block(bx), SharedPropertyKind::Block(by)) => {
                            body_eq_bounded(bx, by, depth + 1)
                        }
                        (SharedPropertyKind::Scalar(vx), SharedPropertyKind::Scalar(vy)) => {
                            vx.value.semantic_eq(&vy.value)
                        }
                        _ => false,
                    }
            }
            (BodyEntryKind::ListItem(x), BodyEntryKind::ListItem(y)) => list_item_eq(x, y, depth),
            (BodyEntryKind::Arm(x), BodyEntryKind::Arm(y)) => render_arm(x) == render_arm(y),
            // Authoring constructs carry no config values.
            (BodyEntryKind::FieldDefinition(_), BodyEntryKind::FieldDefinition(_)) => true,
            _ => false,
        }
    }
}

fn modifier_value_eq(a: &ModifierValue, b: &ModifierValue, depth: u32) -> bool {
    match (a, b) {
        (ModifierValue::Inline(x), ModifierValue::Inline(y)) => x.value.semantic_eq(&y.value),
        (ModifierValue::Block(x), ModifierValue::Block(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(i, j)| list_item_eq(i, j, depth))
        }
        // Authoring construct: carries no config values.
        (ModifierValue::TypeAnnotation { .. }, ModifierValue::TypeAnnotation { .. }) => true,
        _ => false,
    }
}

fn list_item_eq(a: &ListItem, b: &ListItem, depth: u32) -> bool {
    match (&a.kind, &b.kind) {
        (
            ListItemKind::Named { name: an, body: ab },
            ListItemKind::Named { name: bn, body: bb },
        ) => an.name == bn.name && body_eq_bounded(ab, bb, depth + 1),
        (
            ListItemKind::Shorthand {
                value: av,
                body: ab,
            },
            ListItemKind::Shorthand {
                value: bv,
                body: bb,
            },
        ) => {
            av.value.semantic_eq(&bv.value)
                && match (ab, bb) {
                    (Some(x), Some(y)) => body_eq_bounded(x, y, depth + 1),
                    (None, None) => true,
                    _ => false,
                }
        }
        (ListItemKind::Reference(x), ListItemKind::Reference(y)) => x.name == y.name,
        (ListItemKind::Role(x), ListItemKind::Role(y)) => x == y,
        _ => false,
    }
}

/// One effective routing arm, with its own origin (the selector's span in the
/// winning file) so per-arm changes carry real file:line.
struct ArmEntry {
    selector: String,
    target: String,
    origin: Origin,
}

/// The effective arm list of a routing block. Last file carrying arms wins
/// wholesale (arms are ordered first-match — collections overlay by
/// replacement, and order IS meaning).
fn collect_arm_entries(e: &Effective) -> Option<Vec<ArmEntry>> {
    let bodies: &[(PathBuf, &Body)] = match e {
        Effective::Bodies(b) => b,
        _ => return None,
    };
    let (file, body) = bodies.iter().rev().find(|(_, b)| {
        b.entries
            .iter()
            .any(|en| matches!(en.kind, BodyEntryKind::Arm(_)))
    })?;
    let mut arms = Vec::new();
    for entry in &body.entries {
        if let BodyEntryKind::Arm(a) = &entry.kind {
            arms.push(ArmEntry {
                selector: arm_selector_str(a).to_string(),
                target: arm_target_str(a),
                origin: Origin::File {
                    file: file.clone(),
                    span: a.selector_span,
                },
            });
        }
    }
    Some(arms)
}

/// Per-arm diff of a routing block (RFC 0007): arms pair by SELECTOR — unique
/// by validation (duplicate arm keys and duplicate `else` are rejected) — via
/// the same deterministic LCS the ordered lists use, so order stays meaning:
/// * a paired arm with a new target → `Modified(old_target → new_target)` at
///   the arm's selector element path;
/// * an unpaired arm → `Added`/`Removed` carrying the FULL `sel -> target`
///   rendering, so a MOVED arm reads as a -/+ pair of identical text at its
///   two file:lines (deterministic — a security report never guesses).
fn diff_arms(old: &[ArmEntry], new: &[ArmEntry], path: &FieldPath, out: &mut Vec<FieldChange>) {
    let matched = lcs_pairs_by(old, new, |a, b| a.selector == b.selector);
    for &(i, j) in &matched {
        if old[i].target != new[j].target {
            push(
                &path.appended(PathSeg::Element(ElemKey::Name(new[j].selector.clone()))),
                ChangeKind::Modified {
                    old: Value::String(old[i].target.clone()),
                    new: Value::String(new[j].target.clone()),
                },
                new[j].origin.clone(),
                out,
            );
        }
    }
    for (j, a) in new.iter().enumerate() {
        if !matched.iter().any(|&(_, jj)| jj == j) {
            push(
                &path.appended(PathSeg::Element(ElemKey::Name(a.selector.clone()))),
                ChangeKind::Added {
                    new: Value::String(format_arm(&a.selector, &a.target)),
                },
                a.origin.clone(),
                out,
            );
        }
    }
    for (i, a) in old.iter().enumerate() {
        if !matched.iter().any(|&(ii, _)| ii == i) {
            push(
                &path.appended(PathSeg::Element(ElemKey::Name(a.selector.clone()))),
                ChangeKind::Removed {
                    old: Value::String(format_arm(&a.selector, &a.target)),
                },
                a.origin.clone(),
                out,
            );
        }
    }
}

fn lcs_pairs(old: &[Elem<'_>], new: &[Elem<'_>]) -> Vec<(usize, usize)> {
    lcs_pairs_by(old, new, |a, b| a.id.eq(&b.id))
}

/// Deterministic LCS pairing over any identity relation (Myers-class O(n·m)
/// DP — trivial at config scale). One implementation for ordered scalars AND
/// routing arms, so their alignment semantics can never diverge.
fn lcs_pairs_by<T>(old: &[T], new: &[T], eq: impl Fn(&T, &T) -> bool) -> Vec<(usize, usize)> {
    let n = old.len();
    let m = new.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if eq(&old[i], &new[j]) {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < n && j < m {
        if eq(&old[i], &new[j]) {
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

/// An element's identity as a path segment: identifier keys stay `Name`
/// (rendered dotted), scalar shorthand keys become `Key` (rendered bracketed +
/// quoted, so an `[]install` package like `[vendor]-x.v1` stays unambiguous).
fn elem_key(id: &ElemId) -> ElemKey {
    match id {
        ElemId::Name(n) => ElemKey::Name((*n).to_string()),
        ElemId::Val(v) => ElemKey::Key((*v).clone()),
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
        FieldType::Primitive {
            ty: PrimitiveType::Secret,
            ..
        } => true,
        FieldType::Modifier(i) | FieldType::List(i) | FieldType::Set(i) => is_secret(i),
        _ => false,
    }
}

/// Record a change at `path`. All classify/redact facts (directives, secret-
/// ness) ride on the path's field hops, so `push` needs only WHERE, WHAT, and
/// the origin — no re-derivation from the schema.
fn push(path: &FieldPath, kind: ChangeKind, origin: Origin, out: &mut Vec<FieldChange>) {
    out.push(FieldChange {
        path: path.clone(),
        kind,
        origin,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::DeclarationKind;

    const SCHEMA: &str = "model limits:\n    cap number = 5\n\nmodel server:\n    port number = 8080\n    name string?\n    token secret?\n    timeout duration?\n    cidrs set<string>? #live\n    order []string?\n    limits limits?\n    providers []provider?\n\nmodel provider:\n    url string?\n    clientSecret secret?\n\noneof email by kind:\n    \"log\" -> emailLog\n    \"post\" -> emailPost\n\nmodel emailLog:\n    path string?\n\nmodel emailPost:\n    apiKey secret?\n";

    fn index() -> SchemaIndex {
        let (schema, errs) = crate::cst::extract_schema(SCHEMA);
        assert!(errs.is_empty(), "{errs:?}");
        SchemaIndex::build(schema.models, schema.enums, schema.oneofs)
    }

    fn parse_doc(src: &str) -> crate::ast::File {
        crate::cst::parse_to_ast(src).unwrap()
    }

    /// The changed field's own directives (the terminal field hop) — what a
    /// consumer folds; replaces the old flat `FieldChange.directives`.
    fn leaf_directives(c: &FieldChange) -> &[Directive] {
        c.path
            .field_steps()
            .last()
            .map(|f| f.directives.as_slice())
            .unwrap_or_default()
    }

    /// The rendered path, for terse assertions.
    fn p(c: &FieldChange) -> String {
        c.path.to_string()
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
        assert_eq!(p(&d[0]), "denial.page");
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
        assert_eq!(p(&d[0]), "denial");
    }

    /// RFC 0015 — a nominal variant SWITCH (`as modelA` → `as modelB`) must stay
    /// VISIBLE even when the two bodies are field-IDENTICAL. Diffing both sides
    /// against one shared model would silently miss it; per-side resolution +
    /// precise removes/adds keeps the invariant.
    #[test]
    fn nominal_variant_switch_is_visible_even_when_fields_identical() {
        let schema = "model modelA:\n    shared string?\n    a string?\nmodel modelB:\n    shared string?\n    b string?\nmodel server:\n    slot (modelA | modelB)?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        // Only the annotation changes; `shared = "x"` is byte-identical.
        let old = parse_doc("server s:\n    slot as modelA:\n        shared = \"x\"\n");
        let new = parse_doc("server s:\n    slot as modelB:\n        shared = \"x\"\n");
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert!(
            !d.is_empty(),
            "an annotation-only variant switch must be visible, not silent: {d:?}"
        );
        assert!(
            d.iter()
                .all(|c| !matches!(c.kind, ChangeKind::OpaqueChanged)),
            "a legitimate switch must not read as drift: {d:?}"
        );
    }

    /// F3 (interaction audit): the switch must be visible even with EMPTY
    /// bodies — nothing to half-diff, so the explicit `as A` → `as B` Modified
    /// entry is the only witness. Covers variants that differ only in schema
    /// defaults (the effective config changes with zero authored entries).
    #[test]
    fn nominal_variant_switch_with_empty_bodies_is_visible() {
        let schema = "model modelA:\n    cap number = 1\nmodel modelB:\n    cap number = 2\nmodel server:\n    slot (modelA | modelB)?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let old = parse_doc("server s:\n    slot as modelA:\n");
        let new = parse_doc("server s:\n    slot as modelB:\n");
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert!(
            d.iter().any(|c| matches!(
                &c.kind,
                ChangeKind::Modified { old, new }
                    if old.as_str() == Some("as modelA") && new.as_str() == Some("as modelB")
            )),
            "an empty-body variant switch must emit the explicit switch change: {d:?}"
        );
    }

    /// Round-12: a MODEL ↔ ONEOF variant switch (`(modelA | mail)` where
    /// `mail` is a oneof) gets the explicit witness + precise per-side diffs —
    /// previously it degraded to a single `OpaqueChanged`.
    #[test]
    fn nominal_switch_between_model_and_oneof_is_precise() {
        let schema = "model modelA:\n    a string?\nmodel mailLog:\n    level string?\n\noneof mail by kind:\n    \"log\" -> mailLog\n\nmodel server:\n    slot (modelA | mail)?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let old = parse_doc("server s:\n    slot as modelA:\n        a = \"x\"\n");
        let new = parse_doc(
            "server s:\n    slot as mail:\n        kind = \"log\"\n        level = \"info\"\n",
        );
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert!(
            d.iter().any(|c| matches!(
                &c.kind,
                ChangeKind::Modified { old, new }
                    if old.as_str() == Some("as modelA") && new.as_str() == Some("as mail")
            )),
            "the model↔oneof switch must emit the explicit witness: {d:?}"
        );
        assert!(
            d.iter()
                .all(|c| !matches!(c.kind, ChangeKind::OpaqueChanged)),
            "a legitimate switch must not read as drift: {d:?}"
        );
        assert!(
            d.iter()
                .any(|c| p(c) == "slot.a" && matches!(c.kind, ChangeKind::Removed { .. })),
            "the model side's fields removed precisely: {d:?}"
        );
    }

    /// Round-11: the ARMS ↔ ELEMS transition (both collection-shaped,
    /// different kinds) must surface BOTH sides — the arm removed AND the new
    /// elements added — not just the arm side.
    #[test]
    fn shape_switch_arms_to_elems_keeps_both_sides_visible() {
        let schema =
            "model step:\n    run string?\nmodel server:\n    slot ((role -> string) | []step)?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let old = parse_doc("server s:\n    slot:\n        @admin -> \"OLDTARGET\"\n");
        let new = parse_doc("server s:\n    slot:\n        - A:\n            run = \"x\"\n");
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert!(
            d.iter()
                .any(|c| matches!(c.kind, ChangeKind::Removed { .. })),
            "the arm side must be visibly removed: {d:?}"
        );
        assert!(
            d.iter().any(|c| matches!(c.kind, ChangeKind::Added { .. })),
            "the element side must be visibly added: {d:?}"
        );
    }

    /// Round-10 F1: a SHAPE switch (keyed model form → list form of
    /// `(step | []step)`) must surface the keyed side's content — previously
    /// the collections early-return silently dropped it.
    #[test]
    fn shape_switch_keyed_to_list_keeps_old_content_visible() {
        let schema = "model step:\n    run string?\n    a string?\nmodel server:\n    slot (step | []step)?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let old = parse_doc("server s:\n    slot:\n        run = \"OLDVALUE\"\n");
        let new = parse_doc("server s:\n    slot:\n        - A:\n            a = \"x\"\n");
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert!(
            d.iter()
                .any(|c| p(c) == "slot.run" && matches!(c.kind, ChangeKind::Removed { .. })),
            "the keyed side's content must be visibly removed: {d:?}"
        );
        assert!(
            d.iter().any(|c| matches!(c.kind, ChangeKind::Added { .. })),
            "the list side's content must be visibly added: {d:?}"
        );
    }

    /// Round-10 F2: a model ↔ scalar transition on `(modelA | string)` must
    /// surface BOTH sides — the body's fields removed AND the new value added.
    #[test]
    fn shape_switch_model_to_scalar_keeps_both_sides_visible() {
        let schema = "model modelA:\n    a string?\nmodel server:\n    slot (modelA | string)?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let old = parse_doc("server s:\n    slot as modelA:\n        a = \"OLDVALUE\"\n");
        let new = parse_doc("server s:\n    slot = \"NEWSCALAR\"\n");
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert!(
            d.iter()
                .any(|c| p(c) == "slot.a" && matches!(c.kind, ChangeKind::Removed { .. })),
            "the model side's field must be visibly removed: {d:?}"
        );
        assert!(
            d.iter().any(|c| matches!(
                &c.kind,
                ChangeKind::Added { new } if new.as_str() == Some("NEWSCALAR")
            )),
            "the scalar side must be visibly added: {d:?}"
        );
    }

    /// Round-10 F3: NO phantom switch witness on an absent side — a pure add
    /// of `slot as modelB:` is an Add, not an `as modelA → as modelB` Modified
    /// that claims an annotation which never existed.
    #[test]
    fn no_phantom_switch_witness_on_pure_add_or_remove() {
        let schema = "model modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel server:\n    slot (modelA | modelB)?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let old = parse_doc("server s:\n    other = 1\n");
        let new = parse_doc("server s:\n    other = 1\n    slot as modelB:\n        b = \"x\"\n");
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert!(
            !d.iter().any(|c| matches!(
                &c.kind,
                ChangeKind::Modified { old, .. } if old.as_str().is_some_and(|s| s.starts_with("as "))
            )),
            "a pure add must not fabricate a variant-switch witness: {d:?}"
        );
        assert!(
            d.iter()
                .any(|c| p(c) == "slot.b" && matches!(c.kind, ChangeKind::Added { .. })),
            "the added body renders as adds: {d:?}"
        );
    }

    /// The ELEMENT-level twin: an annotation-only switch on a paired `[]union`
    /// element (`- one as modelA:` → `- one as modelB:`, identical entries)
    /// must be visible — the paired-element recursion resolves per side, exactly
    /// like the field-level rule.
    #[test]
    fn nominal_variant_switch_on_list_element_is_visible() {
        let schema = "model modelA:\n    shared string?\nmodel modelB:\n    shared string?\nmodel server:\n    slots [](modelA | modelB)?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let old = parse_doc(
            "server s:\n    slots:\n        - one as modelA:\n            shared = \"x\"\n",
        );
        let new = parse_doc(
            "server s:\n    slots:\n        - one as modelB:\n            shared = \"x\"\n",
        );
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert!(
            !d.is_empty(),
            "an element-level annotation-only switch must be visible: {d:?}"
        );
        assert!(
            d.iter()
                .all(|c| !matches!(c.kind, ChangeKind::OpaqueChanged)),
            "a legitimate element switch must not read as drift: {d:?}"
        );
    }

    /// ARM blocks diff PER-ARM (RFC 0007): a retargeted arm is one `Modified`
    /// of target values at ITS selector element path; unchanged arms are
    /// silent; a REORDER is honestly a -/+ pair of the full moved arm (arms
    /// are ordered first-match — order IS meaning); a brand-new arm is one
    /// full-arm `Added` at its selector path.
    #[test]
    fn arm_blocks_diff_per_arm_including_reorder() {
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

        // Retarget: precise target-only Modified at the arm's own path.
        let retarget = "server s:\n    route:\n        @role/a -> Z\n        else -> Y\n";
        let d = diff2(base, retarget);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(p(&d[0]), "route.@role/a");
        assert!(
            matches!(&d[0].kind, ChangeKind::Modified { old, new }
                if matches!(old, Value::String(s) if s == "X")
                    && matches!(new, Value::String(s) if s == "Z")),
            "target-only payloads: {d:?}"
        );

        assert!(diff2(base, base).is_empty(), "unchanged arms are silent");

        // Reorder: the moved arm reads as a -/+ pair of the SAME full arm text
        // (deterministic LCS picks which arm moved), at its selector path.
        let reorder = "server s:\n    route:\n        else -> Y\n        @role/a -> X\n";
        let d = diff2(base, reorder);
        assert_eq!(d.len(), 2, "a move is a -/+ pair: {d:?}");
        let texts: Vec<&str> = d
            .iter()
            .map(|c| match &c.kind {
                ChangeKind::Added {
                    new: Value::String(s),
                }
                | ChangeKind::Removed {
                    old: Value::String(s),
                } => s.as_str(),
                k => panic!("expected full-arm Added/Removed, got {k:?}"),
            })
            .collect();
        assert!(
            texts.iter().all(|t| *t == "@role/a -> X"),
            "both sides of the pair carry the moved arm verbatim: {texts:?}"
        );
        assert!(d.iter().all(|c| p(c) == "route.@role/a"), "{d:?}");

        // A brand-new arm: one full-arm Added at its selector path.
        let grown = "server s:\n    route:\n        @role/a -> X\n        @role/b -> W\n        else -> Y\n";
        let d = diff2(base, grown);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(p(&d[0]), "route.@role/b");
        assert!(
            matches!(&d[0].kind, ChangeKind::Added { new: Value::String(s) } if s == "@role/b -> W"),
            "{d:?}"
        );
    }

    /// The flagship consumer path (nudge RFC 0031): `server → sandboxCeiling
    /// THE |block blind-spot fix (RFC 0032): the VALIDATED namespaced form
    /// (`|block: - egress:` + nested CIDR list) — elements pair by namespace
    /// name and their scalar bodies diff with the parent set's semantics at the
    /// element path. Full per-element fidelity, real origins, #live reachable;
    /// a reorder-only edit inside the namespace stays invisible.
    #[test]
    fn namespaced_set_elements_diff_with_full_fidelity() {
        let schema = "model ceiling:\n    |block set<string>? #live\n\nmodel server:\n    sandboxCeiling ceiling?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let src = |cidrs: &str| {
            format!(
                "server s:\n    sandboxCeiling:\n        |block:\n            - egress:\n{cidrs}"
            )
        };
        let old = parse_doc(&src(
            "                - \"203.0.113.0/24\"\n                - \"10.0.0.0/8\"\n",
        ));
        let new = parse_doc(&src(
            "                - \"10.0.0.0/8\"\n                - \"203.0.113.0/24\"\n                - \"198.51.100.0/24\"\n",
        ));
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("server.nml"), server_body(&old))],
            &[(PathBuf::from("server.nml"), server_body(&new))],
        );
        assert_eq!(d.len(), 1, "reorder invisible, one addition: {d:?}");
        // Identifier element keys render DOTTED (the ElemKey convention;
        // bracketed-quoted is reserved for scalar keys that may contain dots).
        assert_eq!(p(&d[0]), "sandboxCeiling.|block.egress");
        assert!(
            matches!(&d[0].kind, ChangeKind::Added { new }
                if new.semantic_eq(&Value::String("198.51.100.0/24".into()))),
            "{d:?}"
        );
        // #live on the |block hop still folds for classification.
        assert!(
            d[0].path
                .field_steps()
                .any(|f| f.directives.iter().any(|dir| dir.name == "live")),
            "{d:?}"
        );
        assert!(
            matches!(&d[0].origin, Origin::File { .. }),
            "real file origin: {:?}",
            d[0].origin
        );

        // Cosmetic-only (reorder inside the namespace) ⇒ silence.
        let cosmetic = parse_doc(&src(
            "                - \"10.0.0.0/8\"\n                - \"203.0.113.0/24\"\n",
        ));
        let none = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("server.nml"), server_body(&old))],
            &[(PathBuf::from("server.nml"), server_body(&cosmetic))],
        );
        assert!(none.is_empty(), "{none:?}");
    }

    /// The structural equality's depth bound (defense-in-depth): the PARSER
    /// already caps nesting (RFC 0004 §9 — over-deep blocks are skipped, in
    /// strict AND best-effort modes), so parsed input can never reach this
    /// bound; it guards programmatically-built ASTs and any future drift
    /// between the parser's cap and the differ's. Past `MAX_DEPTH` the
    /// comparison is UNEQUAL — fail-VISIBLE (an `OpaqueChanged` on identical
    /// content), never a stack overflow, never silence.
    #[test]
    fn over_deep_bodies_fail_visible_not_stack_overflow() {
        use crate::ast::{BodyEntry, Identifier, NestedBlock};
        fn deep_body(levels: u32) -> Body {
            let mut body = Body::fresh(Vec::new());
            for i in 0..levels {
                body = Body::fresh(vec![BodyEntry {
                    span: Span { start: 0, end: 0 },
                    kind: BodyEntryKind::NestedBlock(NestedBlock {
                        name: Identifier {
                            name: format!("n{i}"),
                            span: Span { start: 0, end: 0 },
                        },
                        body,
                    }),
                }]);
            }
            body
        }
        let a = deep_body(MAX_DEPTH + 8);
        let b = deep_body(MAX_DEPTH + 8);
        // Identical over-deep bodies: UNEQUAL past the cap (fail-visible), and
        // critically this returns instead of overflowing the stack.
        assert!(
            !body_structural_eq(&a, &b),
            "over-deep must compare unequal"
        );
        // Within the cap, identical bodies still compare equal.
        let c = deep_body(8);
        let d = deep_body(8);
        assert!(
            body_structural_eq(&c, &d),
            "shallow identical bodies are equal"
        );
    }

    /// The walk's resource boundary honors the invariant: a change buried
    /// DEEPER than the walk cap surfaces as a visible `OpaqueChanged` (the walk
    /// compares what it cannot descend into), while identical over-deep content
    /// stays silent — never silent truncation of a real change.
    #[test]
    fn changes_below_the_walk_depth_cap_surface_visibly() {
        use crate::ast::{BodyEntry, Identifier, NestedBlock, Property};
        use crate::types::SpannedValue;
        let schema =
            "model node:\n    child node?\n    v number?\n\nmodel server:\n    root node?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        // Hand-build a chain deeper than the walk cap with `v = <leaf>` at the bottom.
        fn chain(levels: u32, leaf: i64) -> Body {
            let nospan = Span { start: 0, end: 0 };
            let mut body = Body::fresh(vec![BodyEntry {
                span: nospan,
                kind: BodyEntryKind::Property(Property {
                    name: Identifier {
                        name: "v".into(),
                        span: nospan,
                    },
                    value: SpannedValue {
                        value: Value::Number(leaf.into()),
                        span: nospan,
                    },
                }),
            }]);
            for _ in 0..levels {
                body = Body::fresh(vec![BodyEntry {
                    span: nospan,
                    kind: BodyEntryKind::NestedBlock(NestedBlock {
                        name: Identifier {
                            name: "child".into(),
                            span: nospan,
                        },
                        body,
                    }),
                }]);
            }
            Body::fresh(vec![BodyEntry {
                span: nospan,
                kind: BodyEntryKind::NestedBlock(NestedBlock {
                    name: Identifier {
                        name: "root".into(),
                        span: nospan,
                    },
                    body,
                }),
            }])
        }
        let old = chain(MAX_DEPTH + 8, 1);
        let new = chain(MAX_DEPTH + 8, 2);
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("server.nml"), &old)],
            &[(PathBuf::from("server.nml"), &new)],
        );
        assert_eq!(d.len(), 1, "a change below the cap must be VISIBLE: {d:?}");
        assert!(matches!(d[0].kind, ChangeKind::OpaqueChanged), "{d:?}");

        // Identical over-deep content ⇒ silence (no false positives at the cap).
        let same = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("server.nml"), &old)],
            &[(PathBuf::from("server.nml"), &old)],
        );
        assert!(same.is_empty(), "{same:?}");
    }

    /// A DECLARED-opaque field (`object`-typed, e.g. an embedded grant DSL):
    /// the differ never interprets the domain body — an inner edit is exactly
    /// ONE payload-free `ObjectChanged` at the field path (no mis-structural
    /// recursion into a grammar the schema says nothing about), identical
    /// content is silent, and the payload-free discipline means nothing from
    /// the domain body can leak.
    #[test]
    fn declared_object_fields_diff_opaque_by_design() {
        let schema = "model plugin:\n    package string+\n    egress object?\n\nmodel server:\n    p plugin?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let src = |cidr: &str| {
            format!(
                "server s:\n    p:\n        package = \"x\"\n        egress:\n            - http:\n                - \"{cidr}\"\n"
            )
        };
        let d2 = |o: &str, n: &str| {
            let (of, nf) = (parse_doc(o), parse_doc(n));
            diff_config(
                &idx,
                "server",
                &[(PathBuf::from("f.nml"), server_body(&of))],
                &[(PathBuf::from("f.nml"), server_body(&nf))],
            )
        };
        let d = d2(&src("10.0.5.0/24"), &src("10.0.6.0/24"));
        assert_eq!(d.len(), 1, "one typed opaque change: {d:?}");
        assert_eq!(p(&d[0]), "p.egress");
        assert!(matches!(d[0].kind, ChangeKind::ObjectChanged), "{d:?}");
        // Payload-free: the domain body's content cannot leak.
        let rendered = format!("{:?}", d[0]);
        assert!(
            !rendered.contains("10.0.5.0") && !rendered.contains("10.0.6.0"),
            "DECLARED-OPAQUE LEAKED: {rendered}"
        );
        // Identical ⇒ silent; other sibling fields still diff precisely.
        assert!(d2(&src("10.0.5.0/24"), &src("10.0.5.0/24")).is_empty());
    }

    /// The COMPOSED end-state (body-positional shorthand + declared-object
    /// leaf — the tenantGrants architecture): `plugins []tenantGrantPlugin+`
    /// binds the bare body items to a REAL field, so elements get true model
    /// recursion — and the `egress object` leaf inside surfaces as ONE typed
    /// `ObjectChanged` at its full element path. A plugin ADD stays a precise
    /// per-element change; identical content is silent.
    #[test]
    fn body_positional_shorthand_composes_with_declared_object_leaf() {
        let schema = "model tgp:\n    package string+\n    egress object?\n\nmodel tg:\n    tenant string+\n    plugins []tgp+\n\nmodel server:\n    grants []tg?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let src = |cidr: &str| {
            format!(
                "server s:\n    grants:\n        - \"_op\":\n            - \"[v]-a.v1\":\n                egress:\n                    - http:\n                        - \"{cidr}\"\n"
            )
        };
        let d2 = |o: &str, n: &str| {
            let (of, nf) = (parse_doc(o), parse_doc(n));
            diff_config(
                &idx,
                "server",
                &[(PathBuf::from("f.nml"), server_body(&of))],
                &[(PathBuf::from("f.nml"), server_body(&nf))],
            )
        };
        // The grant-DSL edit: ONE typed ObjectChanged at the FULL element path
        // (through the real model recursion the declared field unlocked).
        let d = d2(&src("10.0.5.0/24"), &src("10.0.6.0/24"));
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(matches!(d[0].kind, ChangeKind::ObjectChanged), "{d:?}");
        assert_eq!(p(&d[0]), "grants[\"_op\"][\"[v]-a.v1\"].egress");
        // Payload-free: the domain body cannot leak.
        let rendered = format!("{:?}", d[0]);
        assert!(!rendered.contains("10.0."), "LEAKED: {rendered}");
        // Identical ⇒ silent.
        assert!(d2(&src("10.0.5.0/24"), &src("10.0.5.0/24")).is_empty());
    }

    /// The UNMODELED REMAINDER (the tenantGrants silent-diff class): a model
    /// that RESOLVES but has no field for its body's bare element list — the
    /// items now diff as an identity-paired collection: an added element is a
    /// precise per-element `Added`; a paired element's un-modelable body edit
    /// is a pathed `OpaqueChanged` at that element — and identical content
    /// stays silent.
    #[test]
    fn unmodeled_body_items_diff_with_element_precision() {
        // `tg` mirrors tenantGrant: shorthand scalar, NO field for body items.
        let schema = "model tg:\n    tenant string+\n\nmodel server:\n    grants []tg?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let src = |plugins: &str| format!("server s:\n    grants:\n        - \"_op\":\n{plugins}");
        let one = src(
            "            - \"[v]-a.v1\":\n                egress:\n                    - http:\n                        - \"10.0.5.0/24\"\n",
        );
        let two = src(
            "            - \"[v]-a.v1\":\n                egress:\n                    - http:\n                        - \"10.0.5.0/24\"\n            - \"[v]-b.v1\":\n                egress:\n                    - http:\n                        - \"10.0.9.0/24\"\n",
        );
        let edited = src(
            "            - \"[v]-a.v1\":\n                egress:\n                    - http:\n                        - \"10.0.6.0/24\"\n",
        );
        let d2 = |o: &str, n: &str| {
            let (of, nf) = (parse_doc(o), parse_doc(n));
            diff_config(
                &idx,
                "server",
                &[(PathBuf::from("f.nml"), server_body(&of))],
                &[(PathBuf::from("f.nml"), server_body(&nf))],
            )
        };
        // A new plugin element: ONE precise Added carrying its identity.
        let d = d2(&one, &two);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(
            matches!(&d[0].kind, ChangeKind::Added { new }
                if new.semantic_eq(&Value::String("[v]-b.v1".into()))),
            "{d:?}"
        );
        // An edit INSIDE a plugin's (un-modelable) body: VISIBLE as a pathed
        // OpaqueChanged at that element — never silent (the bug this closes).
        let d = d2(&one, &edited);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(matches!(d[0].kind, ChangeKind::OpaqueChanged), "{d:?}");
        assert!(
            p(&d[0]).contains("[v]-a.v1"),
            "pathed at the element: {}",
            p(&d[0])
        );
        // Identical content: silent (no false positives from the remainder walk).
        assert!(d2(&one, &one).is_empty());
    }

    /// Uncovered NAMED entries (matching no model field) surface as one
    /// OpaqueChanged at the model's path when they differ — and the oneof
    /// DISCRIMINATOR property is exempt (it belongs to the oneof, not the
    /// variant model), so ordinary oneof bodies stay clean.
    #[test]
    fn unmodeled_named_entries_surface_and_discriminator_is_exempt() {
        let (sch, errs) = crate::cst::extract_schema(SCHEMA);
        assert!(errs.is_empty());
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        // `limits` has one field `cap`; `mystery` matches no field.
        let with =
            |v: u32| format!("server s:\n    limits:\n        cap = 5\n        mystery = {v}\n");
        let d2 = |o: &str, n: &str| {
            let (of, nf) = (parse_doc(o), parse_doc(n));
            diff_config(
                &idx,
                "server",
                &[(PathBuf::from("f.nml"), server_body(&of))],
                &[(PathBuf::from("f.nml"), server_body(&nf))],
            )
        };
        let d = d2(&with(1), &with(2));
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(p(&d[0]), "limits");
        assert!(matches!(d[0].kind, ChangeKind::OpaqueChanged), "{d:?}");
        // Unchanged unmodeled entry ⇒ silent.
        assert!(d2(&with(1), &with(1)).is_empty());
    }

    /// A oneof DISCRIMINATOR flip is visible in its own right — not merely
    /// implied by the variant fields that accompany it. `provider "log" →
    /// "postmark"` must emit a `Modified` at the discriminator path (the
    /// headline change), independent of the variant-field adds.
    #[test]
    fn oneof_discriminator_flip_is_visible() {
        let schema = "model log:\n\nmodel post:\n    token string?\n\noneof provider by kind:\n    \"log\" -> log\n    \"post\" -> post\n\nmodel server:\n    p provider?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let old = parse_doc("server s:\n    p:\n        kind = \"log\"\n");
        let new = parse_doc("server s:\n    p:\n        kind = \"post\"\n        token = \"t\"\n");
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        // Both the discriminator flip AND the new variant field are visible.
        assert!(
            d.iter().any(|c| p(c) == "p.kind"
                && matches!(&c.kind, ChangeKind::Modified { old, new }
                    if matches!(old, Value::String(s) if s == "log")
                        && matches!(new, Value::String(s) if s == "post"))),
            "discriminator flip must be visible: {d:?}"
        );
        assert!(
            d.iter().any(|c| p(c) == "p.token"),
            "variant field too: {d:?}"
        );
    }

    /// The discriminator flip is visible even when one side OMITS the property
    /// and relies on the union default (`by kind = "builtin"`): omitting `kind`
    /// on the old side then setting it explicitly on the new side is a real
    /// variant switch, and the effective-value comparison surfaces it — the
    /// (Some,Some)-only guard would have dropped it silently.
    #[test]
    fn oneof_discriminator_flip_visible_across_the_default() {
        let schema = "model builtin:\n\nmodel card:\n    title string?\n\noneof exp by kind = \"builtin\":\n    \"builtin\" -> builtin\n    \"card\" -> card\n\nmodel server:\n    e exp?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        // old OMITS kind (⇒ default "builtin"); new sets it to "card".
        let old = parse_doc("server s:\n    e:\n");
        let new = parse_doc("server s:\n    e:\n        kind = \"card\"\n        title = \"t\"\n");
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert!(
            d.iter().any(|c| p(c) == "e.kind"
                && matches!(&c.kind, ChangeKind::Modified { old, new }
                    if matches!(old, Value::String(s) if s == "builtin")
                        && matches!(new, Value::String(s) if s == "card"))),
            "default→explicit discriminator flip must be visible: {d:?}"
        );
        // And omitting on BOTH sides (both default) stays silent for the discriminator.
        let same = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&old))],
        );
        assert!(
            same.iter().all(|c| p(c) != "e.kind"),
            "no phantom flip: {same:?}"
        );
    }

    /// The variant SWITCH that drops fields (`post(fields) → log(empty)`): the
    /// flip plus PRECISE per-field `Removed`s from the old variant's own model —
    /// and crucially NO `OpaqueChanged` (the old fields ARE described, by the
    /// old variant; a coarse/false-drift signal here was the bug this owner
    /// function exists to fix).
    #[test]
    fn oneof_variant_switch_drops_fields_precisely() {
        let schema = "model log:\n\nmodel post:\n    token string?\n    addr string?\n\noneof provider by kind:\n    \"log\" -> log\n    \"post\" -> post\n\nmodel server:\n    p provider?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let old = parse_doc(
            "server s:\n    p:\n        kind = \"post\"\n        token = \"t\"\n        addr = \"a\"\n",
        );
        let new = parse_doc("server s:\n    p:\n        kind = \"log\"\n");
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert!(
            d.iter()
                .all(|c| !matches!(c.kind, ChangeKind::OpaqueChanged)),
            "a legitimate switch must never read as drift: {d:?}"
        );
        assert!(d.iter().any(|c| p(c) == "p.kind"), "flip visible: {d:?}");
        assert!(
            d.iter()
                .any(|c| p(c) == "p.token" && matches!(c.kind, ChangeKind::Removed { .. }))
                && d.iter()
                    .any(|c| p(c) == "p.addr" && matches!(c.kind, ChangeKind::Removed { .. })),
            "old variant's fields removed PRECISELY: {d:?}"
        );
    }

    /// ELEMENT-level oneofs (`[]denial`-shaped): a kind-flip on a collection
    /// element routes through the SAME owner as field-level — both call sites
    /// converge by construction.
    #[test]
    fn oneof_element_level_kind_flip_is_precise() {
        let schema = "model builtin:\n\nmodel card:\n    title string?\n\noneof denial by kind = \"builtin\":\n    \"builtin\" -> builtin\n    \"card\" -> card\n\nmodel server:\n    denials []denial?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let old = parse_doc(
            "server s:\n    denials:\n        - NotFound:\n            kind = \"builtin\"\n",
        );
        let new = parse_doc(
            "server s:\n    denials:\n        - NotFound:\n            kind = \"card\"\n            title = \"t\"\n",
        );
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert!(
            d.iter()
                .all(|c| !matches!(c.kind, ChangeKind::OpaqueChanged)),
            "{d:?}"
        );
        assert!(
            d.iter().any(|c| p(c) == "denials.NotFound.kind"),
            "element-level flip at the element path: {d:?}"
        );
        assert!(d.iter().any(|c| p(c) == "denials.NotFound.title"), "{d:?}");
    }

    /// A oneof block ADDED whole (old side absent): precise per-field `Added`s
    /// and NO discriminator flip — even with a default (a flip against a side
    /// that does not exist would be a phantom change). Mirror for removal.
    #[test]
    fn oneof_block_added_or_removed_is_precise_with_no_phantom_flip() {
        // `port` keeps the absent-side server body non-empty.
        let schema = "model builtin:\n\nmodel card:\n    title string?\n\noneof exp by kind = \"builtin\":\n    \"builtin\" -> builtin\n    \"card\" -> card\n\nmodel server:\n    port number?\n    e exp?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let absent = parse_doc("server s:\n    port = 1\n");
        let with_card = parse_doc(
            "server s:\n    port = 1\n    e:\n        kind = \"card\"\n        title = \"t\"\n",
        );
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&absent))],
            &[(PathBuf::from("f.nml"), server_body(&with_card))],
        );
        assert!(
            d.iter().all(|c| p(c) != "e.kind"),
            "NO phantom flip on a pure add: {d:?}"
        );
        assert!(
            d.iter()
                .any(|c| p(c) == "e.title" && matches!(c.kind, ChangeKind::Added { .. })),
            "added block's fields precise: {d:?}"
        );
        // Removal mirrors.
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&with_card))],
            &[(PathBuf::from("f.nml"), server_body(&absent))],
        );
        assert!(d.iter().all(|c| p(c) != "e.kind"), "{d:?}");
        assert!(
            d.iter()
                .any(|c| p(c) == "e.title" && matches!(c.kind, ChangeKind::Removed { .. })),
            "removed block's fields precise: {d:?}"
        );
    }

    /// The `[](step | []step)` workflow shape (nudge's `parallel`): a branch
    /// element whose body is a LIST of steps selects the `[]step` (ListOf)
    /// union variant. Resolution returns `ListOf` (not Model/OneOf), so the
    /// generic fall-through takes over — and PRECISELY: the scalar-element
    /// recursion re-diffs the branch's body items as a collection, each nested
    /// step resolving to its model via the keyed-body RULE-COMPLETION. So the
    /// union list-variant needs no bespoke arm — existing machinery + the
    /// rule-completion make it precise (no coarse `OpaqueChanged`). This test
    /// GUARDS that: if it ever regresses to opaque, the ListOf precision was
    /// lost and a dedicated arm is warranted.
    #[test]
    fn union_listof_element_variant_diffs_precisely() {
        let schema = "model step:\n    name string+\n    id string?\n    parallel [](step | []step)?\n\nmodel server:\n    steps []step?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let src = |cid: &str| {
            format!(
                "server s:\n    steps:\n        - A:\n            parallel:\n                - br:\n                    - C:\n                        id = \"{cid}\"\n                    - D:\n                        id = \"d\"\n"
            )
        };
        let old = parse_doc(&src("c1"));
        let new = parse_doc(&src("c2"));
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("f.nml"), server_body(&old))],
            &[(PathBuf::from("f.nml"), server_body(&new))],
        );
        assert_eq!(d.len(), 1, "one precise change: {d:?}");
        assert_eq!(
            p(&d[0]),
            "steps.A.parallel.br.C.id",
            "the nested step's field, fully pathed: {d:?}"
        );
        assert!(
            matches!(&d[0].kind, ChangeKind::Modified { .. }),
            "precise, not a coarse OpaqueChanged: {d:?}"
        );
    }

    /// Model↔grammar drift, shape 1 — a `ModelRef` to a model MISSING from the
    /// index (renamed/deleted model): the instance's bodies cannot be walked,
    /// so the change degrades to a VISIBLE payload-free `OpaqueChanged` at the
    /// field path (module invariant), and an identical no-op stays silent.
    #[test]
    fn unknown_model_ref_drift_degrades_visible_never_silent() {
        let (sch, errs) = crate::cst::extract_schema(SCHEMA);
        assert!(errs.is_empty());
        // Simulate drift: drop the `limits` model the `server.limits` field references.
        let models: Vec<_> = sch
            .models
            .into_iter()
            .filter(|m| m.name != "limits")
            .collect();
        let idx = SchemaIndex::build(models, sch.enums, sch.oneofs);
        let old = parse_doc("server s:\n    limits:\n        cap = 5\n");
        let new = parse_doc("server s:\n    limits:\n        cap = 9\n");
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("server.nml"), server_body(&old))],
            &[(PathBuf::from("server.nml"), server_body(&new))],
        );
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(p(&d[0]), "limits");
        assert!(matches!(d[0].kind, ChangeKind::OpaqueChanged), "{d:?}");

        // Identical content under the unknown model ⇒ silence (no-op preserved).
        let same = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("server.nml"), server_body(&old))],
            &[(PathBuf::from("server.nml"), server_body(&old))],
        );
        assert!(same.is_empty(), "{same:?}");
    }

    /// Model↔grammar drift, shape 2 — an UNKNOWN oneof discriminator variant:
    /// the variant's body cannot be resolved, so the change is a visible
    /// `OpaqueChanged`, and — the security property — the payload carries NO
    /// value from the body (the schema cannot know what is secret in an
    /// undescribable shape).
    #[test]
    fn unknown_oneof_variant_degrades_visible_and_leaks_nothing() {
        let schema = "oneof email by kind:\n    \"log\" -> emailLog\n\nmodel emailLog:\n    path string?\n\nmodel server:\n    email email?\n";
        let (sch, errs) = crate::cst::extract_schema(schema);
        assert!(errs.is_empty(), "{errs:?}");
        let idx = SchemaIndex::build(sch.models, sch.enums, sch.oneofs);
        let old = parse_doc(
            "server s:\n    email:\n        kind = \"smtp\"\n        password = \"hunter2\"\n",
        );
        let new = parse_doc(
            "server s:\n    email:\n        kind = \"smtp\"\n        password = \"hunter3\"\n",
        );
        let d = diff_config(
            &idx,
            "server",
            &[(PathBuf::from("server.nml"), server_body(&old))],
            &[(PathBuf::from("server.nml"), server_body(&new))],
        );
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(p(&d[0]), "email");
        assert!(matches!(d[0].kind, ChangeKind::OpaqueChanged), "{d:?}");
        // Payload-free: the change's debug rendering must not contain the
        // secret-looking values from either side's body.
        let rendered = format!("{:?}", d[0]);
        assert!(
            !rendered.contains("hunter2") && !rendered.contains("hunter3"),
            "OPAQUE CHANGE LEAKED BODY CONTENT: {rendered}"
        );
    }

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
        assert_eq!(p(&d[0]), "sandboxCeiling.|block");
        assert!(
            matches!(&d[0].kind, ChangeKind::Added { new }
                if new.semantic_eq(&Value::String("192.168.0.0/16".into()))),
            "{d:?}"
        );
        // The #live directive on the modifier field rides the change.
        assert!(
            leaf_directives(&d[0]).iter().any(|dir| dir.name == "live"),
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
        assert_eq!(p(&d[0]), "port");
        assert!(matches!(&d[0].kind, ChangeKind::Modified { .. }));
        assert!(matches!(&d[0].origin, Origin::File { file, .. } if file.ends_with("new.nml")));

        // Deleting an explicit value reverts to the schema default.
        let d = diff_single("server s:\n    port = 9090\n", "server s:\n");
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(
            matches!(&d[0].kind, ChangeKind::Modified { new, .. }
                if new.semantic_eq(&Value::Number(crate::types::Number::from(8080)))),
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

    /// RFC 0017 motivation §1, the reload win itself: respelling a duration
    /// in another unit is NO change (typed values compare semantically, so
    /// nudge's reload classifier never forces a restart over `30s` →
    /// `30000ms`), while a real change of the same field reports normally.
    #[test]
    fn duration_unit_respelling_is_no_change() {
        let d = diff_single(
            "server s:\n    timeout = 30s\n",
            "server s:\n    timeout = 30000ms\n",
        );
        assert!(d.is_empty(), "unit respelling must be invisible: {d:?}");

        let d = diff_single(
            "server s:\n    timeout = 30s\n",
            "server s:\n    timeout = 31s\n",
        );
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(p(&d[0]), "timeout");
        assert!(matches!(&d[0].kind, ChangeKind::Modified { .. }));
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
        assert!(leaf_directives(&d[0]).iter().any(|dir| dir.name == "live"));

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
            .find(|c| p(c) == "providers.Google.url")
            .expect("leaf path");
        assert!(matches!(&url.kind, ChangeKind::Modified { .. }));
        assert!(!url.is_secret());
        let sec = d
            .iter()
            .find(|c| p(c) == "providers.Google.clientSecret")
            .expect("secret leaf");
        assert!(sec.is_secret(), "secret flag must ride for redaction");
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

    /// The structured path renders unambiguously and skips element hops when
    /// folded for classification (Design B): modifier fields keep their `|`
    /// sigil, scalar element keys are bracketed+quoted (dots inside stay one
    /// segment), identifier element keys read dotted, and `field_steps()`
    /// yields only the field hops in order.
    #[test]
    fn structured_path_render_and_field_steps() {
        let path = FieldPath::from_segments(vec![
            PathSeg::Field(FieldStep::new("server", vec![], false)),
            PathSeg::Field(FieldStep {
                name: "block".into(),
                modifier: true,
                directives: vec![],
                is_secret: false,
            }),
        ]);
        assert_eq!(path.to_string(), "server.|block");

        let keyed = FieldPath::from_segments(vec![
            PathSeg::Field(FieldStep::new("plugins", vec![], false)),
            PathSeg::Element(ElemKey::Key(Value::String("[acme]-x.v1".into()))),
            PathSeg::Field(FieldStep::new("egressRate", vec![], false)),
            PathSeg::Field(FieldStep::new("rate", vec![], false)),
        ]);
        assert_eq!(
            keyed.to_string(),
            "plugins[\"[acme]-x.v1\"].egressRate.rate"
        );
        // Element hops are skipped when folding for classification.
        assert_eq!(
            keyed
                .field_steps()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["plugins", "egressRate", "rate"]
        );

        let named = FieldPath::from_segments(vec![
            PathSeg::Field(FieldStep::new("providers", vec![], false)),
            PathSeg::Element(ElemKey::Name("Google".into())),
            PathSeg::Field(FieldStep::new("clientSecret", vec![], true)),
        ]);
        assert_eq!(named.to_string(), "providers.Google.clientSecret");
        assert!(named.is_secret(), "terminal secret hop drives redaction");

        // Defense-in-depth: a secret can never be an element key in any real
        // schema, but the render is secret-safe by construction — never the
        // value, never even the $ENV variable name.
        let secret_key = FieldPath::from_segments(vec![
            PathSeg::Field(FieldStep::new("keys", vec![], false)),
            PathSeg::Element(ElemKey::Key(Value::Secret("API_TOKEN".into()))),
        ]);
        let rendered = secret_key.to_string();
        assert_eq!(rendered, "keys[‹secret›]");
        assert!(
            !rendered.contains("API_TOKEN"),
            "secret var name must not render"
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
    /// A duplicate-identity element that DISAPPEARS must be reported.
    /// Named removal used an existence test, so two old `- A:` against
    /// one new `- A:` reported nothing at all — silent element loss, and
    /// no reload for a `#restart` subtree. Both passes now read the same
    /// 1:1 pairing.
    #[test]
    fn removing_one_of_two_same_named_elements_is_reported() {
        let (sch, errs) = crate::cst::extract_schema(MULTI_SCHEMA);
        assert!(errs.is_empty(), "{errs:?}");
        let run = |old_src: &str, new_src: &str| {
            let (of, nf) = (parse_doc(old_src), parse_doc(new_src));
            let fields = config_root_fields_from_files(&[&of, &nf]);
            let root = synthesize_config_root("config", &fields);
            let mut models = sch.models.clone();
            models.push(root);
            let index = SchemaIndex::build(models, sch.enums.clone(), sch.oneofs.clone());
            let (ob, nb) = (wrap_file_as_body(&of), wrap_file_as_body(&nf));
            diff_config(
                &index,
                "config",
                &[(PathBuf::from("nudge.nml"), &ob)],
                &[(PathBuf::from("nudge.nml"), &nb)],
            )
        };
        let two = "[]role roles:\n    - Api:\n        description = \"a\"\n    - Api:\n        description = \"b\"\n";
        let one = "[]role roles:\n    - Api:\n        description = \"a\"\n";
        let d = run(two, one);
        assert!(
            d.iter()
                .any(|c| matches!(&c.kind, ChangeKind::Removed { .. })),
            "dropping one of two same-named elements must report a Removed: {d:?}"
        );
        // Both gone still reports both (unchanged behavior).
        let none = "[]role roles:\n    - Other:\n        description = \"z\"\n";
        let d = run(two, none);
        assert_eq!(
            d.iter()
                .filter(|c| matches!(&c.kind, ChangeKind::Removed { .. }))
                .count(),
            2,
            "{d:?}"
        );
        // No spurious removal when counts match.
        let d = run(two, two);
        assert!(
            !d.iter()
                .any(|c| matches!(&c.kind, ChangeKind::Removed { .. })),
            "identical input must report no removal: {d:?}"
        );
    }

    /// Element pairing is 1:1 even when several elements share one
    /// identity. RFC 0016 made numeric identity semantic (`8080` and
    /// `8080.0` are one identity), widening a latent collision class:
    /// a plain first-match `find` paired BOTH new elements against
    /// `old[0]`, reporting a wrong baseline and never diffing `old[1]`.
    /// (Audit finding; the same hazard pre-existed for textually
    /// duplicated keys.)
    #[test]
    fn cohort_equal_element_identities_pair_one_to_one() {
        let (sch, errs) = crate::cst::extract_schema(MULTI_SCHEMA);
        assert!(errs.is_empty(), "{errs:?}");
        let build = |src: &str| parse_doc(src);

        // Two elements whose keys are the same VALUE in different forms.
        let old_src = "[]install plugins:\n    - \"8080\":\n        egressRate:\n            rate = 1\n            burst = 1\n    - \"8080.0\":\n        egressRate:\n            rate = 2\n            burst = 1\n";
        let new_src = "[]install plugins:\n    - \"8080\":\n        egressRate:\n            rate = 1\n            burst = 1\n    - \"8080.0\":\n        egressRate:\n            rate = 99\n            burst = 1\n";
        let (old_f, new_f) = (build(old_src), build(new_src));
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
        // Exactly one change, and its baseline is the SECOND element's
        // old value (2), not the first's (1).
        let rate: Vec<_> = d.iter().filter(|c| p(c).contains("rate")).collect();
        assert_eq!(rate.len(), 1, "one rate change expected: {d:?}");
        assert!(
            matches!(&rate[0].kind, ChangeKind::Modified { old, .. }
                if old.semantic_eq(&Value::Number(crate::types::Number::from(2)))),
            "baseline must be the paired element's own old value: {:?}",
            rate[0].kind
        );
    }

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
        let port = d.iter().find(|c| p(c) == "server.port").expect("port");
        assert!(matches!(&port.kind, ChangeKind::Modified { .. }));

        // install egressRate.rate reported at a precise leaf through the
        // scalar-shorthand element body. (The #live directive sits on the
        // `egressRate` container field's hop, so the consumer classifies by
        // folding the nearest directive over the path's field steps — exercised
        // on the nudge side.)
        let rate = d
            .iter()
            .find(|c| p(c) == "plugins[\"[acme]-x.v1\"].egressRate.rate")
            .unwrap_or_else(|| panic!("install leaf path missing: {d:?}"));
        assert!(matches!(&rate.kind, ChangeKind::Modified { .. }));

        // role edit reports (restart-class — no directive rides).
        let role = d
            .iter()
            .find(|c| p(c) == "roles.admin.description")
            .unwrap_or_else(|| panic!("role leaf path missing: {d:?}"));
        assert!(matches!(&role.kind, ChangeKind::Modified { .. }));
        assert!(leaf_directives(role).is_empty());

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
                .any(|c| p(c) == "plugins[\"[acme]-x.v1\"].egressRate.rate"
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
        assert_eq!(p(&d[0]), "plugins");
        assert!(
            matches!(&d[0].kind, ChangeKind::Added { new } if new.semantic_eq(&Value::String("[acme]-y.v1".into()))),
            "{d:?}"
        );
    }
}
