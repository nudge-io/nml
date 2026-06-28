//! Orchestrated schema-loading pipeline.
//!
//! [`load_schema`] is the single entry point for turning parsed schema files
//! into definitions ready for a
//! [`SchemaValidator`](crate::schema::SchemaValidator).  It runs the full
//! `nml-core` extraction pipeline -- extract, duplicate detection,
//! inheritance-cycle detection, inheritance resolution, and reference-cycle
//! detection -- and surfaces every problem as a [`Diagnostic`].

use std::collections::HashSet;

use nml_core::error::NmlError;
// Import the passes by name (not the module) so the bare `schema` identifier stays
// free for the local `ExtractedSchema` value and our own `crate::schema` module.
use nml_core::schema::{
    find_extends_cycles, find_model_cycles, find_oneof_errors, find_shorthand_errors,
    resolve_model_inheritance,
    ExtractedSchema,
};

use crate::diagnostics::{Diagnostic, Severity};

/// Load a schema from one or more NML source documents.
///
/// Pipeline:
/// 1. extract model/enum definitions from every source over the CST
///    ([`extract_schema`](nml_core::cst::extract_schema)) — surfacing any parse
///    error as a diagnostic while still extracting from the well-formed parts,
/// 2. report duplicate model/enum names (first definition wins),
/// 3. report inheritance (`is`) cycles as **errors** -- such hierarchies
///    cannot be resolved,
/// 4. resolve model inheritance so child models carry ancestor fields,
/// 5. report model-reference cycles as **warnings** -- recursive models are
///    structurally valid (instance validation is depth-limited) but worth
///    flagging.
///
/// Always returns the loaded definitions, even when diagnostics are present,
/// so callers can keep validating with a best-effort schema.
pub fn load_schema(sources: &[&str]) -> (ExtractedSchema, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();

    let mut schema = ExtractedSchema::default();
    for source in sources {
        let (extracted, errors) = nml_core::cst::extract_schema(source);
        schema.models.extend(extracted.models);
        schema.enums.extend(extracted.enums);
        schema.oneofs.extend(extracted.oneofs);
        for err in &errors {
            diagnostics.push(to_diagnostic(err, Severity::Error));
        }
    }

    report_duplicates(&schema, &mut diagnostics);

    for err in find_extends_cycles(&schema) {
        diagnostics.push(to_diagnostic(&err, Severity::Error));
    }

    resolve_model_inheritance(&mut schema);

    // At most one scalar-shorthand (`!`) field per model — checked post-inheritance
    // so an inherited `!` and a child `!` are caught together (RFC 0005 §8).
    for err in find_shorthand_errors(&schema) {
        diagnostics.push(to_diagnostic(&err, Severity::Error));
    }

    // `oneof` integrity (arm models exist, unique values, name collisions) is
    // an error: a malformed union cannot be validated against.
    for err in find_oneof_errors(&schema) {
        diagnostics.push(to_diagnostic(&err, Severity::Error));
    }

    for err in find_model_cycles(&schema) {
        diagnostics.push(to_diagnostic(&err, Severity::Warning));
    }

    (schema, diagnostics)
}

fn report_duplicates(schema: &ExtractedSchema, diagnostics: &mut Vec<Diagnostic>) {
    let mut seen_models = HashSet::new();
    for model in &schema.models {
        if !seen_models.insert(model.name.as_str()) {
            diagnostics.push(
                Diagnostic::error(format!("duplicate model definition '{}'", model.name))
                    .with_span(model.span),
            );
        }
    }

    let mut seen_enums = HashSet::new();
    for enum_def in &schema.enums {
        if !seen_enums.insert(enum_def.name.as_str()) {
            diagnostics.push(
                Diagnostic::error(format!("duplicate enum definition '{}'", enum_def.name))
                    .with_span(enum_def.span),
            );
        }
    }

    let mut seen_oneofs = HashSet::new();
    for oneof in &schema.oneofs {
        if !seen_oneofs.insert(oneof.name.as_str()) {
            diagnostics.push(
                Diagnostic::error(format!("duplicate oneof definition '{}'", oneof.name))
                    .with_span(oneof.span),
            );
        }
    }
}

