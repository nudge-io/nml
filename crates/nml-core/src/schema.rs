use std::collections::{HashMap, HashSet};

use crate::diagnostic::{Code, Diagnostic, Severity, Suggestion, codes};
use crate::model::{EnumDef, FieldDef, FieldType, MixinRef, ModelDef, ModelKind, OneOfDef};
use crate::span::Span;

/// Schema definitions (models / enums / oneofs) extracted from a source file.
/// Produced by [`crate::cst::extract`] over the CST; the validation/inheritance
/// passes in this module operate on it.
#[derive(Debug, Default, Clone)]
pub struct ExtractedSchema {
    pub models: Vec<ModelDef>,
    pub enums: Vec<EnumDef>,
    pub oneofs: Vec<OneOfDef>,
}

impl ExtractedSchema {
    /// Whether the schema contains no definitions at all.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty() && self.enums.is_empty() && self.oneofs.is_empty()
    }
}

/// Attach a definition's declaring source (when the loader stamped one, see
/// [`ModelDef::source`]) so a definition-anchored finding renders
/// `file:line:col` against the right file. Findings stay unattributed only
/// when no single definition owns them.
fn at_def(diag: Diagnostic, source: &Option<String>) -> Diagnostic {
    match source {
        Some(s) => diag.with_source(s),
        None => diag,
    }
}

/// Validate `oneof` declarations against the rest of the schema:
/// - every arm model must be a declared `model`,
/// - discriminator values must be unique within a union,
/// - a union name must not collide with a model or enum name,
/// - no arm carries a plain field named like the discriminator
///   (`shadowed_discriminator`).
///
/// Over the definitions as EXTRACTED, before [`resolve_model_inheritance`]:
/// the last rule reads each definition's own fields and walks `is` itself,
/// so the definition that declares a shadowing field is still known and
/// the row lands where the author edits.
pub fn find_oneof_errors(schema: &ExtractedSchema) -> Vec<Diagnostic> {
    let models: HashMap<&str, &ModelDef> =
        schema.models.iter().map(|m| (m.name.as_str(), m)).collect();
    let trait_names: HashSet<&str> = schema
        .models
        .iter()
        .filter(|m| m.is_trait())
        .map(|m| m.name.as_str())
        .collect();
    let enum_names: HashSet<&str> = schema.enums.iter().map(|e| e.name.as_str()).collect();
    let mut errors = Vec::new();

    for oneof in &schema.oneofs {
        // Every diagnostic for this union points at its declaration span.
        // One code per fix-pattern (RFC 0008): the code names what to do,
        // the message names the specifics.
        let err = |code: Code, message: String| {
            at_def(
                Diagnostic::error(message)
                    .with_code(code)
                    .with_span(oneof.span),
                &oneof.source,
            )
        };

        if models.contains_key(oneof.name.as_str()) || enum_names.contains(oneof.name.as_str()) {
            errors.push(err(codes::ONEOF_NAME_COLLISION, format!(
                "name '{}' is declared as both a oneof and a model/enum; names must be unique across model/enum/oneof",
                oneof.name
            )));
        }

        let mut seen_values: HashSet<&str> = HashSet::new();
        for (value, model) in &oneof.variants {
            if !seen_values.insert(value.as_str()) {
                errors.push(err(
                    codes::DUPLICATE_DISCRIMINANT,
                    format!(
                        "oneof '{}' has duplicate discriminator value \"{}\"",
                        oneof.name, value
                    ),
                ));
            }
            if trait_names.contains(model.as_str()) {
                // A trait is declared but not instantiable — a distinct fix
                // pattern from an unknown name (RFC 0011).
                errors.push(err(
                    codes::TRAIT_ONEOF_VARIANT,
                    format!(
                        "oneof '{}' arm \"{}\" targets trait '{}' — variants must be \
                         instantiable models",
                        oneof.name, value, model
                    ),
                ));
            } else if !models.contains_key(model.as_str()) {
                errors.push(err(
                    codes::ONEOF_INTEGRITY,
                    format!(
                        "oneof '{}' arm \"{}\" references unknown model '{}'",
                        oneof.name, value, model
                    ),
                ));
            }
            if let Some(arm) = models.get(model.as_str()) {
                shadowed_discriminator(&models, oneof, value, arm, &mut errors);
            }
        }

        // A declared default discriminator must name one of the arms.
        if let Some(default) = &oneof.default_discriminator {
            if !oneof.variants.iter().any(|(value, _)| value == default) {
                errors.push(err(
                    codes::ONEOF_BAD_DEFAULT,
                    format!(
                        "oneof '{}' default discriminator \"{}\" does not match any arm",
                        oneof.name, default
                    ),
                ));
            }
        }

        // An enum-typed discriminator must name a declared enum, and the arm keys
        // must *exactly* cover its variants (exhaustiveness — no missing variant and
        // no arm outside the enum).
        if let Some(type_name) = &oneof.discriminator_type {
            match schema.enums.iter().find(|e| &e.name == type_name) {
                None => errors.push(err(
                    codes::ONEOF_BAD_DISCRIMINANT_TYPE,
                    format!(
                        "oneof '{}' discriminator type '{}' is not a declared enum",
                        oneof.name, type_name
                    ),
                )),
                Some(enum_def) => {
                    let variants: HashSet<&str> =
                        enum_def.variants.iter().map(String::as_str).collect();
                    let arms: HashSet<&str> =
                        oneof.variants.iter().map(|(v, _)| v.as_str()).collect();
                    // Iterate in source order so diagnostics are deterministic.
                    for variant in &enum_def.variants {
                        if !arms.contains(variant.as_str()) {
                            errors.push(err(
                                codes::ONEOF_NOT_EXHAUSTIVE,
                                format!(
                                    "oneof '{}' is missing an arm for enum '{}' variant \"{}\"",
                                    oneof.name, type_name, variant
                                ),
                            ));
                        }
                    }
                    for (value, _) in &oneof.variants {
                        if !variants.contains(value.as_str()) {
                            errors.push(err(
                                codes::ONEOF_NOT_EXHAUSTIVE,
                                format!(
                                    "oneof '{}' arm \"{}\" is not a variant of enum '{}'",
                                    oneof.name, value, type_name
                                ),
                            ));
                        }
                    }
                }
            }
        }
    }

    errors
}

/// NML2054 for one arm. A plain field named like the union's discriminator
/// — declared on the arm model or reaching it through `is` — can never be
/// set: an instance's property of that name is always claimed AS the
/// discriminator (validation strips it before the variant checks;
/// completion suppresses the field's values, RFC 0015). Required, it makes
/// every instance unsatisfiable — a missing-field error on a property the
/// instance states; optional, it is a declaration nothing can fill. An
/// error at load, with the one spelling the language sanctions kept: an
/// OPTIONAL `#sealed` field of that name is the seal ("this oneof forbids
/// arm switching", RFC 0019) — being unsettable is its point. Modifier-form
/// fields (`|kind`) are distinct authoring and never shadow; a trait arm
/// has no instances (its own error) and is not judged.
///
/// Located where the author edits, with the exact remedy when there is
/// one: an own field at the field, its DELETION the suggestion — no
/// instance can have set it, so nothing depends on it; an own required
/// seal at the field, the `?` that makes it the sanctioned spelling the
/// suggestion; an inherited field at the arm's `is` reference that brings
/// it in, with no suggestion — the field may be live in the mixin's other
/// users, so which remedy (rename the discriminator, drop the mixin,
/// delete the field where it is declared) is the author's. Every row
/// carries the union's declaration as a note.
fn shadowed_discriminator(
    models: &HashMap<&str, &ModelDef>,
    oneof: &OneOfDef,
    value: &str,
    arm: &ModelDef,
    errors: &mut Vec<Diagnostic>,
) {
    if arm.is_trait() {
        return;
    }
    let disc = oneof.discriminator.as_str();
    let Some((origin, field, via)) = field_origin(models, arm, disc) else {
        return;
    };
    if matches!(field.field_type, FieldType::Modifier(_)) {
        return;
    }
    let sealed = field.directives.iter().any(|d| d.name == "sealed");
    if sealed && field.optional {
        return;
    }
    let site = format!(
        "oneof '{}' arm \"{value}\": model '{}'",
        oneof.name, arm.name
    );
    let claim = format!("an instance's '{disc}' property is always read as the discriminator");
    let optional_seal = format!("`{disc} {}? #sealed`", field.field_type);
    let diag = match via {
        None if sealed => Diagnostic::error(format!(
            "{site} seals the discriminator with a required field '{disc}' — {claim}, so a \
             required field can never be satisfied; declare it optional: {optional_seal}"
        ))
        .with_span(field.span)
        .with_suggestion(Suggestion::fix("?").at(Span::empty(field.type_span.end))),
        None if !field.optional => Diagnostic::error(format!(
            "{site} declares a required field '{disc}' named like the discriminator — {claim}, \
             so no instance can satisfy it (a missing-field error on a property it states); \
             delete it (to forbid arm switching instead, seal it: {optional_seal})"
        ))
        .with_span(field.span)
        .with_suggestion(Suggestion::delete().at(field.span)),
        None => Diagnostic::error(format!(
            "{site} declares a field '{disc}' named like the discriminator — {claim}, so the \
             field can never be set; delete it (to forbid arm switching instead, seal it: \
             {optional_seal})"
        ))
        .with_span(field.span)
        .with_suggestion(Suggestion::delete().at(field.span)),
        Some(mixin) => {
            let remedy = if sealed {
                format!("declare it optional where it is declared: {optional_seal}")
            } else {
                format!(
                    "rename the discriminator, drop `is {}`, or delete the field where it is \
                     declared",
                    mixin.name
                )
            };
            Diagnostic::error(format!(
                "{site} inherits a field '{disc}' named like the discriminator from {} '{}' — \
                 {claim}, so the field can never be set in this arm; {remedy}",
                origin.kind.label(),
                origin.name
            ))
            .with_span(mixin.span)
            .with_related_in(
                field.span,
                format!("field '{disc}' declared here"),
                origin.source.clone(),
            )
        }
    };
    errors.push(at_def(
        diag.with_code(codes::SHADOWED_DISCRIMINATOR)
            .with_related_in(
                oneof.span,
                format!("oneof '{}' selects its arm by '{disc}' here", oneof.name),
                oneof.source.clone(),
            ),
        &arm.source,
    ));
}

/// The definition that supplies the field `name` to `model`'s instances,
/// the field, and the `is` reference of `model` it arrives through (`None`
/// when `model` declares it): the model's own field when it declares one
/// of that name — in either form, an own name overrides every inherited
/// one — else the first `is` target, in clause order, whose own chain
/// declares it. Exactly the precedence [`resolve_model_inheritance`] gives
/// the resolved field, read off the unresolved definitions so the
/// declaring definition is still known. A definition is entered once, so
/// a cyclic chain (its own error) terminates.
fn field_origin<'a>(
    models: &HashMap<&str, &'a ModelDef>,
    model: &'a ModelDef,
    name: &str,
) -> Option<(&'a ModelDef, &'a FieldDef, Option<&'a MixinRef>)> {
    fn own<'a>(model: &'a ModelDef, name: &str) -> Option<&'a FieldDef> {
        model.fields.iter().find(|f| f.name == name)
    }
    fn walk<'a>(
        models: &HashMap<&str, &'a ModelDef>,
        model: &'a ModelDef,
        name: &str,
        seen: &mut HashSet<&'a str>,
    ) -> Option<(&'a ModelDef, &'a FieldDef)> {
        if !seen.insert(model.name.as_str()) {
            return None;
        }
        if let Some(f) = own(model, name) {
            return Some((model, f));
        }
        model.extends.iter().find_map(|parent| {
            models
                .get(parent.name.as_str())
                .and_then(|p| walk(models, p, name, seen))
        })
    }
    if let Some(f) = own(model, name) {
        return Some((model, f, None));
    }
    let mut seen: HashSet<&str> = HashSet::new();
    seen.insert(model.name.as_str());
    model.extends.iter().find_map(|parent| {
        let p = models.get(parent.name.as_str())?;
        walk(models, p, name, &mut seen).map(|(origin, f)| (origin, f, Some(parent)))
    })
}

