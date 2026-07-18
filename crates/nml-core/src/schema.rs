use std::collections::{HashMap, HashSet};

use crate::error::NmlError;
use crate::model::{EnumDef, FieldDef, FieldType, ModelDef, OneOfDef};

/// Schema definitions (models / enums / oneofs) extracted from a source file.
/// Produced by [`crate::cst::extract`] over the CST; the validation/inheritance
/// passes in this module operate on it.
#[derive(Debug, Default)]
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

/// Validate `oneof` declarations against the rest of the schema:
/// - every arm model must be a declared `model`,
/// - discriminator values must be unique within a union,
/// - a union name must not collide with a model or enum name.
pub fn find_oneof_errors(schema: &ExtractedSchema) -> Vec<NmlError> {
    let model_names: HashSet<&str> = schema.models.iter().map(|m| m.name.as_str()).collect();
    let enum_names: HashSet<&str> = schema.enums.iter().map(|e| e.name.as_str()).collect();
    let mut errors = Vec::new();

    for oneof in &schema.oneofs {
        // Every diagnostic for this union points at its declaration span.
        let err = |message: String| NmlError::Validation {
            message,
            span: oneof.span,
        };

        if model_names.contains(oneof.name.as_str()) || enum_names.contains(oneof.name.as_str()) {
            errors.push(err(format!(
                "name '{}' is declared as both a oneof and a model/enum; names must be unique across model/enum/oneof",
                oneof.name
            )));
        }

        let mut seen_values: HashSet<&str> = HashSet::new();
        for (value, model) in &oneof.variants {
            if !seen_values.insert(value.as_str()) {
                errors.push(err(format!(
                    "oneof '{}' has duplicate discriminator value \"{}\"",
                    oneof.name, value
                )));
            }
            if !model_names.contains(model.as_str()) {
                errors.push(err(format!(
                    "oneof '{}' arm \"{}\" references unknown model '{}'",
                    oneof.name, value, model
                )));
            }
        }

        // A declared default discriminator must name one of the arms.
        if let Some(default) = &oneof.default_discriminator {
            if !oneof.variants.iter().any(|(value, _)| value == default) {
                errors.push(err(format!(
                    "oneof '{}' default discriminator \"{}\" does not match any arm",
                    oneof.name, default
                )));
            }
        }

        // An enum-typed discriminator must name a declared enum, and the arm keys
        // must *exactly* cover its variants (exhaustiveness — no missing variant and
        // no arm outside the enum).
        if let Some(type_name) = &oneof.discriminator_type {
            match schema.enums.iter().find(|e| &e.name == type_name) {
                None => errors.push(err(format!(
                    "oneof '{}' discriminator type '{}' is not a declared enum",
                    oneof.name, type_name
                ))),
                Some(enum_def) => {
                    let variants: HashSet<&str> =
                        enum_def.variants.iter().map(String::as_str).collect();
                    let arms: HashSet<&str> =
                        oneof.variants.iter().map(|(v, _)| v.as_str()).collect();
                    // Iterate in source order so diagnostics are deterministic.
                    for variant in &enum_def.variants {
                        if !arms.contains(variant.as_str()) {
                            errors.push(err(format!(
                                "oneof '{}' is missing an arm for enum '{}' variant \"{}\"",
                                oneof.name, type_name, variant
                            )));
                        }
                    }
                    for (value, _) in &oneof.variants {
                        if !variants.contains(value.as_str()) {
                            errors.push(err(format!(
                                "oneof '{}' arm \"{}\" is not a variant of enum '{}'",
                                oneof.name, value, type_name
                            )));
                        }
                    }
                }
            }
        }
    }

    errors
}

/// Each model may declare **at most one** scalar-shorthand (`!`) field: a bare
/// scalar list item supplies a single value, so it can fill only one field.
///
/// Run **after** [`resolve_model_inheritance`] so an inherited `!` and a child
/// `!` are caught together (a child cannot add a second shorthand atop a
/// parent's). RFC 0005 §8.
pub fn find_shorthand_errors(schema: &ExtractedSchema) -> Vec<NmlError> {
    let mut errors = Vec::new();
    for model in &schema.models {
        let shorthand: Vec<&str> = model
            .fields
            .iter()
            .filter(|f| f.shorthand)
            .map(|f| f.name.as_str())
            .collect();
        if shorthand.len() > 1 {
            let names = shorthand
                .iter()
                .map(|n| format!("'{n}'"))
                .collect::<Vec<_>>()
                .join(", ");
            errors.push(NmlError::Validation {
                message: format!(
                    "model '{}' declares more than one shorthand field ({names}); a bare scalar fills a single field",
                    model.name
                ),
                span: model.span,
            });
        }
    }
    errors
}

