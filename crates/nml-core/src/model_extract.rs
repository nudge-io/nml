use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::error::NmlError;
use crate::model::{EnumDef, FieldDef, FieldType, ModelDef};
use crate::types::PrimitiveType;

/// Results of extracting schema definitions from a parsed NML file.
#[derive(Debug, Default)]
pub struct ExtractedSchema {
    pub models: Vec<ModelDef>,
    pub enums: Vec<EnumDef>,
}

/// Extract model and enum definitions from a parsed AST.
pub fn extract(file: &File) -> ExtractedSchema {
    let mut schema = ExtractedSchema::default();

    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => match block.keyword.name.as_str() {
                "model" => {
                    if let Some(model) = extract_model(block, decl.span) {
                        schema.models.push(model);
                    }
                }
                "enum" => {
                    if let Some(enum_def) = extract_enum(block, decl.span) {
                        schema.enums.push(enum_def);
                    }
                }
                _ => {}
            },
            DeclarationKind::Array(_) => {}
            DeclarationKind::Const(_) | DeclarationKind::Template(_) => {}
        }
    }

    schema
}

/// Detect cycles in the model dependency graph.
///
/// Builds a directed graph of model-to-model edges via `FieldType::ModelRef`
/// (including through `List` and `Union` wrappers) and reports any cycles found.
pub fn find_model_cycles(schema: &ExtractedSchema) -> Vec<NmlError> {
    let model_names: HashSet<&str> = schema.models.iter().map(|m| m.name.as_str()).collect();

    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for model in &schema.models {
        let refs = collect_model_refs(&model.fields, &model_names);
        edges.insert(model.name.as_str(), refs);
    }

    let mut errors = Vec::new();
    let mut globally_visited = HashSet::new();

    for model in &schema.models {
        if globally_visited.contains(model.name.as_str()) {
            continue;
        }
        let mut path = Vec::new();
        detect_cycle(
            model.name.as_str(),
            &edges,
            &mut path,
            &mut globally_visited,
            schema,
            &mut errors,
        );
    }

    errors
}

fn collect_model_refs<'a>(fields: &'a [FieldDef], known_models: &HashSet<&str>) -> Vec<&'a str> {
    let mut refs = Vec::new();
    for field in fields {
        collect_refs_from_type(&field.field_type, known_models, &mut refs);
    }
    refs
}

fn collect_refs_from_type<'a>(
    ft: &'a FieldType,
    known_models: &HashSet<&str>,
    refs: &mut Vec<&'a str>,
) {
    match ft {
        FieldType::ModelRef(name) if known_models.contains(name.as_str()) => {
            refs.push(name.as_str());
        }
        FieldType::List(inner) => collect_refs_from_type(inner, known_models, refs),
        FieldType::Union(variants) => {
            for v in variants {
                collect_refs_from_type(v, known_models, refs);
            }
        }
        _ => {}
    }
}

fn detect_cycle<'a>(
    name: &'a str,
    edges: &HashMap<&'a str, Vec<&'a str>>,
    path: &mut Vec<&'a str>,
    globally_visited: &mut HashSet<&'a str>,
    schema: &ExtractedSchema,
    errors: &mut Vec<NmlError>,
) {
    if let Some(pos) = path.iter().position(|n| *n == name) {
        let cycle: Vec<&str> = path[pos..].to_vec();
        for member in &cycle {
            let span = schema
                .models
                .iter()
                .find(|m| m.name == *member)
                .map(|m| m.span)
                .unwrap_or(crate::span::Span::empty(0));
            let cycle_desc: Vec<_> = cycle
                .iter()
                .chain(std::iter::once(&cycle[0]))
                .copied()
                .collect();
            errors.push(NmlError::Validation {
                message: format!(
                    "circular dependency in model definitions: {}",
                    cycle_desc.join(" -> ")
                ),
                span,
            });
        }
        return;
    }

    if globally_visited.contains(name) {
        return;
    }

    path.push(name);
    if let Some(neighbors) = edges.get(name) {
        for neighbor in neighbors {
            detect_cycle(neighbor, edges, path, globally_visited, schema, errors);
        }
    }
    path.pop();
    globally_visited.insert(name);
}