/// Validate enum definitions: duplicate variants (both authored forms name
/// one variant — `- "a"` and `- a` are the same) and empty enums (no value
/// can satisfy a field the enum types). Both are warnings: harmless at
/// runtime, definitely unintended — and an enum is transiently empty while
/// being typed in an editor.
pub fn find_enum_errors(schema: &ExtractedSchema) -> Vec<Diagnostic> {
    let mut errors = Vec::new();
    for enum_def in &schema.enums {
        if enum_def.variants.is_empty() {
            errors.push(at_def(
                Diagnostic::warning(format!(
                    "enum '{}' declares no variants — no value can satisfy it",
                    enum_def.name
                ))
                .with_code(codes::EMPTY_ENUM)
                .with_span(enum_def.span),
                &enum_def.source,
            ));
        }
        let mut seen: HashSet<&str> = HashSet::new();
        for variant in &enum_def.variants {
            if !seen.insert(variant.as_str()) {
                errors.push(at_def(
                    Diagnostic::warning(format!(
                        "enum '{}' declares variant \"{}\" more than once",
                        enum_def.name, variant
                    ))
                    .with_code(codes::DUPLICATE_ENUM_VARIANT)
                    .with_span(enum_def.span),
                    &enum_def.source,
                ));
            }
        }
    }
    errors
}

/// Validate composition (`is`) and trait usage across the schema (RFC 0011):
/// - every `is` target must resolve to a declared model or trait (with a
///   machine-applicable did-you-mean over both when it doesn't),
/// - an `is` target must not name an enum or a `oneof`,
/// - a trait must never appear as a field's value type — traits are
///   composition-only (through lists, sets, unions, modifier types, and
///   `(K -> V)` arm positions alike).
///
/// Run **before** [`resolve_model_inheritance`]: fields are still owned by
/// the definition that wrote them, so each violation reports exactly once,
/// at its declaring model/trait.
pub fn find_composition_errors(schema: &ExtractedSchema) -> Vec<Diagnostic> {
    let defs: HashMap<&str, ModelKind> = schema
        .models
        .iter()
        .map(|m| (m.name.as_str(), m.kind))
        .collect();
    let enum_names: HashSet<&str> = schema.enums.iter().map(|e| e.name.as_str()).collect();
    let oneof_names: HashSet<&str> = schema.oneofs.iter().map(|o| o.name.as_str()).collect();
    let mut errors = Vec::new();

    for model in &schema.models {
        // Direct duplicates in one `is` clause are noise (the merge is
        // idempotent) — warn on the second occurrence, at its token.
        // Transitive diamonds (`x is a, b` where `b is a`) are the point of
        // mixins and stay silent.
        let mut listed: HashSet<&str> = HashSet::new();
        for parent in &model.extends {
            if !listed.insert(parent.name.as_str()) {
                errors.push(at_def(
                    Diagnostic::warning(format!(
                        "'{}' is listed more than once in {} '{}''s `is` clause",
                        parent.name,
                        model.kind.label(),
                        model.name,
                    ))
                    .with_code(codes::DUPLICATE_MIXIN)
                    .with_span(parent.span),
                    &model.source,
                ));
            }
        }
        for parent in &model.extends {
            if defs.contains_key(parent.name.as_str()) {
                continue;
            }
            let wrong_kind = if enum_names.contains(parent.name.as_str()) {
                Some("an enum")
            } else if oneof_names.contains(parent.name.as_str()) {
                Some("a oneof")
            } else {
                None
            };
            let diag = match wrong_kind {
                Some(kind_name) => Diagnostic::error(format!(
                    "`is` target '{}' in {} '{}' is {} — only models and traits compose",
                    parent.name,
                    model.kind.label(),
                    model.name,
                    kind_name,
                ))
                .with_code(codes::INVALID_MIXIN_KIND)
                .with_span(parent.span),
                None => {
                    let mut diag = Diagnostic::error(format!(
                        "unknown `is` target '{}' in {} '{}' — no model or trait with that name",
                        parent.name,
                        model.kind.label(),
                        model.name,
                    ))
                    .with_code(codes::UNKNOWN_MIXIN)
                    .with_span(parent.span);
                    // The candidates in DECLARATION order, from the same
                    // models `defs` indexes: the suggester breaks ties
                    // toward the earliest, so a hash-ordered vocabulary
                    // would make this hint a different name on each run.
                    let candidates = schema.models.iter().map(|m| m.name.as_str());
                    if let Some(s) = crate::suggest::suggest(&parent.name, candidates) {
                        diag = diag.with_suggestion(Suggestion::did_you_mean(s).at(parent.span));
                    }
                    diag
                }
            };
            errors.push(at_def(diag, &model.source));
        }

        for field in &model.fields {
            let mut referenced = Vec::new();
            collect_trait_refs(&field.field_type, &defs, &mut referenced);
            for trait_name in referenced {
                errors.push(at_def(
                    Diagnostic::error(format!(
                        "field '{}' of {} '{}' is typed by trait '{}' — a trait is not a \
                         value type; mix it into a model with `is` instead",
                        field.name,
                        model.kind.label(),
                        model.name,
                        trait_name,
                    ))
                    .with_code(codes::TRAIT_AS_FIELD_TYPE)
                    .with_span(field.span),
                    &model.source,
                ));
            }
        }
    }

    errors
}

/// Collect every trait name a field type references, at any nesting depth.
fn collect_trait_refs<'a>(
    ft: &'a FieldType,
    defs: &HashMap<&str, ModelKind>,
    out: &mut Vec<&'a str>,
) {
    match ft {
        FieldType::ModelRef(name) => {
            if defs.get(name.as_str()) == Some(&ModelKind::Trait) {
                out.push(name.as_str());
            }
        }
        FieldType::List(inner) | FieldType::Set(inner) | FieldType::Modifier(inner) => {
            collect_trait_refs(inner, defs, out)
        }
        FieldType::Union(parts) => {
            for part in parts {
                collect_trait_refs(part, defs, out);
            }
        }
        FieldType::Arms { key, target } => {
            collect_trait_refs(key, defs, out);
            collect_trait_refs(target, defs, out);
        }
        FieldType::Primitive { .. } => {}
    }
}

/// Each model may declare **at most one** scalar-shorthand (`!`) field: a bare
/// scalar list item supplies a single value, so it can fill only one field.
///
/// Run **after** [`resolve_model_inheritance`] so an inherited `!` and a child
/// `!` are caught together (a child cannot add a second shorthand atop a
/// parent's). RFC 0005 §8.
pub fn find_shorthand_errors(schema: &ExtractedSchema) -> Vec<Diagnostic> {
    use crate::model::FieldType;
    let mut errors = Vec::new();
    for model in &schema.models {
        // RFC 0005 positional shorthand is AXIS-aware, and the axis is fully
        // determined by the field's type — no second marker needed:
        //  * a SCALAR `+` field is filled by the list item's KEY (`- "id":`);
        //  * a LIST/SET `+` field is filled by the body's BARE list items.
        // At most one of each: two same-axis markers would be ambiguous.
        let (mut key_fill, mut body_fill): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
        for f in model.fields.iter().filter(|f| f.shorthand) {
            match f.field_type {
                FieldType::List(_) | FieldType::Set(_) => body_fill.push(f.name.as_str()),
                _ => key_fill.push(f.name.as_str()),
            }
        }
        for (axis, names, what) in [
            (
                "scalar",
                &key_fill,
                "a bare item key fills a single scalar field",
            ),
            (
                "list",
                &body_fill,
                "bare body items fill a single list field",
            ),
        ] {
            if names.len() > 1 {
                let joined = names
                    .iter()
                    .map(|n| format!("'{n}'"))
                    .collect::<Vec<_>>()
                    .join(", ");
                errors.push(at_def(
                    Diagnostic::error(format!(
                        "model '{}' declares more than one {axis} shorthand field ({joined}); {what}",
                        model.name
                    ))
                    .with_code(codes::MULTIPLE_POSITIONAL_FIELDS)
                    .with_span(model.span),
                    &model.source,
                ));
            }
        }
    }
    errors
}

/// Detect cycles in the model dependency graph.
///
/// Builds a directed graph of model-to-model edges via `FieldType::ModelRef`
/// (including through `List` and `Union` wrappers) and reports any cycles found.
pub fn find_model_cycles(schema: &ExtractedSchema) -> Vec<Diagnostic> {
    let model_names: HashSet<&str> = schema.models.iter().map(|m| m.name.as_str()).collect();

    // A field that references a `oneof` depends transitively on each of its
    // variant models, so expand those references into model-to-model edges to
    // keep cycle detection sound through unions.
    let oneof_variants: HashMap<&str, Vec<&str>> = schema
        .oneofs
        .iter()
        .map(|o| {
            let variants = o
                .variants
                .iter()
                .map(|(_, model)| model.as_str())
                .filter(|m| model_names.contains(m))
                .collect();
            (o.name.as_str(), variants)
        })
        .collect();

    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for model in &schema.models {
        let refs = collect_model_refs(&model.fields, &model_names, &oneof_variants);
        edges.insert(model.name.as_str(), refs);
    }

    let mut errors = Vec::new();
    report_graph_cycles(
        schema.models.iter().map(|m| m.name.as_str()),
        &edges,
        |cycle| {
            push_cycle_errors(
                schema,
                cycle,
                "circular dependency in model definitions",
                codes::MODEL_REFERENCE_CYCLE,
                // Advisory at the source (RFC 0008 severity-at-source): a
                // reference cycle is legal, merely suspicious.
                Severity::Warning,
                &mut errors,
            )
        },
    );
    errors
}

fn collect_model_refs<'a>(
    fields: &'a [FieldDef],
    known_models: &HashSet<&str>,
    oneof_variants: &HashMap<&'a str, Vec<&'a str>>,
) -> Vec<&'a str> {
    let mut refs = Vec::new();
    for field in fields {
        collect_refs_from_type(&field.field_type, known_models, oneof_variants, &mut refs);
    }
    refs
}

fn collect_refs_from_type<'a>(
    ft: &'a FieldType,
    known_models: &HashSet<&str>,
    oneof_variants: &HashMap<&'a str, Vec<&'a str>>,
    refs: &mut Vec<&'a str>,
) {
    match ft {
        FieldType::ModelRef(name) if known_models.contains(name.as_str()) => {
            refs.push(name.as_str());
        }
        // A reference to a `oneof` is a dependency on each of its variants.
        FieldType::ModelRef(name) => {
            if let Some(variants) = oneof_variants.get(name.as_str()) {
                refs.extend(variants.iter().copied());
            }
        }
        FieldType::List(inner) => collect_refs_from_type(inner, known_models, oneof_variants, refs),
        FieldType::Union(variants) => {
            for v in variants {
                collect_refs_from_type(v, known_models, oneof_variants, refs);
            }
        }
        _ => {}
    }
}