/// Detect cycles in the model dependency graph.
///
/// Builds a directed graph of model-to-model edges via `FieldType::ModelRef`
/// (including through `List` and `Union` wrappers) and reports any cycles found.
pub fn find_model_cycles(schema: &ExtractedSchema) -> Vec<NmlError> {
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
    errors: &mut Vec<NmlError>,
) {
    let desc = cycle
        .iter()
        .chain(std::iter::once(&cycle[0]))
        .copied()
        .collect::<Vec<_>>()
        .join(" -> ");
    for &member in cycle {
        let span = schema
            .models
            .iter()
            .find(|m| m.name == member)
            .map(|m| m.span)
            .unwrap_or_else(|| crate::span::Span::empty(0));
        errors.push(NmlError::Validation {
            message: format!("{kind}: {desc}"),
            span,
        });
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
                        if let Some(&p) = index.get(parent.as_str()) {
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
                        if let Some(&p) = index.get(parent) {
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
pub fn find_extends_cycles(schema: &ExtractedSchema) -> Vec<NmlError> {
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for model in &schema.models {
        edges.insert(
            model.name.as_str(),
            model.extends.iter().map(|s| s.as_str()).collect(),
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
                &mut errors,
            )
        },
    );
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cst::extract_schema;
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
        match &errs[0] {
            NmlError::Validation { message, .. } => {
                assert!(message.contains("'a'"), "{message}");
                assert!(message.contains("'b'"), "{message}");
                assert!(message.contains("shorthand"), "{message}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
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
                name: format!("m{i}"),
                extends: if i + 1 < DEPTH {
                    vec![format!("m{}", i + 1)]
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
                name: format!("m{i}"),
                extends: if i + 1 < DEPTH {
                    vec![format!("m{}", i + 1)]
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
            name: name.to_string(),
            extends: vec![parent.to_string()],
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
        assert!(errors
            .iter()
            .all(|e| e.message().contains("circular inheritance")));
    }

    #[test]
    fn inheritance_cycle_resolves_without_hang_or_panic() {
        // `a is b` / `b is a` — the cycle is reported elsewhere (find_extends_cycles);
        // resolution must still terminate (no hang, no panic) on a best-effort basis,
        // each model at minimum retaining its own field.
        let model = |name: &str, parent: &str, f: &str| ModelDef {
            name: name.to_string(),
            extends: vec![parent.to_string()],
            fields: vec![FieldDef {
                name: f.to_string(),
                field_type: FieldType::Primitive(PrimitiveType::String),
                optional: false,
                shorthand: false,
                default_value: None,
                directives: Vec::new(),
                doc: None,
                span: crate::span::Span::empty(0),
            }],
            span: crate::span::Span::empty(0),
        };
        let mut schema = ExtractedSchema {
            models: vec![model("a", "b", "fa"), model("b", "a", "fb")],
            enums: vec![],
            oneofs: vec![],
        };
        resolve_model_inheritance(&mut schema); // must not hang or panic
        assert!(schema
            .models
            .iter()
            .all(|m| m.fields.iter().any(|f| f.name.starts_with('f'))));
    }

    #[test]
    fn inheritance_resolves_diamond_once_in_order() {
        // Diamond: D ⟶ {B, C} ⟶ A. The shared base A is resolved a single time;
        // fields appear ancestor-first with the first occurrence winning, child
        // fields last. Exercises the memoized merge across a re-converging DAG.
        let field = |name: &str| FieldDef {
            name: name.to_string(),
            field_type: FieldType::Primitive(PrimitiveType::String),
            optional: false,
            shorthand: false,
            default_value: None,
            directives: Vec::new(),
            doc: None,
            span: crate::span::Span::empty(0),
        };
        let model = |name: &str, extends: &[&str], f: &str| ModelDef {
            name: name.to_string(),
            extends: extends.iter().map(|s| s.to_string()).collect(),
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
            errs.iter().any(|e| e.message().contains("emailPostmark")),
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
                .any(|e| e.message().contains("duplicate discriminator value")),
            "expected duplicate-value error; got {errs:?}"
        );
    }

    #[test]
    fn test_oneof_default_discriminator_must_match_arm() {
        let schema = extract_src(
            "model a:\n    x string?\n\noneof u by kind = \"bogus\":\n    \"k\" -> a\n",
        );
        let errs = find_oneof_errors(&schema);
        assert!(
            errs.iter().any(|e| e
                .message()
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
            errs.iter().any(|e| e.message().contains("missing an arm")
                && e.message().contains("postmark")),
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
                .any(|e| e.message().contains("not a variant of enum")
                    && e.message().contains("postmark")),
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
                .any(|e| e.message().contains("is not a declared enum")),
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
                .any(|e| e.message().contains("both a oneof and a model")),
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
                .any(|e| e.message().contains("circular dependency")),
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
            FieldType::Primitive(PrimitiveType::String)
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
            FieldType::Primitive(PrimitiveType::Object)
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
                .any(|e| e.message().contains("circular dependency")),
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
            let msg = e.message();
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
                FieldType::Primitive(PrimitiveType::Number)
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
                .any(|e| e.message().contains("circular inheritance")),
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
                .any(|e| e.message().contains("circular inheritance")),
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