/// Resolve parent model fields into child models via the `extends` relation.
///
/// For each model that has `extends`, recursively collects ancestor fields and
/// prepends them before the child's own fields. Parents are processed
/// left-to-right; duplicate field names are skipped (first occurrence wins).
/// Child fields shadow/override any ancestor field with the same name.
pub fn resolve_model_inheritance(schema: &mut ExtractedSchema) {
    let index: HashMap<String, usize> = schema
        .models
        .iter()
        .enumerate()
        .map(|(i, m)| (m.name.clone(), i))
        .collect();

    let models_snapshot: Vec<ModelDef> = schema.models.clone();

    for model in &mut schema.models {
        if model.extends.is_empty() {
            continue;
        }

        let child_field_names: HashSet<String> =
            model.fields.iter().map(|f| f.name.clone()).collect();

        let mut seen = HashSet::new();
        for name in &child_field_names {
            seen.insert(name.clone());
        }

        let mut inherited = Vec::new();
        collect_ancestor_fields(
            &model.extends,
            &index,
            &models_snapshot,
            &mut seen,
            &mut inherited,
            &mut HashSet::new(),
        );

        inherited.extend(model.fields.drain(..));
        model.fields = inherited;
    }
}

fn collect_ancestor_fields(
    parents: &[String],
    index: &HashMap<String, usize>,
    models: &[ModelDef],
    seen_fields: &mut HashSet<String>,
    out: &mut Vec<FieldDef>,
    visited: &mut HashSet<String>,
) {
    for parent_name in parents {
        if visited.contains(parent_name) {
            continue;
        }
        let Some(&idx) = index.get(parent_name) else {
            continue;
        };
        visited.insert(parent_name.clone());
        let parent = &models[idx];

        // Recurse into grandparents first so ancestor fields appear before parent fields.
        collect_ancestor_fields(&parent.extends, index, models, seen_fields, out, visited);

        for field in &parent.fields {
            if seen_fields.insert(field.name.clone()) {
                out.push(field.clone());
            }
        }
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
    let mut globally_visited = HashSet::new();

    for model in &schema.models {
        if globally_visited.contains(model.name.as_str()) {
            continue;
        }
        let mut path = Vec::new();
        detect_extends_cycle(
            model.name.as_str(),
            &edges,
            &mut path,
            &mut globally_visited,
            schema,
            &mut errors,
        );
    }

    errors
}

fn detect_extends_cycle<'a>(
    name: &'a str,
    edges: &HashMap<&'a str, Vec<&'a str>>,
    path: &mut Vec<&'a str>,
    globally_visited: &mut HashSet<&'a str>,
    schema: &ExtractedSchema,
    errors: &mut Vec<NmlError>,
) {
    if let Some(pos) = path.iter().position(|n| *n == name) {
        let cycle: Vec<&str> = path[pos..].to_vec();
        for member in &cycle {
            let span = schema
                .models
                .iter()
                .find(|m| m.name == *member)
                .map(|m| m.span)
                .unwrap_or(crate::span::Span::empty(0));
            let cycle_desc: Vec<_> = cycle
                .iter()
                .chain(std::iter::once(&cycle[0]))
                .copied()
                .collect();
            errors.push(NmlError::Validation {
                message: format!(
                    "circular inheritance in model definitions: {}",
                    cycle_desc.join(" -> ")
                ),
                span,
            });
        }
        return;
    }

    if globally_visited.contains(name) {
        return;
    }

    path.push(name);
    if let Some(neighbors) = edges.get(name) {
        for neighbor in neighbors {
            detect_extends_cycle(neighbor, edges, path, globally_visited, schema, errors);
        }
    }
    path.pop();
    globally_visited.insert(name);
}

fn extract_model(block: &BlockDecl, span: crate::span::Span) -> Option<ModelDef> {
    let mut fields = Vec::new();

    for entry in &block.body.entries {
        match &entry.kind {
            BodyEntryKind::FieldDefinition(fd) => {
                fields.push(convert_field_def(fd, entry.span));
            }
            BodyEntryKind::Modifier(m) => {
                if let ModifierValue::TypeAnnotation {
                    field_type,
                    optional,
                } = &m.value
                {
                    fields.push(FieldDef {
                        name: m.name.name.clone(),
                        field_type: FieldType::Modifier(resolve_type_name(field_type)),
                        optional: *optional,
                        default_value: None,
                        span: entry.span,
                    });
                }
            }
            _ => {}
        }
    }

    Some(ModelDef {
        name: block.name.name.clone(),
        extends: block.extends.iter().map(|id| id.name.clone()).collect(),
        fields,
        span,
    })
}

