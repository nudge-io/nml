use std::collections::HashSet;

use nml_core::ast::*;
use nml_core::model::{EnumDef, ModelDef};
use nml_core::span::SourceMap;
use nml_core::template;
use nml_core::types::{TemplateSegment, Value};
use nml_validate::diagnostics::Severity;
use nml_validate::schema::SchemaValidator;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

/// Configuration for diagnostics computation.
#[derive(Debug, Default)]
pub struct DiagnosticConfig {
    /// Valid template namespaces. If empty, defaults to `template::VALID_NAMESPACES`.
    pub template_namespaces: Vec<String>,
    /// Valid modifier names. If empty, defaults to `nml_validate::schema::VALID_MODIFIERS`.
    pub modifiers: Vec<String>,
}

/// Compute diagnostics for an NML source document.
pub fn compute(
    source: &str,
    models: &[ModelDef],
    enums: &[EnumDef],
    config: &DiagnosticConfig,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let source_map = SourceMap::new(source);

    match nml_core::parse(source) {
        Ok(file) => {
            let mut resolver = nml_core::resolver::Resolver::new();
            resolver.register_file(&file);

            for err in resolver.find_duplicates() {
                diagnostics.push(nml_error_to_diagnostic(&err, &source_map));
            }

            for err in resolver.find_unresolved_references(&file) {
                diagnostics.push(nml_error_to_diagnostic(&err, &source_map));
            }

            if !models.is_empty() || !enums.is_empty() {
                let validator = SchemaValidator::new(models.to_vec(), enums.to_vec())
                    .with_modifiers(config.modifiers.clone());
                for diag in validator.validate(&file) {
                    if let Some(span) = diag.span {
                        let start = source_map.location(span.start);
                        let end = source_map.location(span.end);
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position::new(
                                    start.line as u32 - 1,
                                    start.column as u32 - 1,
                                ),
                                end: Position::new(
                                    end.line as u32 - 1,
                                    end.column as u32 - 1,
                                ),
                            },
                            severity: Some(match diag.severity {
                                Severity::Error => DiagnosticSeverity::ERROR,
                                Severity::Warning => DiagnosticSeverity::WARNING,
                            }),
                            message: diag.message,
                            source: Some("nml".to_string()),
                            ..Default::default()
                        });
                    }
                }
            }

            let ns: Vec<&str> = if config.template_namespaces.is_empty() {
                template::VALID_NAMESPACES.to_vec()
            } else {
                config.template_namespaces.iter().map(|s| s.as_str()).collect()
            };
            validate_templates(&file, &ns, &source_map, &mut diagnostics);
        }
        Err(err) => {
            diagnostics.push(nml_error_to_diagnostic(&err, &source_map));
        }
    }

    diagnostics
}

fn validate_templates(
    file: &File,
    valid_ns: &[&str],
    source_map: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) {
    for decl in &file.declarations {
        let step_names = collect_step_names(decl);
        match &decl.kind {
            DeclarationKind::Block(block) => {
                validate_body_templates(&block.body, &step_names, valid_ns, source_map, diags);
            }
            DeclarationKind::Template(t) => {
                validate_value_templates(&t.value.value, &step_names, valid_ns, source_map, diags);
            }
            DeclarationKind::Const(c) => {
                validate_value_templates(&c.value.value, &step_names, valid_ns, source_map, diags);
            }
            DeclarationKind::Array(arr) => {
                for prop in &arr.body.properties {
                    validate_value_templates(
                        &prop.value.value,
                        &step_names,
                        valid_ns,
                        source_map,
                        diags,
                    );
                }
                for item in &arr.body.items {
                    validate_list_item_templates(item, &step_names, valid_ns, source_map, diags);
                }
            }
        }
    }
}

fn collect_step_names(decl: &Declaration) -> HashSet<String> {
    let mut names = HashSet::new();
    if let DeclarationKind::Block(block) = &decl.kind {
        if block.keyword.name == "workflow" {
            collect_step_names_from_body(&block.body, &mut names);
        }
    }
    names
}