/// Iterative depth-first search reporting **every** cycle in a directed graph of
/// named nodes. Runs on an explicit heap stack (never the call stack), so it is
/// stack-safe at any depth — a deep chain in untrusted input can never overflow.
/// `on_cycle` fires once per back-edge with the cycle's members in order (starting
/// at the re-entered node); the caller decides how to report it.
///
/// Shared by the schema graph checks here (inheritance, model references) and the
/// membership-cycle check in `nml-validate`, so cycle detection has one home.
pub fn report_graph_cycles<'a>(
    nodes: impl IntoIterator<Item = &'a str>,
    edges: &HashMap<&'a str, Vec<&'a str>>,
    mut on_cycle: impl FnMut(&[&'a str]),
) {
    enum Work<'a> {
        Enter(&'a str),
        Exit(&'a str),
    }
    let mut done: HashSet<&str> = HashSet::new(); // fully explored (no cycles through it)
    let mut on_path: HashSet<&str> = HashSet::new(); // currently on the DFS path
    let mut path: Vec<&str> = Vec::new(); // the DFS path, ordered, for cycle reporting

    for start in nodes {
        if done.contains(start) {
            continue;
        }
        let mut stack = vec![Work::Enter(start)];
        while let Some(work) = stack.pop() {
            match work {
                Work::Enter(name) => {
                    if on_path.contains(name) {
                        // Back-edge to an ancestor → the cycle is from it to here.
                        let pos = path
                            .iter()
                            .position(|n| *n == name)
                            .expect("on_path ⇒ in path");
                        on_cycle(&path[pos..]);
                        continue;
                    }
                    if done.contains(name) {
                        continue;
                    }
                    on_path.insert(name);
                    path.push(name);
                    stack.push(Work::Exit(name));
                    if let Some(neighbors) = edges.get(name) {
                        // Reversed so neighbors are visited in source order.
                        for neighbor in neighbors.iter().rev() {
                            stack.push(Work::Enter(neighbor));
                        }
                    }
                }
                Work::Exit(name) => {
                    on_path.remove(name);
                    path.pop();
                    done.insert(name);
                }
            }
        }
    }
}

/// Emit one diagnostic per member of a detected cycle (each pointing at that
/// model's span), all describing the same loop. Shared by the inheritance and
/// model-reference cycle checks.
fn push_cycle_errors(
    schema: &ExtractedSchema,
    cycle: &[&str],
    kind: &str,
    code: Code,
    severity: Severity,
    errors: &mut Vec<Diagnostic>,
) {
    let desc = cycle
        .iter()
        .chain(std::iter::once(&cycle[0]))
        .copied()
        .collect::<Vec<_>>()
        .join(" -> ");
    for &member in cycle {
        let member_def = schema.models.iter().find(|m| m.name == member);
        let span = member_def
            .map(|m| m.span)
            .unwrap_or_else(|| crate::span::Span::empty(0));
        let mut diag = Diagnostic::error(format!("{kind}: {desc}"))
            .with_code(code)
            .with_span(span);
        diag.severity = severity.clone();
        errors.push(at_def(diag, member_def.map_or(&None, |m| &m.source)));
    }
}

/// Resolve parent model fields into child models via the `extends` relation:
/// each model's `fields` becomes the full set inherited from its ancestors
/// (ancestor-first, parents left-to-right, first occurrence of a name winning)
/// followed by its own fields, which override any inherited name.
///
/// Each model is resolved **once**, in dependency order, and reused by its
/// descendants — so a shared base (or a deep ancestor subtree) is collected a
/// single time. The work is `O(models + edges + total resolved fields)`, optimal
/// for this flattened representation (the field total is the output size, an
/// inherent lower bound). The traversal is an iterative work-stack, so it is
/// stack-safe at any depth (untrusted schema files reach this via `load_schema`),
/// and the `InProgress` colour breaks inheritance cycles (reported separately by
/// [`find_extends_cycles`]) so resolution always terminates.
pub fn resolve_model_inheritance(schema: &mut ExtractedSchema) {
    // Owned keys so the index does not borrow `schema.models` — leaving it free
    // to mutate when writing the resolved fields back at the end.
    let index: HashMap<String, usize> = schema
        .models
        .iter()
        .enumerate()
        .map(|(i, m)| (m.name.clone(), i))
        .collect();

    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        Unvisited,
        InProgress,
        Done,
    }
    enum Work {
        Enter(usize),
        Build(usize),
    }

    let n = schema.models.len();
    let mut color = vec![Color::Unvisited; n];
    let mut resolved: Vec<Vec<FieldDef>> = Vec::with_capacity(n);
    resolved.resize_with(n, Vec::new);

    for start in 0..n {
        if color[start] != Color::Unvisited {
            continue;
        }
        let mut stack = vec![Work::Enter(start)];
        while let Some(work) = stack.pop() {
            match work {
                // Discover a model: schedule its build, then push its parents on
                // top so they (and their ancestors) resolve first — post-order.
                Work::Enter(i) => {
                    if color[i] != Color::Unvisited {
                        continue;
                    }
                    color[i] = Color::InProgress;
                    stack.push(Work::Build(i));
                    for parent in &schema.models[i].extends {
                        if let Some(&p) = index.get(parent.name.as_str()) {
                            if color[p] == Color::Unvisited {
                                stack.push(Work::Enter(p));
                            }
                        }
                    }
                }
                // Parents are resolved (or were cycle-broken → empty): merge their
                // resolved fields ancestor-first (own names pre-claimed so they
                // override), then append this model's own fields.
                Work::Build(i) => {
                    let mut seen: HashSet<String> = schema.models[i]
                        .fields
                        .iter()
                        .map(|f| f.name.clone())
                        .collect();
                    let mut fields = Vec::new();
                    for parent in &schema.models[i].extends {
                        if let Some(&p) = index.get(parent.name.as_str()) {
                            for field in &resolved[p] {
                                if seen.insert(field.name.clone()) {
                                    fields.push(field.clone());
                                }
                            }
                        }
                    }
                    fields.extend(schema.models[i].fields.iter().cloned());
                    resolved[i] = fields;
                    color[i] = Color::Done;
                }
            }
        }
    }

    for (model, fields) in schema.models.iter_mut().zip(resolved) {
        model.fields = fields;
    }
}

/// Detect cycles in the model `extends` (inheritance) graph.
///
/// Returns one error per model participating in a cycle.
pub fn find_extends_cycles(schema: &ExtractedSchema) -> Vec<Diagnostic> {
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for model in &schema.models {
        edges.insert(
            model.name.as_str(),
            model.extends.iter().map(|s| s.name.as_str()).collect(),
        );
    }

    let mut errors = Vec::new();
    report_graph_cycles(
        schema.models.iter().map(|m| m.name.as_str()),
        &edges,
        |cycle| {
            push_cycle_errors(
                schema,
                cycle,
                "circular inheritance in model definitions",
                codes::EXTENDS_CYCLE,
                Severity::Error,
                &mut errors,
            )
        },
    );
    errors
}

/// Every span the extracted schema carries — definitions, mixin references,
/// fields (whole and type), facet bounds, defaults and directives — each
/// named by the field that holds it; the schema-side twin of
/// [`crate::ast::for_each_span`].
pub fn for_each_span(schema: &ExtractedSchema, f: &mut impl FnMut(crate::span::SpanSite)) {
    for m in &schema.models {
        site(f, "ModelDef", m.span);
        for e in &m.extends {
            site(f, "MixinRef", e.span);
        }
        for field in &m.fields {
            site(f, "FieldDef", field.span);
            site(f, "FieldDef.type_span", field.type_span);
            field_type_spans(f, &field.field_type);
            if let Some(d) = &field.default_value {
                crate::ast::value_spans(f, "FieldDef.default_value", d);
            }
            for d in &field.directives {
                crate::ast::directive_spans(f, d);
            }
        }
    }
    for e in &schema.enums {
        site(f, "EnumDef", e.span);
    }
    for o in &schema.oneofs {
        site(f, "OneOfDef", o.span);
    }
}

fn site(f: &mut impl FnMut(crate::span::SpanSite), kind: &'static str, span: crate::span::Span) {
    // Every schema site is a whole node's or a whole token's span; the two
    // sub-token shapes reach a schema only through `crate::ast::value_spans`.
    f(crate::span::SpanSite::aligned(kind, span));
}

fn field_type_spans(f: &mut impl FnMut(crate::span::SpanSite), t: &crate::model::FieldType) {
    use crate::model::{FieldType, PrimitiveFacets};
    match t {
        FieldType::Primitive { facets, .. } => match facets {
            PrimitiveFacets::Number(facets) => facet_spans(f, facets),
            PrimitiveFacets::Duration(facets) => facet_spans(f, facets),
            PrimitiveFacets::None => {}
        },
        FieldType::List(inner) | FieldType::Modifier(inner) | FieldType::Set(inner) => {
            field_type_spans(f, inner)
        }
        FieldType::Union(variants) => {
            for v in variants {
                field_type_spans(f, v);
            }
        }
        FieldType::Arms { key, target } => {
            field_type_spans(f, key);
            field_type_spans(f, target);
        }
        FieldType::ModelRef(_) => {}
    }
}

fn facet_spans<T>(f: &mut impl FnMut(crate::span::SpanSite), facets: &crate::model::Facets<T>) {
    if let Some(b) = &facets.min {
        site(f, "FacetBound.min", b.span);
    }
    if let Some(b) = &facets.max {
        site(f, "FacetBound.max", b.span);
    }
    if let Some(m) = &facets.multiple_of {
        site(f, "FacetMultipleOf", m.span);
    }
}

#[cfg(test)]
mod tests {

    /// A facet-violating default inside a UNION must be reported.
    /// The regression this guards: variants that cannot hold the value
    /// (a `string` beside a faceted `number`) have no facets to check,
    /// so treating their silence as admission made every faceted-union
    /// default unreportable on every surface.
    #[test]
    fn union_defaults_are_measured_against_applicable_variants_only() {
        let reports = |src: &str| {
            crate::cst::extract_schema(src)
                .1
                .iter()
                .filter(|d| d.code == Some(crate::diagnostic::codes::FACET_VIOLATION))
                .count()
        };
        for (src, want) in [
            // Non-numeric variants must not vacuously admit, either order.
            (
                "model m:\n    port (number(min = 1, max = 65535) | string) = 0\n",
                1,
            ),
            ("model m:\n    x (string | number(min = 10)) = 5\n", 1),
            ("model m:\n    x (number(min = 10) | bool) = 5\n", 1),
            // Collections report ELEMENT-WISE, like enforcement.
            (
                "model m:\n    x [](number(min = 10) | string) = [5, 3]\n",
                2,
            ),
            ("model m:\n    x set<number(min = 10) | string> = [5]\n", 1),
            // Traits get the same treatment as models.
            ("trait t:\n    x (number(min = 10) | string) = 5\n", 1),
            // Control: all-numeric union, every band rejecting.
            (
                "model m:\n    x (number(min = 10) | number(max = 0)) = 5\n",
                1,
            ),
        ] {
            assert_eq!(reports(src), want, "wrong NML2057 count for {src:?}");
        }
        // A variant that genuinely admits keeps it clean — including
        // the value landing on the NON-numeric side.
        for src in [
            "model m:\n    x (number(min = 10) | string) = \"free\"\n",
            "model m:\n    x (number(min = 10) | number(max = 0)) = -5\n",
            "model m:\n    x (number(min = 10) | string) = 50\n",
        ] {
            assert_eq!(reports(src), 0, "must stay clean: {src:?}");
        }
    }

    /// Facets attach to `number` and `duration` ONLY; every other
    /// primitive refuses with a message naming the actual type — the
    /// author needs to know WHICH type refused. A `duration` field
    /// with a UNITLESS bound is the domain-agreement error instead
    /// (durations became legal carriers when RFC 0018 §3's deferral
    /// closed): the message teaches the literal shape.
    #[test]
    fn facets_on_other_primitives_name_the_type() {
        for ty in ["string", "bool", "path", "secret", "money"] {
            let src = format!("model m:\n    x {ty}(min = 1)\n");
            let (_s, diags) = crate::cst::extract_schema(&src);
            let msg = diags
                .iter()
                .find(|d| d.code == Some(crate::diagnostic::codes::FACET_DEFINITION))
                .map(|d| d.rendered_message())
                .unwrap_or_else(|| panic!("{ty} facets must be rejected: {diags:?}"));
            assert!(
                msg.contains(&format!("`{ty}` cannot carry them")),
                "message must name the type: {msg}"
            );
        }
        let (_s, diags) = crate::cst::extract_schema("model m:\n    x duration(min = 1)\n");
        let msg = diags
            .iter()
            .find(|d| d.code == Some(crate::diagnostic::codes::FACET_DEFINITION))
            .map(|d| d.rendered_message())
            .unwrap_or_else(|| panic!("unitless duration bound must be rejected: {diags:?}"));
        assert!(
            msg.contains("has no unit") && msg.contains("min = 1s"),
            "message must teach the duration-literal shape: {msg}"
        );
    }

    /// Duration facets (RFC 0017 literals under the RFC 0018 grammar):
    /// a well-formed declaration loads clean; the declaration rules
    /// judge ranges SEMANTICALLY (nanos), so `min = 1000ms` against
    /// `max = 1s` is satisfiable (equal) while the exclusive spelling
    /// of the same pair is not; `multipleOf = 0s` is rejected like its
    /// numeric zero sibling; and a violating duration DEFAULT is
    /// NML2057 at the declaration, mixed units included.
    #[test]
    fn duration_facet_declarations_and_defaults() {
        let clean = "model m:\n    t duration(min = 1s, max = 2h, multipleOf = 250ms) = 1500ms\n";
        let (_s, diags) = crate::cst::extract_schema(clean);
        assert!(diags.is_empty(), "well-formed must load clean: {diags:?}");

        // Compound literals (RFC 0017 §10) work in facet position too,
        // and the bounds still judge semantically: the default 90m
        // satisfies min = 1h30m because they are one value.
        let compound = "model m:\n    t duration(min = 1h30m, max = 2h) = 90m\n";
        let (s, diags) = crate::cst::extract_schema(compound);
        assert!(
            diags.is_empty(),
            "compound facet must load clean: {diags:?}"
        );
        let field = s.models[0]
            .fields
            .iter()
            .find(|f| f.name == "t")
            .expect("field t");
        let crate::model::FieldType::Primitive {
            facets: crate::model::PrimitiveFacets::Duration(fs),
            ..
        } = &field.field_type
        else {
            panic!("duration facets expected: {:?}", field.field_type);
        };
        assert_eq!(
            fs.min.as_ref().map(|m| m.value.to_string()).as_deref(),
            Some("1h30m")
        );

        // REGRESSION: a dangling magnitude in facet position (`min =
        // 1h30`) once loaded silently as the truncated bound `1h` — it
        // must be NML3008, exactly as in value position.
        let (_s, diags) = crate::cst::extract_schema("model m:\n    t duration(min = 1h30)\n");
        assert!(
            diags
                .iter()
                .any(|d| d.code.is_some_and(|c| c.to_string() == "NML3008")),
            "dangling facet magnitude must be NML3008, never a truncated bound: {diags:?}"
        );

        let equal = "model m:\n    t duration(min = 1000ms, max = 1s)\n";
        let (_s, diags) = crate::cst::extract_schema(equal);
        assert!(diags.is_empty(), "1000ms == 1s is satisfiable: {diags:?}");

        let strict = "model m:\n    t duration(exclusiveMin = 1000ms, max = 1s)\n";
        let (_s, diags) = crate::cst::extract_schema(strict);
        assert!(
            diags.iter().any(|d| d.message.contains("unsatisfiable")),
            "exclusive equal bounds are empty: {diags:?}"
        );

        // Plain cross-unit inversion — the everyday authoring mistake.
        let inverted = "model m:\n    t duration(min = 2h, max = 90s)\n";
        let (_s, diags) = crate::cst::extract_schema(inverted);
        assert!(
            diags.iter().any(|d| d.message.contains("unsatisfiable")),
            "2h > 90s must be unsatisfiable: {diags:?}"
        );

        // Discrete-domain empty range: both bounds exclusive, one
        // nanosecond apart — lo < hi, yet NO representable value exists
        // between them. Numbers are dense and cannot hit this; loading
        // clean here would break the file's own invariant (an
        // unsatisfiable range must never load clean and then reject
        // every value).
        let adjacent = "model m:\n    t duration(exclusiveMin = 0s, exclusiveMax = 1ns)\n";
        let (_s, diags) = crate::cst::extract_schema(adjacent);
        assert!(
            diags.iter().any(|d| d.message.contains("unsatisfiable")),
            "adjacent exclusive nanos admit nothing: {diags:?}"
        );

        // One nanosecond of daylight: exclusive bounds 2ns apart admit
        // exactly 1ns — satisfiable, must load clean.
        let daylight = "model m:\n    t duration(exclusiveMin = 0s, exclusiveMax = 2ns)\n";
        let (_s, diags) = crate::cst::extract_schema(daylight);
        assert!(
            diags.is_empty(),
            "a 2ns exclusive gap admits 1ns: {diags:?}"
        );

        let zero = "model m:\n    t duration(multipleOf = 0s)\n";
        let (_s, diags) = crate::cst::extract_schema(zero);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("positive duration")),
            "multipleOf = 0s must be rejected: {diags:?}"
        );

        let bad_default = "model m:\n    t duration(min = 1s) = 500ms\n";
        let (_s, diags) = crate::cst::extract_schema(bad_default);
        assert!(
            diags.iter().any(
                |d| d.code == Some(crate::diagnostic::codes::FACET_VIOLATION)
                    && d.message.contains("below")
            ),
            "500ms breaks min = 1s at the declaration: {diags:?}"
        );

        // Cross-domain, both directions: each names its fix.
        let (_s, diags) = crate::cst::extract_schema("model m:\n    x number(min = 5s)\n");
        assert!(
            diags.iter().any(|d| d.message.contains("is a duration")),
            "duration bound on a number field: {diags:?}"
        );
    }

    /// The cross-domain teaching message must never hand out an illegal
    /// literal: duration magnitudes are non-negative integers, so a
    /// fractional or negative `n` gets the generic worked example
    /// (`5s`/`500ms`), not `1.5s`/`-5s` — spellings the parser itself
    /// rejects (NML3005/NML3006).
    #[test]
    fn cross_domain_teaching_example_is_always_legal() {
        for bad in ["1.5", "-5"] {
            let src = format!("model m:\n    t duration(min = {bad})\n");
            let (_s, diags) = crate::cst::extract_schema(&src);
            let msg = diags
                .iter()
                .find(|d| d.message.contains("has no unit"))
                .map(|d| d.rendered_message())
                .unwrap_or_else(|| panic!("min = {bad} must be cross-domain: {diags:?}"));
            assert!(
                msg.contains("min = 5s") && !msg.contains(&format!("{bad}s")),
                "example must be a legal literal for {bad}: {msg}"
            );
        }
    }

    /// A missing facet comma (`min = 5 max = 10`) must not eat the next
    /// key as a duration unit — the lead diagnostic would point at
    /// "unknown unit `max`" instead of the real defect (the separator).
    #[test]
    fn missing_facet_comma_is_not_an_unknown_unit() {
        let (_s, diags) = crate::cst::extract_schema("model m:\n    x number(min = 5 max = 10)\n");
        assert!(
            !diags.is_empty(),
            "a malformed facet list must not load clean"
        );
        assert!(
            diags.iter().all(|d| !d.message.contains("unknown unit")),
            "`max` is the next key, not a unit: {diags:?}"
        );
    }

    /// RFC 0016 makes −0 unrepresentable, so every spelling of a
    /// zero bound IS zero — `min = -0.0` must normalize, never error,
    /// and must behave identically to `min = 0`.
    #[test]
    fn negative_zero_facet_spellings_normalize() {
        for spelling in ["0", "-0", "0.0", "-0.0"] {
            let src = format!("model m:\n    x number(min = {spelling})\n");
            let (_s, diags) = crate::cst::extract_schema(&src);
            assert!(
                diags.is_empty(),
                "min = {spelling} must load clean: {diags:?}"
            );
        }
    }

    /// RFC 0018 §1.1 claims facets attach to MODIFIER fields, and
    /// enforcement honors them (extraction lowers `|cap number(...)` to
    /// `FieldType::Modifier(inner)`; validation recurses through it).
    /// The declaration rules must reach them too — otherwise an
    /// unsatisfiable range on a modifier loads clean and then rejects
    /// every value with contradictory violations, the exact trap §1.2
    /// promises cannot exist.
    #[test]
    fn facet_rules_reach_typed_modifiers() {
        let src = "model m:\n    |allow string(min = 1)\n    |cap number(min = 2, max = 1)\n";
        let (_schema, diags) = crate::cst::extract_schema(src);
        let msgs: Vec<String> = diags
            .iter()
            .filter(|d| d.code == Some(crate::diagnostic::codes::FACET_DEFINITION))
            .map(|d| d.rendered_message())
            .collect();
        assert_eq!(
            msgs.len(),
            2,
            "typed modifiers must face the rules: {diags:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("facets attach only to `number`")),
            "{msgs:?}"
        );
        assert!(msgs.iter().any(|m| m.contains("unsatisfiable")), "{msgs:?}");
        // A well-formed faceted modifier stays clean.
        let (_s, ok) = crate::cst::extract_schema("model m:\n    |cap number(min = 1)\n");
        assert!(
            ok.iter()
                .all(|d| d.code != Some(crate::diagnostic::codes::FACET_DEFINITION)),
            "{ok:?}"
        );
    }

    use crate::model::PrimitiveFacets;

    use super::*;
    use crate::cst::extract_schema;
    use crate::model::MixinRef;
    use crate::types::PrimitiveType;

    fn extract_src(src: &str) -> ExtractedSchema {
        extract_schema(src).0
    }

    #[test]
    fn at_most_one_shorthand_field_per_model() {
        // A single `!` field (alongside a non-shorthand `name`) is fine.
        let ok = extract_src("model r:\n    name string\n    path path+\n");
        assert!(find_shorthand_errors(&ok).is_empty());

        // Two `!` fields is a schema error naming both.
        let bad = extract_src("model r:\n    a string+\n    b path+\n");
        let errs = find_shorthand_errors(&bad);
        assert_eq!(errs.len(), 1);
        let message = &errs[0].message;
        assert!(message.contains("'a'"), "{message}");
        assert!(message.contains("'b'"), "{message}");
        assert!(message.contains("shorthand"), "{message}");
        assert_eq!(errs[0].code, Some(codes::MULTIPLE_POSITIONAL_FIELDS));
    }

    #[test]
    fn deep_inheritance_chain_does_not_overflow_stack() {
        // A linear `is` chain far deeper than any call-stack limit. Fieldless
        // models keep the flattened output O(depth) — a chain *with* fields is
        // inherently O(n²) to flatten (the output size) — so this isolates the
        // traversal: the iterative resolver runs on the heap and cannot overflow
        // at any depth. The previous recursive resolver crashed here.
        const DEPTH: usize = 200_000;
        let models: Vec<ModelDef> = (0..DEPTH)
            .map(|i| ModelDef {
                kind: ModelKind::Model,
                source: None,
                name: format!("m{i}"),
                extends: if i + 1 < DEPTH {
                    vec![MixinRef::synthetic(format!("m{}", i + 1))]
                } else {
                    vec![]
                },
                fields: vec![],
                span: crate::span::Span::empty(0),
            })
            .collect();
        let mut schema = ExtractedSchema {
            models,
            enums: vec![],
            oneofs: vec![],
        };
        resolve_model_inheritance(&mut schema); // must not overflow
        assert!(schema.models.iter().all(|m| m.fields.is_empty()));
    }

    #[test]
    fn deep_chain_cycle_detection_does_not_overflow_stack() {
        // The cycle detectors are also reached via `load_schema` on untrusted
        // schema files. A deep *acyclic* chain would overflow a recursive DFS; the
        // iterative `report_graph_cycles` runs on the heap. No cycle ⇒ no errors.
        const DEPTH: usize = 200_000;
        let models: Vec<ModelDef> = (0..DEPTH)
            .map(|i| ModelDef {
                kind: ModelKind::Model,
                source: None,
                name: format!("m{i}"),
                extends: if i + 1 < DEPTH {
                    vec![MixinRef::synthetic(format!("m{}", i + 1))]
                } else {
                    vec![]
                },
                fields: vec![],
                span: crate::span::Span::empty(0),
            })
            .collect();
        let schema = ExtractedSchema {
            models,
            enums: vec![],
            oneofs: vec![],
        };
        assert!(find_extends_cycles(&schema).is_empty()); // must not overflow
    }

    #[test]
    fn extends_cycle_detected_and_reported() {
        // Correctness of the iterative detector: a→b→c→a is found, with one
        // diagnostic per member (each pointing at that model).
        let model = |name: &str, parent: &str| ModelDef {
            kind: ModelKind::Model,
            source: None,
            name: name.to_string(),
            extends: vec![MixinRef::synthetic(parent)],
            fields: vec![],
            span: crate::span::Span::empty(0),
        };
        let schema = ExtractedSchema {
            models: vec![model("a", "b"), model("b", "c"), model("c", "a")],
            enums: vec![],
            oneofs: vec![],
        };
        let errors = find_extends_cycles(&schema);
        assert_eq!(errors.len(), 3, "one diagnostic per cycle member");
        assert!(
            errors
                .iter()
                .all(|e| e.message.contains("circular inheritance"))
        );
    }

    #[test]
    fn inheritance_cycle_resolves_without_hang_or_panic() {
        // `a is b` / `b is a` — the cycle is reported elsewhere (find_extends_cycles);
        // resolution must still terminate (no hang, no panic) on a best-effort basis,
        // each model at minimum retaining its own field.
        let model = |name: &str, parent: &str, f: &str| ModelDef {
            kind: ModelKind::Model,
            source: None,
            name: name.to_string(),
            extends: vec![MixinRef::synthetic(parent)],
            fields: vec![FieldDef {
                name: f.to_string(),
                field_type: FieldType::Primitive {
                    ty: PrimitiveType::String,
                    facets: PrimitiveFacets::None,
                },
                optional: false,
                shorthand: false,
                default_value: None,
                directives: Vec::new(),
                doc: None,
                span: crate::span::Span::empty(0),
                type_span: crate::span::Span::empty(0),
            }],
            span: crate::span::Span::empty(0),
        };
        let mut schema = ExtractedSchema {
            models: vec![model("a", "b", "fa"), model("b", "a", "fb")],
            enums: vec![],
            oneofs: vec![],
        };
        resolve_model_inheritance(&mut schema); // must not hang or panic
        assert!(
            schema
                .models
                .iter()
                .all(|m| m.fields.iter().any(|f| f.name.starts_with('f')))
        );
    }

    #[test]
    fn inheritance_resolves_diamond_once_in_order() {
        // Diamond: D ⟶ {B, C} ⟶ A. The shared base A is resolved a single time;
        // fields appear ancestor-first with the first occurrence winning, child
        // fields last. Exercises the memoized merge across a re-converging DAG.
        let field = |name: &str| FieldDef {
            name: name.to_string(),
            field_type: FieldType::Primitive {
                ty: PrimitiveType::String,
                facets: PrimitiveFacets::None,
            },
            optional: false,
            shorthand: false,
            default_value: None,
            directives: Vec::new(),
            doc: None,
            span: crate::span::Span::empty(0),
            type_span: crate::span::Span::empty(0),
        };
        let model = |name: &str, extends: &[&str], f: &str| ModelDef {
            kind: ModelKind::Model,
            source: None,
            name: name.to_string(),
            extends: extends.iter().map(|s| MixinRef::synthetic(*s)).collect(),
            fields: vec![field(f)],
            span: crate::span::Span::empty(0),
        };
        let mut schema = ExtractedSchema {
            models: vec![
                model("A", &[], "a"),
                model("B", &["A"], "b"),
                model("C", &["A"], "c"),
                model("D", &["B", "C"], "d"),
            ],
            enums: vec![],
            oneofs: vec![],
        };
        resolve_model_inheritance(&mut schema);
        let names = |name: &str| -> Vec<String> {
            schema
                .models
                .iter()
                .find(|m| m.name == name)
                .unwrap()
                .fields
                .iter()
                .map(|f| f.name.clone())
                .collect()
        };
        // A's `a` appears exactly once (via B), not duplicated through C.
        assert_eq!(names("D"), vec!["a", "b", "c", "d"]);
        assert_eq!(names("B"), vec!["a", "b"]);
    }

    #[test]
    fn test_extract_oneof() {
        let schema = extract_src(
            "model emailLog:\n    fromAddress string?\n\nmodel emailPostmark:\n    serverToken secret\n\noneof email by provider:\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n",
        );
        assert_eq!(schema.oneofs.len(), 1);
        let o = &schema.oneofs[0];
        assert_eq!(o.name, "email");
        assert_eq!(o.discriminator, "provider");
        assert_eq!(
            o.variants,
            vec![
                ("log".to_string(), "emailLog".to_string()),
                ("postmark".to_string(), "emailPostmark".to_string()),
            ]
        );
        assert!(find_oneof_errors(&schema).is_empty());
    }

    #[test]
    fn test_oneof_unknown_arm_model_rejected() {
        let schema = extract_src(
            "model emailLog:\n    x string?\n\noneof email by provider:\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n",
        );
        let errs = find_oneof_errors(&schema);
        assert!(
            errs.iter().any(|e| e.message.contains("emailPostmark")),
            "expected unknown-arm-model error; got {errs:?}"
        );
    }

    #[test]
    fn test_oneof_duplicate_value_rejected() {
        let schema = extract_src(
            "model a:\n    x string?\n\nmodel b:\n    y string?\n\noneof u by kind:\n    \"k\" -> a\n    \"k\" -> b\n",
        );
        let errs = find_oneof_errors(&schema);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("duplicate discriminator value")),
            "expected duplicate-value error; got {errs:?}"
        );
    }

    /// The NML2054 rows of `src`, through the loader's call order.
    fn shadow_rows(src: &str) -> Vec<Diagnostic> {
        find_oneof_errors(&extract_src(src))
            .into_iter()
            .filter(|e| e.code == Some(codes::SHADOWED_DISCRIMINATOR))
            .collect()
    }

    fn span_of(src: &str, needle: &str) -> Span {
        token_at(src, needle, needle.len())
    }

    /// The `len`-byte span starting where `needle` first occurs.
    fn token_at(src: &str, needle: &str, len: usize) -> Span {
        let start = src.find(needle).expect("needle in source");
        Span::new(start, start + len)
    }

    #[test]
    fn a_plain_field_named_like_the_discriminator_is_an_error_at_the_field_with_its_deletion() {
        // Optional or required, sealed or not: an own plain field of the
        // discriminator's name is an ERROR located at the field's content
        // (not the indentation), carrying its deletion as the one
        // suggestion and the union's declaration as a note.
        for decl in ["kind string?", "kind string", "kind (va | vb)"] {
            let src = format!(
                "model va:\n    p string\n\nmodel vb:\n    q string\n\nmodel logM:\n    \
                 // the entry's kind\n    {decl}\n    msg string?\n\noneof mail by kind:\n    \
                 \"log\" -> logM\n"
            );
            let rows = shadow_rows(&src);
            assert_eq!(rows.len(), 1, "{decl}: {rows:?}");
            let row = &rows[0];
            assert!(matches!(row.severity, Severity::Error), "{decl}: {row:?}");
            assert_eq!(row.span, Some(span_of(&src, decl)), "{decl}: at the field");
            // The honest consequence per shape: an optional field is a
            // declaration nothing can fill; a required one fails every
            // instance on a property it states.
            let expected = if decl == "kind string?" {
                "oneof 'mail' arm \"log\": model 'logM' declares a field 'kind' named like \
                 the discriminator — an instance's 'kind' property is always read as the \
                 discriminator, so the field can never be set; delete it"
            } else {
                "oneof 'mail' arm \"log\": model 'logM' declares a required field 'kind' named \
                 like the discriminator — an instance's 'kind' property is always read as the \
                 discriminator, so no instance can satisfy it (a missing-field error on a \
                 property it states); delete it"
            };
            assert!(row.message.contains(expected), "{decl}: {}", row.message);
            assert_eq!(
                row.suggestions,
                vec![Suggestion::delete().at(span_of(&src, decl))],
                "{decl}: the deletion, anchored at the field"
            );
            assert_eq!(row.related.len(), 1, "{decl}: the union's note");
            assert_eq!(
                row.related[0].span.start,
                src.find("oneof mail").expect("the union"),
                "{decl}: the note at the union's declaration"
            );
            assert_eq!(
                row.related[0].message,
                "oneof 'mail' selects its arm by 'kind' here"
            );
        }
        // The seal is spelled in the message with the field's own type.
        let row = &shadow_rows(
            "model logM:\n    kind (va | vb)\n\nmodel va:\n    p string\n\nmodel vb:\n    \
             q string\n\noneof mail by kind:\n    \"log\" -> logM\n",
        )[0];
        assert!(
            row.message.ends_with("seal it: `kind (va | vb)? #sealed`)"),
            "{}",
            row.message
        );
    }

    #[test]
    fn the_modifier_form_and_the_optional_seal_never_shadow() {
        // The modifier form (`|kind`) is distinct authoring; an OPTIONAL
        // `#sealed` field of the discriminator's name is RFC 0019's
        // no-arm-switching spelling — being unsettable is its point.
        for src in [
            "model logM:\n    |kind string?\n\noneof mail by kind:\n    \"log\" -> logM\n",
            "model logM:\n    kind string? #sealed\n\noneof mail by kind:\n    \"log\" -> logM\n",
            // Inherited, the seal is the same spelling (a trait sealing
            // every arm it is mixed into).
            "trait sealed:\n    kind string? #sealed\n\nmodel logM is sealed:\n    m string?\n\n\
             oneof mail by kind:\n    \"log\" -> logM\n",
            // An own modifier form shadows an inherited plain field — the
            // arm's resolved field IS the modifier, which never shadows.
            "trait t:\n    kind string?\n\nmodel logM is t:\n    |kind string?\n\noneof mail by \
             kind:\n    \"log\" -> logM\n",
            // No field of the name anywhere: nothing to judge.
            "model logM:\n    msg string?\n\noneof mail by kind:\n    \"log\" -> logM\n",
        ] {
            assert!(shadow_rows(src).is_empty(), "{src}");
        }
    }

    #[test]
    fn a_required_seal_is_refused_with_the_optional_spelling_as_its_fix() {
        // `kind string #sealed` — required — is the unsatisfiable shape
        // wearing the seal's directive: an error at the field, the `?`
        // that makes it the sanctioned spelling as the one suggestion,
        // inserted at the type's end — after its facets, before a default or a
        // directive (the field's type is immaterial: it is never set).
        let src = "model logM:\n    kind number(min = 1) #sealed\n\noneof mail by kind:\n    \
                   \"log\" -> logM\n";
        let rows = shadow_rows(src);
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert!(matches!(row.severity, Severity::Error), "{row:?}");
        assert_eq!(row.span, Some(span_of(src, "kind number(min = 1) #sealed")));
        assert_eq!(
            row.message,
            "oneof 'mail' arm \"log\": model 'logM' seals the discriminator with a required \
             field 'kind' — an instance's 'kind' property is always read as the \
             discriminator, so a required field can never be satisfied; declare it \
             optional: `kind number(min = 1)? #sealed`"
        );
        let after_type = span_of(src, "number(min = 1)").end;
        assert_eq!(
            row.suggestions,
            vec![Suggestion::fix("?").at(Span::empty(after_type))],
            "the `?` at the type's end"
        );
        assert_eq!(row.related.len(), 1, "the union's note rides every row");
    }

    #[test]
    fn an_inherited_field_named_like_the_discriminator_is_located_at_the_mixin_with_no_fix() {
        // The field reaches the arm through `is`: the row is at the arm's
        // reference that brings it in, names the declaring definition,
        // carries NO suggestion (the field may be live in the mixin's other
        // users) and notes the field and the union.
        let src = "trait tagged:\n    kind string?\n\nmodel logM is tagged:\n    msg string?\n\n\
                   model note is tagged:\n    text string?\n\noneof mail by kind:\n    \
                   \"log\" -> logM\n";
        let rows = shadow_rows(src);
        assert_eq!(rows.len(), 1, "one row, for the arm alone: {rows:?}");
        let row = &rows[0];
        assert_eq!(
            row.span,
            Some(token_at(src, "tagged:\n    msg", 6)),
            "at `is tagged`"
        );
        assert_eq!(
            row.message,
            "oneof 'mail' arm \"log\": model 'logM' inherits a field 'kind' named like the \
             discriminator from trait 'tagged' — an instance's 'kind' property is always \
             read as the discriminator, so the field can never be set in this arm; rename \
             the discriminator, drop `is tagged`, or delete the field where it is declared"
        );
        assert!(row.suggestions.is_empty(), "{row:?}");
        assert_eq!(row.related.len(), 2);
        assert_eq!(row.related[0].span, span_of(src, "kind string?"));
        assert_eq!(row.related[0].message, "field 'kind' declared here");
        assert_eq!(
            row.related[1].message,
            "oneof 'mail' selects its arm by 'kind' here"
        );

        // Through a base MODEL, two levels deep: the arm's DIRECT reference
        // is the site, the declaring definition is named.
        let src = "model root:\n    kind string\n\nmodel base is root:\n    x string?\n\n\
                   model logM is base:\n    msg string?\n\noneof mail by kind:\n    \
                   \"log\" -> logM\n";
        let rows = shadow_rows(src);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            rows[0].span,
            Some(token_at(src, "base:\n    msg", 4)),
            "at `is base`"
        );
        assert!(
            rows[0].message.contains("from model 'root'")
                && rows[0].message.contains("drop `is base`"),
            "{}",
            rows[0].message
        );

        // An own field overrides an inherited one: the row is the OWN
        // field's (at the field, with the deletion), never the mixin's.
        let src = "trait tagged:\n    kind string?\n\nmodel logM is tagged:\n    kind string?\n    \
                   msg string?\n\noneof mail by kind:\n    \"log\" -> logM\n";
        let rows = shadow_rows(src);
        assert_eq!(rows.len(), 1, "{rows:?}");
        let own = src.rfind("kind string?").expect("the arm's own field");
        assert_eq!(
            rows[0].span,
            Some(Span::new(own, own + "kind string?".len()))
        );
        assert_eq!(rows[0].suggestions.len(), 1, "the deletion");

        // The first `is` target in clause order supplies the field —
        // `resolve_model_inheritance`'s precedence — so the row is at it.
        let src = "trait a:\n    x string?\n\ntrait b:\n    kind string?\n\ntrait c:\n    kind \
                   string?\n\nmodel logM is a, b, c:\n    msg string?\n\noneof mail by kind:\n    \
                   \"log\" -> logM\n";
        let rows = shadow_rows(src);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0].message.contains("from trait 'b'"),
            "{}",
            rows[0].message
        );

        // An inherited REQUIRED seal: the same site, the seal's remedy.
        let src = "trait tagged:\n    kind string #sealed\n\nmodel logM is tagged:\n    msg \
                   string?\n\noneof mail by kind:\n    \"log\" -> logM\n";
        let rows = shadow_rows(src);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0]
                .message
                .ends_with("declare it optional where it is declared: `kind string? #sealed`"),
            "{}",
            rows[0].message
        );
        assert!(
            rows[0].suggestions.is_empty(),
            "never a fix into another definition"
        );
    }

    #[test]
    fn the_shadow_rule_skips_uninstantiable_and_unknown_arms_and_terminates_on_a_cycle() {
        // A trait arm is its own error (no instances to be ill-formed); an
        // unknown arm is its own error; a cyclic `is` chain is its own
        // error and the walk still terminates.
        let src = "trait t:\n    kind string?\n\noneof mail by kind:\n    \"log\" -> t\n    \
                   \"gone\" -> ghost\n";
        let errs = find_oneof_errors(&extract_src(src));
        assert!(
            errs.iter()
                .any(|e| e.code == Some(codes::TRAIT_ONEOF_VARIANT))
        );
        assert!(errs.iter().any(|e| e.code == Some(codes::ONEOF_INTEGRITY)));
        assert!(
            errs.iter()
                .all(|e| e.code != Some(codes::SHADOWED_DISCRIMINATOR)),
            "{errs:?}"
        );
        let src = "model a is b:\n    x string?\n\nmodel b is a:\n    y string?\n\noneof mail by \
                   kind:\n    \"a\" -> a\n";
        assert!(shadow_rows(src).is_empty());
        // ... and finds a field declared past the cycle's entry.
        let src = "model a is b:\n    x string?\n\nmodel b is a:\n    kind string?\n\noneof mail \
                   by kind:\n    \"a\" -> a\n";
        assert_eq!(shadow_rows(src).len(), 1);
    }

    #[test]
    fn test_oneof_default_discriminator_must_match_arm() {
        let schema = extract_src(
            "model a:\n    x string?\n\noneof u by kind = \"bogus\":\n    \"k\" -> a\n",
        );
        let errs = find_oneof_errors(&schema);
        assert!(
            errs.iter().any(|e| e
                .message
                .contains("default discriminator \"bogus\" does not match any arm")),
            "expected default-mismatch error; got {errs:?}"
        );
    }

    #[test]
    fn test_oneof_valid_default_discriminator_accepted() {
        let schema =
            extract_src("model a:\n    x string?\n\noneof u by kind = \"k\":\n    \"k\" -> a\n");
        assert!(
            find_oneof_errors(&schema).is_empty(),
            "a default matching an arm should be accepted"
        );
        assert_eq!(schema.oneofs[0].default_discriminator.as_deref(), Some("k"));
    }

    #[test]
    fn test_oneof_enum_typed_discriminator_exhaustive_ok() {
        let schema = extract_src(
            "enum kind:\n    - \"log\"\n    - \"postmark\"\n\nmodel a:\n    x string?\n\nmodel b:\n    y string?\n\noneof email by provider as kind:\n    \"log\" -> a\n    \"postmark\" -> b\n",
        );
        assert!(
            find_oneof_errors(&schema).is_empty(),
            "arms exactly covering the enum should be accepted"
        );
        assert_eq!(schema.oneofs[0].discriminator_type.as_deref(), Some("kind"));
    }

    #[test]
    fn test_oneof_enum_typed_missing_arm_rejected() {
        let schema = extract_src(
            "enum kind:\n    - \"log\"\n    - \"postmark\"\n\nmodel a:\n    x string?\n\noneof email by provider as kind:\n    \"log\" -> a\n",
        );
        let errs = find_oneof_errors(&schema);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("missing an arm") && e.message.contains("postmark")),
            "missing enum variant should be reported; got {errs:?}"
        );
    }

    #[test]
    fn test_oneof_enum_typed_extra_arm_rejected() {
        let schema = extract_src(
            "enum kind:\n    - \"log\"\n\nmodel a:\n    x string?\n\nmodel b:\n    y string?\n\noneof email by provider as kind:\n    \"log\" -> a\n    \"postmark\" -> b\n",
        );
        let errs = find_oneof_errors(&schema);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("not a variant of enum")
                    && e.message.contains("postmark")),
            "arm outside the enum should be reported; got {errs:?}"
        );
    }

    #[test]
    fn test_oneof_discriminator_type_must_be_enum() {
        let schema = extract_src(
            "model a:\n    x string?\n\noneof email by provider as notAnEnum:\n    \"log\" -> a\n",
        );
        let errs = find_oneof_errors(&schema);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("is not a declared enum")),
            "unknown discriminator type should be reported; got {errs:?}"
        );
    }

    #[test]
    fn test_oneof_name_collision_with_model_rejected() {
        let schema = extract_src(
            "model email:\n    x string?\n\nmodel emailLog:\n    y string?\n\noneof email by provider:\n    \"log\" -> emailLog\n",
        );
        let errs = find_oneof_errors(&schema);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("both a oneof and a model")),
            "expected name-collision error; got {errs:?}"
        );
    }

    #[test]
    fn test_cycle_detection_traverses_oneof_variants() {
        // a -> (field u: oneof) -> variant b -> field a  => cycle a,b
        let schema = extract_src(
            "model a:\n    u u?\n\nmodel b:\n    parent a?\n\noneof u by kind:\n    \"b\" -> b\n",
        );
        let cycles = find_model_cycles(&schema);
        assert!(
            cycles
                .iter()
                .any(|e| e.message.contains("circular dependency")),
            "cycle through oneof variant should be detected; got {cycles:?}"
        );
    }

    #[test]
    fn test_extract_model() {
        let source = "model provider:\n    type providerType\n    model string\n    temperature number?\n    baseUrl string?\n";
        let schema = extract_schema(source).0;

        assert_eq!(schema.models.len(), 1);
        let model = &schema.models[0];
        assert_eq!(model.name, "provider");
        assert_eq!(model.fields.len(), 4);

        assert_eq!(model.fields[0].name, "type");
        assert!(
            matches!(model.fields[0].field_type, FieldType::ModelRef(ref s) if s == "providerType")
        );
        assert!(!model.fields[0].optional);

        assert_eq!(model.fields[1].name, "model");
        assert!(matches!(
            model.fields[1].field_type,
            FieldType::Primitive {
                ty: PrimitiveType::String,
                ..
            }
        ));

        assert_eq!(model.fields[2].name, "temperature");
        assert!(model.fields[2].optional);

        assert_eq!(model.fields[3].name, "baseUrl");
        assert!(model.fields[3].optional);
    }

    #[test]
    fn test_extract_model_with_default() {
        let source = "model prompt:\n    outputFormat string = \"text\"\n";
        let schema = extract_schema(source).0;

        assert_eq!(schema.models.len(), 1);
        let field = &schema.models[0].fields[0];
        assert_eq!(field.name, "outputFormat");
        assert_eq!(
            field.default_value.as_ref().map(|v| &v.value),
            Some(&crate::types::Value::String("text".into()))
        );
    }

    #[test]
    fn test_extract_model_with_array_field() {
        let source = "model workflow:\n    steps []step\n    extensions []extensionPoint?\n";
        let schema = extract_schema(source).0;

        let model = &schema.models[0];
        assert_eq!(model.fields.len(), 2);

        assert!(matches!(model.fields[0].field_type, FieldType::List(_)));
        assert!(!model.fields[0].optional);

        assert!(matches!(model.fields[1].field_type, FieldType::List(_)));
        assert!(model.fields[1].optional);
    }

    #[test]
    fn test_extract_model_with_modifier_fields() {
        let source = "model plugin:\n    wasm string\n    |allow []string?\n    |deny []string?\n";
        let schema = extract_schema(source).0;

        let model = &schema.models[0];
        assert_eq!(model.fields.len(), 3);
        assert_eq!(model.fields[0].name, "wasm");
        assert!(matches!(model.fields[1].field_type, FieldType::Modifier(_)));
        assert!(model.fields[1].optional);
    }

    #[test]
    fn test_extract_model_with_object_field() {
        use crate::model::FieldType;

        let source = "model plugin:\n    wasm string\n    config object?\n";
        let schema = extract_schema(source).0;

        let model = &schema.models[0];
        assert_eq!(model.fields.len(), 2);
        assert_eq!(model.fields[1].name, "config");
        assert!(matches!(
            &model.fields[1].field_type,
            FieldType::Primitive {
                ty: PrimitiveType::Object,
                ..
            }
        ));
        assert!(model.fields[1].optional);
    }

    #[test]
    fn test_extract_enum() {
        let source = "enum providerType:\n    - \"anthropic\"\n    - \"openai\"\n    - \"groq\"\n    - \"ollama\"\n";
        let schema = extract_schema(source).0;

        assert_eq!(schema.enums.len(), 1);
        let e = &schema.enums[0];
        assert_eq!(e.name, "providerType");
        assert_eq!(e.variants, vec!["anthropic", "openai", "groq", "ollama"]);
    }

    #[test]
    fn test_extract_mixed() {
        let source = "\
enum status:\n    - \"active\"\n    - \"inactive\"\n\n\
model user:\n    name string\n    status status\n";
        let schema = extract_schema(source).0;

        assert_eq!(schema.enums.len(), 1);
        assert_eq!(schema.models.len(), 1);
    }

    #[test]
    fn test_model_cycle_direct() {
        let source = "model A:\n    child B?\n\nmodel B:\n    parent A?\n";
        let schema = extract_schema(source).0;

        let errors = find_model_cycles(&schema);
        assert!(
            !errors.is_empty(),
            "should detect cycle between A and B; errors: {:?}",
            errors
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("circular dependency")),
            "error should mention circular dependency; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_model_cycle_self_referencing() {
        let source = "model tree:\n    value string\n    left tree?\n    right tree?\n";
        let schema = extract_schema(source).0;

        let errors = find_model_cycles(&schema);
        assert!(
            !errors.is_empty(),
            "should detect self-referencing model; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_model_cycle_three_way() {
        let source = "model A:\n    b B?\n\nmodel B:\n    c C?\n\nmodel C:\n    a A?\n";
        let schema = extract_schema(source).0;

        let errors = find_model_cycles(&schema);
        assert!(
            !errors.is_empty(),
            "should detect three-way cycle; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_model_no_cycle() {
        let source = "model prompt:\n    system string?\n\nmodel step:\n    prompt prompt?\n    next string?\n";
        let schema = extract_schema(source).0;

        let errors = find_model_cycles(&schema);
        assert!(
            errors.is_empty(),
            "should not detect cycle in acyclic models; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_model_cycle_through_list() {
        let source = "model workflow:\n    steps []step\n\nmodel step:\n    parent workflow?\n";
        let schema = extract_schema(source).0;

        let errors = find_model_cycles(&schema);
        assert!(
            !errors.is_empty(),
            "should detect cycle through list field; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_model_ref_to_enum_no_cycle() {
        let source = "enum status:\n    - \"active\"\n    - \"inactive\"\n\nmodel user:\n    status status\n";
        let schema = extract_schema(source).0;

        let errors = find_model_cycles(&schema);
        assert!(
            errors.is_empty(),
            "enum refs should not be treated as model cycles; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_model_cycle_through_union() {
        let source = "model step:\n    provider string?\n    parallel [](step | []step)?\n";
        let schema = extract_schema(source).0;

        let errors = find_model_cycles(&schema);
        assert!(
            !errors.is_empty(),
            "should detect self-referencing model through union type; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_model_cycle_indirect_through_union() {
        let source = "model container:\n    items [](itemA | itemB)\n\nmodel itemA:\n    parent container?\n\nmodel itemB:\n    value string\n";
        let schema = extract_schema(source).0;

        let errors = find_model_cycles(&schema);
        assert!(
            !errors.is_empty(),
            "should detect cycle container -> itemA -> container through union; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_multiple_disjoint_model_cycles() {
        let source = "model A:\n    b B?\n\nmodel B:\n    a A?\n\nmodel X:\n    y Y?\n\nmodel Y:\n    x X?\n";
        let schema = extract_schema(source).0;

        let errors = find_model_cycles(&schema);
        assert!(
            errors.len() >= 4,
            "should detect both independent cycles; got {} errors: {:?}",
            errors.len(),
            errors
        );
    }

    #[test]
    fn test_model_cycle_error_message_contains_path() {
        let source = "model A:\n    b B?\n\nmodel B:\n    c C?\n\nmodel C:\n    a A?\n";
        let schema = extract_schema(source).0;

        let errors = find_model_cycles(&schema);
        let has_path = errors.iter().any(|e| {
            let msg = &e.message;
            msg.contains("A -> B") || msg.contains("B -> C") || msg.contains("C -> A")
        });
        assert!(
            has_path,
            "error message should include the cycle path; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_large_acyclic_model_graph_no_false_positive() {
        let mut source = String::new();
        for i in 0..50 {
            source.push_str(&format!(
                "model m{}:\n    value string\n    child m{}?\n\n",
                i,
                i + 1
            ));
        }
        source.push_str("model m50:\n    value string\n");
        let schema = extract_schema(&source).0;

        let errors = find_model_cycles(&schema);
        assert!(
            errors.is_empty(),
            "large acyclic model graph should not produce false positives; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_large_model_graph_performance() {
        let mut source = String::new();
        for i in 0..100 {
            source.push_str(&format!(
                "model node{}:\n    value string\n    left node{}?\n    right node{}?\n\n",
                i,
                (i + 1) % 100,
                (i + 2) % 100,
            ));
        }
        let schema = extract_schema(&source).0;

        let start = std::time::Instant::now();
        let errors = find_model_cycles(&schema);
        let elapsed = start.elapsed();

        assert!(!errors.is_empty(), "should detect cycles in circular graph");
        assert!(
            elapsed.as_millis() < 1000,
            "cycle detection on 100-node graph should complete in <1s; took {:?}",
            elapsed
        );
    }

    // --- resolve_model_inheritance tests ---

    #[test]
    fn test_resolve_single_parent() {
        let source = "model A:\n    x string\n    y number\n\nmodel B is A:\n    z string\n";
        let mut schema = extract_schema(source).0;
        resolve_model_inheritance(&mut schema);

        let b = schema.models.iter().find(|m| m.name == "B").unwrap();
        let names: Vec<&str> = b.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["x", "y", "z"]);
    }

    #[test]
    fn test_resolve_multi_parent() {
        let source =
            "model A:\n    x string\n\nmodel B:\n    y number\n\nmodel C is A, B:\n    z string\n";
        let mut schema = extract_schema(source).0;
        resolve_model_inheritance(&mut schema);

        let c = schema.models.iter().find(|m| m.name == "C").unwrap();
        let names: Vec<&str> = c.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["x", "y", "z"]);
    }

    #[test]
    fn test_resolve_diamond() {
        let source = "\
model A:\n    a string\n\n\
model B is A:\n    b string\n\n\
model C is A:\n    c string\n\n\
model D is B, C:\n    d string\n";
        let mut schema = extract_schema(source).0;
        resolve_model_inheritance(&mut schema);

        let d = schema.models.iter().find(|m| m.name == "D").unwrap();
        let names: Vec<&str> = d.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["a", "b", "c", "d"],
            "A's field should appear only once"
        );
    }

    #[test]
    fn test_resolve_child_override() {
        let source =
            "model A:\n    x string\n    y number\n\nmodel B is A:\n    x number\n    z string\n";
        let mut schema = extract_schema(source).0;
        resolve_model_inheritance(&mut schema);

        let b = schema.models.iter().find(|m| m.name == "B").unwrap();
        let names: Vec<&str> = b.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["y", "x", "z"],
            "parent field 'y' prepended, 'x' kept as child's version"
        );
        assert!(
            matches!(
                b.fields.iter().find(|f| f.name == "x").unwrap().field_type,
                FieldType::Primitive {
                    ty: PrimitiveType::Number,
                    ..
                }
            ),
            "child's 'x' should be number, not string"
        );
    }

    // --- find_extends_cycles tests ---

    #[test]
    fn test_extends_cycle_direct() {
        let source = "model A is B:\n    x string\n\nmodel B is A:\n    y string\n";
        let schema = extract_schema(source).0;

        let errors = find_extends_cycles(&schema);
        assert!(!errors.is_empty(), "should detect cycle between A and B");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("circular inheritance")),
            "error should mention circular inheritance; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_extends_cycle_self() {
        let source = "model A is A:\n    x string\n";
        let schema = extract_schema(source).0;

        let errors = find_extends_cycles(&schema);
        assert!(!errors.is_empty(), "should detect self-referencing extends");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("circular inheritance")),
            "error should mention circular inheritance; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_extends_no_cycle() {
        let source = "model A:\n    x string\n\nmodel B is A:\n    y string\n";
        let schema = extract_schema(source).0;

        let errors = find_extends_cycles(&schema);
        assert!(
            errors.is_empty(),
            "should not detect cycle in acyclic inheritance; errors: {:?}",
            errors
        );
    }
}

#[cfg(test)]
mod composition_tests {
    //! RFC 0011: composition (`is`) and trait-usage integrity.

    use super::*;
    use crate::cst::extract_schema;

    fn find(src: &str) -> Vec<Diagnostic> {
        let (schema, errs) = extract_schema(src);
        assert!(errs.is_empty(), "clean extraction expected: {errs:?}");
        find_composition_errors(&schema)
    }

    fn codes_of(diags: &[Diagnostic]) -> Vec<String> {
        diags
            .iter()
            .filter_map(|d| d.code.map(|c| c.to_string()))
            .collect()
    }

    #[test]
    fn clean_composition_has_no_findings() {
        let errs = find(
            "trait monitored:\n    timeout duration = 5s\n\n\
             trait audited is monitored:\n    auditedBy string?\n\n\
             model endpoint is audited:\n    url string+\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn unknown_is_target_reports_2020_with_suggestion_at_target_token() {
        let src =
            "trait monitored:\n    t duration?\n\nmodel endpoint is monitred:\n    url string?\n";
        let errs = find(src);
        assert_eq!(codes_of(&errs), vec!["NML2020"]);
        let d = &errs[0];
        // The did-you-mean is machine-applicable at exactly the target token.
        let s = d.suggestions.first().expect("suggestion");
        assert_eq!(s.replacement, "monitored");
        let span = d.span.expect("span");
        assert_eq!(&src[span.start..span.end], "monitred");
    }

    /// The `is`-target vocabulary is DECLARATION order: `suggest` breaks a
    /// distance tie toward the earliest candidate, so a hash-ordered one
    /// made this hint a different name on each run of the same binary over
    /// the same file. Judged over TWO orderings of one name set — one
    /// hash-ordered answer cannot be both.
    #[test]
    fn an_unknown_is_target_names_the_earliest_tied_candidate() {
        for [first, second] in [["ax", "ay"], ["ay", "ax"]] {
            let src = format!(
                "trait {first}:\n    t duration?\n\ntrait {second}:\n    u duration?\n\n\
                 model endpoint is az:\n    url string?\n"
            );
            let errs = find(&src);
            assert_eq!(codes_of(&errs), vec!["NML2020"]);
            let s = errs[0].suggestions.first().expect("suggestion");
            assert_eq!(
                s.replacement, first,
                "the earliest of two equidistant targets"
            );
        }
    }

    #[test]
    fn enum_and_oneof_is_targets_report_2021() {
        let errs = find("enum level:\n    - \"a\"\n\nmodel m is level:\n    x string?\n");
        assert_eq!(codes_of(&errs), vec!["NML2021"]);
        assert!(errs[0].message.contains("an enum"));

        let errs = find(
            "model log:\n    x string?\n\noneof n by kind:\n    \"log\" -> log\n\nmodel m is n:\n    y string?\n",
        );
        assert_eq!(codes_of(&errs), vec!["NML2021"]);
        assert!(errs[0].message.contains("a oneof"));
    }

    #[test]
    fn trait_as_field_type_reports_2022_at_every_nesting() {
        // Direct, list, set, union, modifier, and both arm positions.
        let errs = find(
            "trait cap:\n    x string?\n\n\
             model m:\n    a cap?\n    b []cap?\n    c set<cap>?\n    d (string | cap)?\n    |e cap?\n    f (cap -> string)?\n",
        );
        assert_eq!(errs.len(), 6, "{errs:?}");
        assert!(
            errs.iter()
                .all(|d| d.code.map(|c| c.to_string()).as_deref() == Some("NML2022"))
        );
    }

    #[test]
    fn oneof_arm_targeting_trait_reports_2023_not_2012() {
        let (schema, errs) =
            extract_schema("trait cap:\n    x string?\n\noneof entry by kind:\n    \"a\" -> cap\n");
        assert!(errs.is_empty(), "{errs:?}");
        let errs = find_oneof_errors(&schema);
        assert_eq!(codes_of(&errs), vec!["NML2023"]);
    }

    #[test]
    fn inheritance_merges_trait_fields_with_defaults() {
        let (mut schema, errs) = extract_schema(
            "trait monitored:\n    timeout duration = 5s\n    interval duration = 60s\n\n\
             model endpoint is monitored:\n    url string+\n    timeout duration = 9s\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        resolve_model_inheritance(&mut schema);
        let endpoint = schema.models.iter().find(|m| m.name == "endpoint").unwrap();
        let field = |n: &str| endpoint.fields.iter().find(|f| f.name == n).unwrap();
        let default_text = |n: &str| match field(n).default_value.as_ref().map(|sv| &sv.value) {
            Some(crate::types::Value::Duration(d)) => d.to_string(),
            other => panic!("expected a duration default for '{n}', got {other:?}"),
        };
        // Inherited default comes along; the model's own field overrides.
        assert_eq!(default_text("interval"), "60s");
        assert_eq!(default_text("timeout"), "9s");
    }
}

/// RFC 0018 facet definition rules (NML2058), context-free over the
/// semantic AST — the single emission point is [`crate::cst::extract_schema`],
/// which every schema-consuming surface (both CLI verbs, the LSP, the
/// nml-validate loader and therefore packages and downstream boots)
/// funnels through, so the rules cannot be skipped by construction.
pub fn facet_definition_diagnostics(file: &crate::ast::File) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for decl in &file.declarations {
        let crate::ast::DeclarationKind::Block(block) = &decl.kind else {
            continue;
        };
        if !matches!(block.keyword.name.as_str(), "model" | "trait") {
            continue;
        }
        for entry in &block.body.entries {
            // A typed modifier (`|cap number(min = 1)`) declares a field
            // too — extraction lowers it to `FieldType::Modifier(inner)`
            // and enforcement recurses through it, so its facets bind
            // real values. It must face the same declaration rules, or
            // an unsatisfiable range on a modifier loads clean and then
            // rejects every value (the trap §1.2 promises cannot exist).
            match &entry.kind {
                crate::ast::BodyEntryKind::FieldDefinition(fd) => {
                    facet_rules_in_type(&fd.field_type, &fd.name.name, &mut diags);
                    // RFC 0018: "a declared default must itself satisfy
                    // the facets". Checked HERE, not in the validate
                    // verb, or the promise that the rules cannot be
                    // skipped by construction is false for this one
                    // rule — `load_schema`, packages and downstream
                    // boots never call that verb, so a violating
                    // default would load clean and then MATERIALIZE
                    // into runtime config silently.
                    if let Some(default) = &fd.default_value {
                        facet_default_violations(
                            &fd.field_type,
                            &default.value,
                            &fd.name.name,
                            default.span,
                            &mut diags,
                        );
                    }
                }
                crate::ast::BodyEntryKind::Modifier(m) => {
                    if let crate::ast::ModifierValue::TypeAnnotation { field_type, .. } = &m.value {
                        facet_rules_in_type(field_type, &m.name.name, &mut diags);
                    }
                }
                _ => {}
            }
        }
    }
    diags
}

/// The AST facet list as a [`crate::model::Facets`] over one domain —
/// the shape the shared comparison home speaks; `pick` selects the
/// domain's values (cross-domain values are the declaration rules'
/// findings and never load as bounds). Mirrors extraction's CST-side
/// builder at the AST layer; unknown/duplicate keys are the
/// declaration rules' business, so last-writer-wins here is harmless
/// (an invalid declaration never loads).
fn facets_of_ast_as<T: Clone>(
    facets: &[crate::ast::FacetExpr],
    pick: impl Fn(&crate::types::Value) -> Option<T>,
) -> crate::model::Facets<T> {
    let mut out = crate::model::Facets::NONE;
    for f in facets {
        let Some(value) = pick(&f.value.value) else {
            continue;
        };
        let bound = |exclusive| crate::model::FacetBoundOf {
            value: value.clone(),
            exclusive,
            span: f.span,
        };
        match f.key.name.as_str() {
            "min" => out.min = Some(bound(false)),
            "exclusiveMin" => out.min = Some(bound(true)),
            "max" => out.max = Some(bound(false)),
            "exclusiveMax" => out.max = Some(bound(true)),
            "multipleOf" => {
                out.multiple_of = Some(crate::model::FacetMultipleOf {
                    value,
                    span: f.span,
                })
            }
            _ => {}
        }
    }
    out
}

fn pick_number(v: &crate::types::Value) -> Option<crate::types::Number> {
    match v {
        crate::types::Value::Number(n) => Some(*n),
        _ => None,
    }
}

fn pick_duration(v: &crate::types::Value) -> Option<crate::duration::Duration> {
    match v {
        crate::types::Value::Duration(d) => Some(*d),
        _ => None,
    }
}

/// Could this type hold `value` at all? The facet domain only needs
/// the coarse question — a `number` literal cannot land in a `string`,
/// `bool`, enum or model-ref variant — which is exactly what keeps a
/// non-numeric union variant from vacuously admitting a number.
fn facet_type_applies(te: &crate::ast::FieldTypeExpr, value: &crate::types::Value) -> bool {
    use crate::ast::FieldTypeExpr as T;
    match (te, value) {
        (T::Named { name, .. }, crate::types::Value::Number(_)) => name.name == "number",
        (T::Named { name, .. }, crate::types::Value::Duration(_)) => name.name == "duration",
        // Every element must face the element type — `[]string` must
        // NOT claim `[5]`. Mirrors enforcement's `value_matches_type`,
        // which requires all items to match `inner`; without it a
        // non-numeric collection variant vacuously admits a numeric
        // array, the union bug one level down.
        (T::Array(inner) | T::Set(inner), crate::types::Value::Array(items)) => {
            items.iter().all(|i| facet_type_applies(inner, &i.value))
        }
        (T::Union(vs), v) => vs.iter().any(|x| facet_type_applies(x, v)),
        (_, crate::types::Value::Fallback(p, f)) => {
            facet_type_applies(te, &p.value) || facet_type_applies(te, &f.value)
        }
        _ => false,
    }
}

/// A declared default measured against its own facets (NML2057 — it is
/// a VALUE breaking a constraint, reported where values are). Walks
/// collection types element-wise, matching enforcement.
fn facet_default_violations(
    te: &crate::ast::FieldTypeExpr,
    value: &crate::types::Value,
    field_name: &str,
    span: crate::span::Span,
    diags: &mut Vec<Diagnostic>,
) {
    use crate::ast::FieldTypeExpr as T;
    // A fallback default (`= 3 | 5`) faces the type on BOTH legs, like
    // enforcement — either leg can be the value that materializes.
    if let crate::types::Value::Fallback(primary, fallback) = value {
        facet_default_violations(te, &primary.value, field_name, primary.span, diags);
        facet_default_violations(te, &fallback.value, field_name, fallback.span, diags);
        return;
    }
    match te {
        T::Named { name, facets } if name.name == "number" && !facets.is_empty() => {
            let crate::types::Value::Number(n) = value else {
                return;
            };
            for tail in facets_of_ast_as(facets, pick_number).violations(n) {
                diags.push(
                    Diagnostic::error(format!("default for '{field_name}' {tail}"))
                        .with_code(codes::FACET_VIOLATION)
                        .with_span(span),
                );
            }
        }
        T::Named { name, facets } if name.name == "duration" && !facets.is_empty() => {
            let crate::types::Value::Duration(d) = value else {
                return;
            };
            for tail in facets_of_ast_as(facets, pick_duration).violations(d) {
                diags.push(
                    Diagnostic::error(format!("default for '{field_name}' {tail}"))
                        .with_code(codes::FACET_VIOLATION)
                        .with_span(span),
                );
            }
        }
        T::Array(inner) | T::Set(inner) => {
            if let crate::types::Value::Array(items) = value {
                for item in items {
                    facet_default_violations(inner, &item.value, field_name, item.span, diags);
                }
            }
        }
        T::Union(vs) => {
            // Only variants that could HOLD this value get a vote.
            // Without that filter a `string` variant "admitted" a
            // number vacuously (it simply has no facets to check), so
            // EVERY faceted-number union default became unreportable —
            // enforcement avoids this by filtering on `value_matches_
            // type` before asking about facets.
            let applicable: Vec<&crate::ast::FieldTypeExpr> =
                vs.iter().filter(|v| facet_type_applies(v, value)).collect();
            if applicable.is_empty() {
                // Nothing in the union can hold this value: a TYPE
                // error, reported by the type checker, not here.
                return;
            }
            let admitted = applicable.iter().any(|v| {
                let mut probe = Vec::new();
                facet_default_violations(v, value, field_name, span, &mut probe);
                probe.is_empty()
            });
            if !admitted {
                facet_default_violations(applicable[0], value, field_name, span, diags);
            }
        }
        _ => {}
    }
}

fn facet_rules_in_type(
    te: &crate::ast::FieldTypeExpr,
    field_name: &str,
    diags: &mut Vec<Diagnostic>,
) {
    use crate::ast::FieldTypeExpr as T;
    let err = |diags: &mut Vec<Diagnostic>, msg: String, span: crate::span::Span| {
        diags.push(
            Diagnostic::error(msg)
                .with_code(crate::diagnostic::codes::FACET_DEFINITION)
                .with_span(span),
        );
    };
    match te {
        T::Named { name, facets } => {
            if facets.is_empty() {
                return;
            }
            let domain = name.name.as_str();
            if domain != "number" && domain != "duration" {
                err(
                    diags,
                    format!(
                        "'{field_name}': facets attach only to `number` and `duration` — \
                         `{}` cannot carry them",
                        name.name
                    ),
                    facets[0].span,
                );
                return;
            }
            // Domain agreement: a `number` field's bounds are numbers, a
            // `duration` field's are duration literals. A cross-domain
            // value never loads as a bound (extraction drops it), so the
            // mismatch must be SAID here or the facet silently vanishes.
            for f in facets {
                match (&f.value.value, domain) {
                    (crate::types::Value::Duration(d), "number") => err(
                        diags,
                        format!(
                            "'{field_name}': `number` facets take number values — \
                             `{d}` is a duration"
                        ),
                        f.span,
                    ),
                    (crate::types::Value::Number(n), "duration") => {
                        // The worked example must itself be a LEGAL literal:
                        // duration magnitudes are non-negative integers, so
                        // echoing a fractional or negative `n` would teach a
                        // spelling the parser rejects (NML3005/NML3006).
                        let example = if n.is_integral()
                            && *n >= crate::types::Number::ZERO
                            && n.to_string().len() <= 12
                        {
                            format!("(`{} = {n}s`, `{} = {n}ms`, ...) ", f.key.name, f.key.name)
                        } else {
                            format!("(`{} = 5s`, `{} = 500ms`, ...) ", f.key.name, f.key.name)
                        };
                        err(
                            diags,
                            format!(
                                "'{field_name}': `duration` facets take duration literals \
                                 {example}— `{n}` has no unit"
                            ),
                            f.span,
                        );
                    }
                    _ => {}
                }
            }
            const KNOWN: [&str; 5] = ["min", "max", "exclusiveMin", "exclusiveMax", "multipleOf"];
            let mut seen: Vec<&str> = Vec::new();
            for f in facets {
                let k = f.key.name.as_str();
                if !KNOWN.contains(&k) {
                    err(
                        diags,
                        format!(
                            "'{field_name}': unknown facet '{k}' (known: min, max, \
                             exclusiveMin, exclusiveMax, multipleOf)"
                        ),
                        f.span,
                    );
                    continue;
                }
                if seen.contains(&k) {
                    err(
                        diags,
                        format!("'{field_name}': duplicate facet '{k}'"),
                        f.span,
                    );
                }
                seen.push(k);
            }
            let get = |k: &str| facets.iter().find(|f| f.key.name == k);
            // In-domain ordering; `None` when either side is
            // cross-domain (already reported above) or undecoded.
            let ord = |a: &crate::ast::FacetExpr,
                       b: &crate::ast::FacetExpr|
             -> Option<std::cmp::Ordering> {
                match (&a.value.value, &b.value.value) {
                    (crate::types::Value::Number(x), crate::types::Value::Number(y))
                        if domain == "number" =>
                    {
                        Some(x.cmp(y))
                    }
                    (crate::types::Value::Duration(x), crate::types::Value::Duration(y))
                        if domain == "duration" =>
                    {
                        Some(x.cmp(y))
                    }
                    _ => None,
                }
            };
            let shown = |f: &crate::ast::FacetExpr| match &f.value.value {
                crate::types::Value::Number(n) => n.to_string(),
                crate::types::Value::Duration(d) => d.to_string(),
                _ => String::new(),
            };
            for (a, b) in [("min", "exclusiveMin"), ("max", "exclusiveMax")] {
                if let (Some(_), Some(fb)) = (get(a), get(b)) {
                    err(
                        diags,
                        format!("'{field_name}': '{a}' and '{b}' are mutually exclusive"),
                        fb.span,
                    );
                }
            }
            let lo = get("min").or_else(|| get("exclusiveMin"));
            let hi = get("max").or_else(|| get("exclusiveMax"));
            if let (Some(l), Some(h)) = (lo, hi) {
                if let Some(o) = ord(l, h) {
                    let strict =
                        l.key.name.starts_with("exclusive") || h.key.name.starts_with("exclusive");
                    // Durations are a DISCRETE domain (integer nanos): two
                    // exclusive bounds one nanosecond apart admit no value
                    // at all, even though lo < hi. Numbers are dense —
                    // something always sits strictly between distinct
                    // bounds — so only the duration arm needs this.
                    let discrete_gap_empty = l.key.name.starts_with("exclusive")
                        && h.key.name.starts_with("exclusive")
                        && matches!(
                            (&l.value.value, &h.value.value),
                            (
                                crate::types::Value::Duration(x),
                                crate::types::Value::Duration(y),
                            ) if y.total_nanos().saturating_sub(x.total_nanos()) == 1
                        );
                    if o == std::cmp::Ordering::Greater
                        || (o == std::cmp::Ordering::Equal && strict)
                        || discrete_gap_empty
                    {
                        err(
                            diags,
                            format!(
                                "'{field_name}': the declared range is unsatisfiable \
                                 ({} = {} against {} = {})",
                                l.key.name,
                                shown(l),
                                h.key.name,
                                shown(h)
                            ),
                            h.span,
                        );
                    }
                }
            }
            if let Some(m) = get("multipleOf") {
                match &m.value.value {
                    crate::types::Value::Number(v) if domain == "number" => {
                        if *v <= crate::types::Number::ZERO {
                            err(
                                diags,
                                format!("'{field_name}': multipleOf must be positive (got {v})"),
                                m.span,
                            );
                        }
                    }
                    crate::types::Value::Duration(d)
                        if domain == "duration" && d.total_nanos() == 0 =>
                    {
                        err(
                            diags,
                            format!(
                                "'{field_name}': multipleOf must be a positive duration \
                                 (got {d})"
                            ),
                            m.span,
                        );
                    }
                    _ => {}
                }
            }
        }
        T::Array(inner) | T::Set(inner) => facet_rules_in_type(inner, field_name, diags),
        T::Union(vs) => {
            for v in vs {
                facet_rules_in_type(v, field_name, diags);
            }
        }
        T::Arms { key, target } => {
            facet_rules_in_type(key, field_name, diags);
            facet_rules_in_type(target, field_name, diags);
        }
    }
}