fn extract_enum(block: &BlockDecl, span: crate::span::Span) -> Option<EnumDef> {
    let mut variants = Vec::new();

    for entry in &block.body.entries {
        if let BodyEntryKind::ListItem(item) = &entry.kind {
            match &item.kind {
                ListItemKind::Shorthand(val) => {
                    if let crate::types::Value::String(s) = &val.value {
                        variants.push(s.clone());
                    }
                }
                ListItemKind::Reference(ident) => {
                    variants.push(ident.name.clone());
                }
                _ => {}
            }
        }
    }

    Some(EnumDef {
        name: block.name.name.clone(),
        variants,
        span,
    })
}

fn convert_field_def(fd: &FieldDefinition, span: crate::span::Span) -> FieldDef {
    let field_type = resolve_field_type(&fd.field_type);
    let default_value = fd.default_value.as_ref().map(|v| format_default(&v.value));

    FieldDef {
        name: fd.name.name.clone(),
        field_type,
        optional: fd.optional,
        default_value,
        span,
    }
}

fn resolve_field_type(expr: &FieldTypeExpr) -> FieldType {
    match expr {
        FieldTypeExpr::Named(id) => {
            if let Some(prim) = PrimitiveType::from_str(&id.name) {
                FieldType::Primitive(prim)
            } else {
                FieldType::ModelRef(id.name.clone())
            }
        }
        FieldTypeExpr::Array(inner) => FieldType::List(Box::new(resolve_field_type(inner))),
        FieldTypeExpr::Union(variants) => {
            FieldType::Union(variants.iter().map(resolve_field_type).collect())
        }
    }
}

fn resolve_type_name(expr: &FieldTypeExpr) -> String {
    match expr {
        FieldTypeExpr::Named(id) => id.name.clone(),
        FieldTypeExpr::Array(inner) => format!("[]{}", resolve_type_name(inner)),
        FieldTypeExpr::Union(variants) => {
            let names: Vec<_> = variants.iter().map(resolve_type_name).collect();
            format!("({})", names.join(" | "))
        }
    }
}

fn format_default(value: &crate::types::Value) -> String {
    match value {
        crate::types::Value::String(s) => s.clone(),
        crate::types::Value::Number(n) => format!("{n}"),
        crate::types::Value::Bool(b) => format!("{b}"),
        crate::types::Value::Reference(r) => r.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn test_extract_model() {
        let source = "model provider:\n    type providerType\n    model string\n    temperature number?\n    baseUrl string?\n";
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

        assert_eq!(schema.models.len(), 1);
        let field = &schema.models[0].fields[0];
        assert_eq!(field.name, "outputFormat");
        assert_eq!(field.default_value, Some("text".to_string()));
    }

    #[test]
    fn test_extract_model_with_array_field() {
        let source = "model workflow:\n    steps []step\n    extensions []extensionPoint?\n";
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

        let model = &schema.models[0];
        assert_eq!(model.fields.len(), 3);
        assert_eq!(model.fields[0].name, "wasm");
        assert!(matches!(model.fields[1].field_type, FieldType::Modifier(_)));
        assert!(model.fields[1].optional);
    }

    #[test]
    fn test_extract_model_with_object_field() {
        use crate::model::FieldType;
        use crate::types::PrimitiveType;

        let source = "model plugin:\n    wasm string\n    config object?\n";
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

        assert_eq!(schema.enums.len(), 1);
        assert_eq!(schema.models.len(), 1);
    }

    #[test]
    fn test_model_cycle_direct() {
        let source = "model A:\n    child B?\n\nmodel B:\n    parent A?\n";
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(&source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(&source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let mut schema = extract(&file);
        resolve_model_inheritance(&mut schema);

        let b = schema.models.iter().find(|m| m.name == "B").unwrap();
        let names: Vec<&str> = b.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["x", "y", "z"]);
    }

    #[test]
    fn test_resolve_multi_parent() {
        let source =
            "model A:\n    x string\n\nmodel B:\n    y number\n\nmodel C is A, B:\n    z string\n";
        let file = parser::parse(source).unwrap();
        let mut schema = extract(&file);
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
        let file = parser::parse(source).unwrap();
        let mut schema = extract(&file);
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
        let file = parser::parse(source).unwrap();
        let mut schema = extract(&file);
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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

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
        let file = parser::parse(source).unwrap();
        let schema = extract(&file);

        let errors = find_extends_cycles(&schema);
        assert!(
            errors.is_empty(),
            "should not detect cycle in acyclic inheritance; errors: {:?}",
            errors
        );
    }
}
