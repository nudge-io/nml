//! Orchestrated schema-loading pipeline.
//!
//! [`load_schema`] is the single entry point for turning parsed schema files
//! into definitions ready for [`SchemaValidator`].  It runs the full
//! `nml-core` extraction pipeline -- extract, duplicate detection,
//! inheritance-cycle detection, inheritance resolution, and reference-cycle
//! detection -- and surfaces every problem as a [`Diagnostic`].

use std::collections::HashSet;

use nml_core::ast::File;
use nml_core::error::NmlError;
use nml_core::model::{EnumDef, ModelDef};
use nml_core::model_extract::{self, ExtractedSchema};

use crate::diagnostics::{Diagnostic, Severity};
use crate::schema::SchemaValidator;

/// A fully resolved schema: inheritance applied and cycle checks run.
#[derive(Debug, Default)]
pub struct LoadedSchema {
    pub models: Vec<ModelDef>,
    pub enums: Vec<EnumDef>,
}

impl LoadedSchema {
    /// Whether the schema contains no definitions at all.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty() && self.enums.is_empty()
    }

    /// Consume the schema and build a [`SchemaValidator`] from it.
    pub fn into_validator(self) -> SchemaValidator {
        SchemaValidator::new(self.models, self.enums)
    }
}

/// Load a schema from one or more parsed files.
///
/// Pipeline:
/// 1. extract model/enum definitions from every file,
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
pub fn load_schema(files: &[File]) -> (LoadedSchema, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();

    let mut schema = ExtractedSchema::default();
    for file in files {
        let extracted = model_extract::extract(file);
        schema.models.extend(extracted.models);
        schema.enums.extend(extracted.enums);
    }

    report_duplicates(&schema, &mut diagnostics);

    for err in model_extract::find_extends_cycles(&schema) {
        diagnostics.push(to_diagnostic(&err, Severity::Error));
    }

    model_extract::resolve_model_inheritance(&mut schema);

    for err in model_extract::find_model_cycles(&schema) {
        diagnostics.push(to_diagnostic(&err, Severity::Warning));
    }

    (
        LoadedSchema {
            models: schema.models,
            enums: schema.enums,
        },
        diagnostics,
    )
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
    use nml_core::parser;

    fn parse_all(sources: &[&str]) -> Vec<File> {
        sources.iter().map(|s| parser::parse(s).unwrap()).collect()
    }

    fn errors(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
        diags
            .iter()
            .filter(|d| matches!(d.severity, Severity::Error))
            .collect()
    }

    #[test]
    fn test_inheritance_resolved_through_pipeline() {
        let files = parse_all(&[
            "model base:\n    name string\n",
            "model child is base:\n    extra string\n",
        ]);
        let (schema, diags) = load_schema(&files);

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
        let files = parse_all(&[
            "model base:\n    name string\n\nmodel child is base:\n    extra string\n",
        ]);
        let (schema, diags) = load_schema(&files);
        assert!(errors(&diags).is_empty(), "unexpected errors: {diags:?}");

        let validator = schema.into_validator();
        let doc = parser::parse("child C:\n    extra = \"x\"\n").unwrap();
        let result = validator.validate(&doc);
        assert!(
            result
                .iter()
                .any(|d| d.message.contains("missing required field 'name'")),
            "inherited required field should be enforced; diags: {result:?}"
        );
    }

    #[test]
    fn test_extends_cycle_reported_as_error() {
        let files = parse_all(&["model a is b:\n    x string\n\nmodel b is a:\n    y string\n"]);
        let (_, diags) = load_schema(&files);
        assert!(
            errors(&diags)
                .iter()
                .any(|d| d.message.contains("circular inheritance")),
            "extends cycle should be an error diagnostic; diags: {diags:?}"
        );
    }

    #[test]
    fn test_model_ref_cycle_reported_as_warning() {
        let files = parse_all(&["model a:\n    child b?\n\nmodel b:\n    parent a?\n"]);
        let (_, diags) = load_schema(&files);
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
        let files = parse_all(&[
            "model server:\n    port number\n",
            "model server:\n    host string\n",
        ]);
        let (schema, diags) = load_schema(&files);

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
        let files = parse_all(&[
            "enum status:\n    - \"on\"\n",
            "enum status:\n    - \"off\"\n",
        ]);
        let (_, diags) = load_schema(&files);
        assert!(
            errors(&diags)
                .iter()
                .any(|d| d.message.contains("duplicate enum definition 'status'")),
            "duplicate enum names should be errors; diags: {diags:?}"
        );
    }

    #[test]
    fn test_clean_schema_no_diagnostics() {
        let files = parse_all(&[
            "enum status:\n    - \"on\"\n    - \"off\"\n\nmodel base:\n    name string\n",
            "model server is base:\n    status status\n",
        ]);
        let (schema, diags) = load_schema(&files);
        assert!(
            diags.is_empty(),
            "clean schema should load silently: {diags:?}"
        );
        assert!(!schema.is_empty());
        assert_eq!(schema.models.len(), 2);
        assert_eq!(schema.enums.len(), 1);
    }
}