fn collect_step_names_from_body(body: &Body, names: &mut HashSet<String>) {
    for entry in &body.entries {
        if let BodyEntryKind::NestedBlock(nested) = &entry.kind {
            if nested.name.name == "steps" {
                for step_entry in &nested.body.entries {
                    if let BodyEntryKind::ListItem(item) = &step_entry.kind {
                        if let ListItemKind::Named { name, .. } = &item.kind {
                            names.insert(name.name.clone());
                        }
                    }
                }
            }
        }
    }
}

fn validate_body_templates(
    body: &Body,
    step_names: &HashSet<String>,
    valid_ns: &[&str],
    source_map: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) {
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::Property(prop) => {
                validate_value_templates(
                    &prop.value.value,
                    step_names,
                    valid_ns,
                    source_map,
                    diags,
                );
            }
            BodyEntryKind::NestedBlock(nested) => {
                validate_body_templates(&nested.body, step_names, valid_ns, source_map, diags);
            }
            BodyEntryKind::ListItem(item) => {
                validate_list_item_templates(item, step_names, valid_ns, source_map, diags);
            }
            BodyEntryKind::SharedProperty(shared) => {
                validate_body_templates(&shared.body, step_names, valid_ns, source_map, diags);
            }
            _ => {}
        }
    }
}

fn validate_list_item_templates(
    item: &ListItem,
    step_names: &HashSet<String>,
    valid_ns: &[&str],
    source_map: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) {
    match &item.kind {
        ListItemKind::Named { body, .. } => {
            validate_body_templates(body, step_names, valid_ns, source_map, diags);
        }
        ListItemKind::Shorthand(val) => {
            validate_value_templates(&val.value, step_names, valid_ns, source_map, diags);
        }
        _ => {}
    }
}

fn validate_value_templates(
    value: &Value,
    step_names: &HashSet<String>,
    valid_ns: &[&str],
    source_map: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) {
    if let Value::TemplateString(segments) = value {
        for seg in segments {
            if let TemplateSegment::Expression {
                namespace,
                path,
                span,
                ..
            } = seg
            {
                if !valid_ns.contains(&namespace.as_str()) {
                    let start = source_map.location(span.start);
                    let end = source_map.location(span.end);
                    let suggestion = match namespace.as_str() {
                        "arg" => " (did you mean 'args'?)",
                        "step" => " (did you mean 'steps'?)",
                        "inputs" => " (did you mean 'input'?)",
                        "artifact" => " (did you mean 'artifacts'?)",
                        _ => "",
                    };
                    diags.push(Diagnostic {
                        range: Range {
                            start: Position::new(start.line as u32 - 1, start.column as u32 - 1),
                            end: Position::new(end.line as u32 - 1, end.column as u32 - 1),
                        },
                        severity: Some(DiagnosticSeverity::WARNING),
                        message: format!(
                            "unknown template namespace '{namespace}'{suggestion}"
                        ),
                        source: Some("nml".to_string()),
                        ..Default::default()
                    });
                }

                if namespace == "steps" && !step_names.is_empty() {
                    if let Some(step_name) = path.first() {
                        if !step_names.contains(step_name) {
                            let start = source_map.location(span.start);
                            let end = source_map.location(span.end);
                            diags.push(Diagnostic {
                                range: Range {
                                    start: Position::new(
                                        start.line as u32 - 1,
                                        start.column as u32 - 1,
                                    ),
                                    end: Position::new(
                                        end.line as u32 - 1,
                                        end.column as u32 - 1,
                                    ),
                                },
                                severity: Some(DiagnosticSeverity::WARNING),
                                message: format!(
                                    "unknown step '{step_name}' in template expression"
                                ),
                                source: Some("nml".to_string()),
                                ..Default::default()
                            });
                        }
                    }
                }
            }
        }
    }
}