fn to_diagnostic(err: &NmlError, severity: Severity) -> Diagnostic {
    Diagnostic {
        message: err.message().to_string(),
        severity,
        span: Some(err.span()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SchemaValidator;

    fn errors(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
        diags
            .iter()
            .filter(|d| matches!(d.severity, Severity::Error))
            .collect()
    }

    #[test]
    fn test_inheritance_resolved_through_pipeline() {
        let (schema, diags) = load_schema(&[
            "model base:\n    name string\n",
            "model child is base:\n    extra string\n",
        ]);

        assert!(errors(&diags).is_empty(), "unexpected errors: {diags:?}");
        let child = schema.models.iter().find(|m| m.name == "child").unwrap();
        let names: Vec<&str> = child.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["name", "extra"],
            "child should inherit parent fields"
        );
    }

    #[test]
    fn test_inherited_required_field_enforced_by_validator() {
        // `id` (not `name`) is the inherited required field, so block-name injection
        // (which fills `name` from the block identifier) doesn't mask the enforcement.
        let (schema, diags) = load_schema(&[
            "model base:\n    id string\n\nmodel child is base:\n    extra string\n",
        ]);
        assert!(errors(&diags).is_empty(), "unexpected errors: {diags:?}");

        let validator = SchemaValidator::from(schema);
        let doc = nml_core::cst::parse_to_ast("child C:\n    extra = \"x\"\n").unwrap();
        let result = validator.validate(&doc);
        assert!(
            result
                .iter()
                .any(|d| d.message.contains("missing required field 'id'")),
            "inherited required field should be enforced; diags: {result:?}"
        );
    }

    #[test]
    fn test_extends_cycle_reported_as_error() {
        let (_, diags) =
            load_schema(&["model a is b:\n    x string\n\nmodel b is a:\n    y string\n"]);
        assert!(
            errors(&diags)
                .iter()
                .any(|d| d.message.contains("circular inheritance")),
            "extends cycle should be an error diagnostic; diags: {diags:?}"
        );
    }

    #[test]
    fn test_model_ref_cycle_reported_as_warning() {
        let (_, diags) = load_schema(&["model a:\n    child b?\n\nmodel b:\n    parent a?\n"]);
        let cycle = diags
            .iter()
            .find(|d| d.message.contains("circular dependency"))
            .expect("model-reference cycle should be reported");
        assert!(
            matches!(cycle.severity, Severity::Warning),
            "recursive models are valid, so reference cycles are warnings"
        );
    }

    #[test]
    fn test_duplicate_model_names_across_files() {
        let (schema, diags) = load_schema(&[
            "model server:\n    port number\n",
            "model server:\n    host string\n",
        ]);

        assert!(
            errors(&diags)
                .iter()
                .any(|d| d.message.contains("duplicate model definition 'server'")),
            "duplicate model names should be errors; diags: {diags:?}"
        );
        // First definition wins for validation purposes.
        let server = schema.models.iter().find(|m| m.name == "server").unwrap();
        assert_eq!(server.fields[0].name, "port");
    }

    #[test]
    fn test_duplicate_enum_names_across_files() {
        let (_, diags) = load_schema(&[
            "enum status:\n    - \"on\"\n",
            "enum status:\n    - \"off\"\n",
        ]);
        assert!(
            errors(&diags)
                .iter()
                .any(|d| d.message.contains("duplicate enum definition 'status'")),
            "duplicate enum names should be errors; diags: {diags:?}"
        );
    }

    #[test]
    fn malformed_source_still_loads_wellformed_definitions() {
        // Resilience (exceeds the old pre-parsed-File API, which could not carry a
        // parse error): a syntax error in one source surfaces as a diagnostic, yet
        // the well-formed definitions across all sources still load.
        let (schema, diags) = load_schema(&["model good:\n    name string\n", "model M:\n    @@@\n"]);
        assert!(
            schema.models.iter().any(|m| m.name == "good"),
            "well-formed model still loads despite a sibling parse error"
        );
        assert!(
            !errors(&diags).is_empty(),
            "the parse error is surfaced as a diagnostic: {diags:?}"
        );
    }

    #[test]
    fn test_clean_schema_no_diagnostics() {
        let (schema, diags) = load_schema(&[
            "enum status:\n    - \"on\"\n    - \"off\"\n\nmodel base:\n    name string\n",
            "model server is base:\n    status status\n",
        ]);
        assert!(
            diags.is_empty(),
            "clean schema should load silently: {diags:?}"
        );
        assert!(!schema.is_empty());
        assert_eq!(schema.models.len(), 2);
        assert_eq!(schema.enums.len(), 1);
    }
}