/// Convert a single NML error to an LSP diagnostic.
fn nml_error_to_diagnostic(err: &nml_core::error::NmlError, source_map: &SourceMap) -> Diagnostic {
    let span = err.span();
    let start = source_map.location(span.start);
    let end = source_map.location(span.end);

    let severity = match err {
        nml_core::error::NmlError::Validation { .. } => DiagnosticSeverity::WARNING,
        _ => DiagnosticSeverity::ERROR,
    };

    Diagnostic {
        range: Range {
            start: Position::new(start.line as u32 - 1, start.column as u32 - 1),
            end: Position::new(end.line as u32 - 1, end.column as u32 - 1),
        },
        severity: Some(severity),
        message: err.message().to_string(),
        source: Some("nml".to_string()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> DiagnosticConfig {
        DiagnosticConfig::default()
    }

    #[test]
    fn valid_source_no_diagnostics() {
        let source = "service Svc:\n    localMount = \"/\"\n";
        let diags = compute(source, &[], &[], &default_config());
        assert!(
            diags.is_empty(),
            "valid source should produce no diagnostics: {:?}",
            diags
        );
    }

    #[test]
    fn parse_error_produces_diagnostic() {
        let source = "service\n";
        let diags = compute(source, &[], &[], &default_config());
        assert!(
            !diags.is_empty(),
            "parse error should produce diagnostics"
        );
        assert!(diags
            .iter()
            .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)));
    }

    #[test]
    fn duplicate_decl_produces_diagnostic() {
        let source =
            "service Svc:\n    localMount = \"/\"\n\nservice Svc:\n    localMount = \"/other\"\n";
        let diags = compute(source, &[], &[], &default_config());
        assert!(
            diags.iter().any(|d| d.message.contains("duplicate")),
            "duplicate declarations should be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn unresolved_ref_produces_diagnostic() {
        let source = "workflow W:\n    provider = NonExistent\n";
        let diags = compute(source, &[], &[], &default_config());
        assert!(
            diags.iter().any(|d| d.message.contains("unresolved")),
            "unresolved references should be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn valid_template_namespace_no_diagnostic() {
        let source = "service Svc:\n    instructions = \"{{args.instructions}} base\"\n";
        let diags = compute(source, &[], &[], &default_config());
        assert!(
            !diags.iter().any(|d| d.message.contains("namespace")),
            "valid namespace should not be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn invalid_template_namespace_produces_warning() {
        let source = "service Svc:\n    instructions = \"{{arg.instructions}} base\"\n";
        let diags = compute(source, &[], &[], &default_config());
        assert!(
            diags.iter().any(|d| d.message.contains("unknown template namespace 'arg'")
                && d.message.contains("did you mean 'args'")),
            "invalid namespace should be flagged with suggestion: {:?}",
            diags
        );
    }

    #[test]
    fn unknown_namespace_no_suggestion() {
        let source = "service Svc:\n    val = \"{{foo.bar}} baz\"\n";
        let diags = compute(source, &[], &[], &default_config());
        assert!(
            diags.iter().any(|d| d.message.contains("unknown template namespace 'foo'")
                && !d.message.contains("did you mean")),
            "unknown namespace should be flagged without suggestion: {:?}",
            diags
        );
    }

    #[test]
    fn valid_step_reference_no_diagnostic() {
        let source = concat!(
            "workflow W:\n",
            "    steps:\n",
            "        - classify:\n",
            "            prompt:\n",
            "                system = \"classify\"\n",
            "        - generate:\n",
            "            prompt:\n",
            "                system = \"{{steps.classify.intent}} generate\"\n",
        );
        let diags = compute(source, &[], &[], &default_config());
        assert!(
            !diags.iter().any(|d| d.message.contains("unknown step")),
            "valid step reference should not be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn invalid_step_reference_produces_warning() {
        let source = concat!(
            "workflow W:\n",
            "    steps:\n",
            "        - classify:\n",
            "            prompt:\n",
            "                system = \"classify\"\n",
            "        - generate:\n",
            "            prompt:\n",
            "                system = \"{{steps.clasify.intent}} generate\"\n",
        );
        let diags = compute(source, &[], &[], &default_config());
        assert!(
            diags.iter().any(|d| d.message.contains("unknown step 'clasify'")),
            "invalid step reference should be flagged: {:?}",
            diags
        );
    }
}
