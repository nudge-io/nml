use std::collections::HashSet;
use std::sync::Arc;

use nml_core::ast::*;
use nml_core::model::{EnumDef, ModelDef};
use nml_core::span::SourceMap;
use nml_core::types::{TemplateSegment, Value};
use nml_validate::diagnostics::Severity;
use nml_validate::schema::{MembershipSemantics, SchemaValidator};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

/// Embedder-supplied semantics for the LSP.  Every method has a default that
/// returns nothing, so a generic adopter gets a purely structural LSP.
pub trait LanguageExtension: Send + Sync {
    /// Resolve valid identifiers under `namespace` for a given declaration.
    /// Return `None` to skip reference checking for this namespace.
    fn resolve_identifiers(
        &self,
        _decl: &Declaration,
        _namespace: &str,
    ) -> Option<HashSet<String>> {
        None
    }

    /// Markdown hover documentation for `{{namespace.path}}`.
    fn template_hover(&self, _namespace: &str, _path: &str) -> Option<String> {
        None
    }

    /// Built-in `@kind/name`-style references for completion menus.
    /// Returns `(label, description)` pairs.
    fn builtin_reference_completions(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// Configuration for diagnostics computation.
#[derive(Default, Clone)]
pub struct DiagnosticConfig {
    /// Valid template namespaces. If empty, all namespaces are accepted.
    pub template_namespaces: Vec<String>,
    /// Valid modifier names. If empty, all modifiers are accepted.
    pub modifiers: Vec<String>,
    /// Membership semantics passed through to `SchemaValidator`.
    pub membership: MembershipSemantics,
    /// Optional embedder-supplied language extension.
    pub language_extension: Option<Arc<dyn LanguageExtension>>,
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
                    .with_modifiers(config.modifiers.clone())
                    .with_membership_semantics(config.membership.clone());
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
                                end: Position::new(end.line as u32 - 1, end.column as u32 - 1),
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

            let ns: Vec<&str> = config
                .template_namespaces
                .iter()
                .map(|s| s.as_str())
                .collect();
            validate_templates(&file, &ns, &config, &source_map, &mut diagnostics);
        }
        Err(err) => {
            diagnostics.push(nml_error_to_diagnostic(&err, &source_map));
        }
    }

    diagnostics
}

fn validate_shared_property_templates(
    sp: &SharedProperty,
    decl: &Declaration,
    valid_ns: &[&str],
    config: &DiagnosticConfig,
    source_map: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) {
    match &sp.kind {
        SharedPropertyKind::Block(body) => {
            validate_body_templates(body, decl, valid_ns, config, source_map, diags);
        }
        SharedPropertyKind::Scalar(sv) => {
            validate_value_templates(&sv.value, decl, valid_ns, config, source_map, diags);
        }
    }
}

fn validate_templates(
    file: &File,
    valid_ns: &[&str],
    config: &DiagnosticConfig,
    source_map: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) {
    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => {
                validate_body_templates(&block.body, decl, valid_ns, config, source_map, diags);
            }
            DeclarationKind::Template(t) => {
                validate_value_templates(
                    &t.value.value,
                    decl,
                    valid_ns,
                    config,
                    source_map,
                    diags,
                );
            }
            DeclarationKind::Const(c) => {
                validate_value_templates(
                    &c.value.value,
                    decl,
                    valid_ns,
                    config,
                    source_map,
                    diags,
                );
            }
            DeclarationKind::Array(arr) => {
                for sp in &arr.body.shared_properties {
                    validate_shared_property_templates(
                        sp, decl, valid_ns, config, source_map, diags,
                    );
                }
                for prop in &arr.body.properties {
                    validate_value_templates(
                        &prop.value.value,
                        decl,
                        valid_ns,
                        config,
                        source_map,
                        diags,
                    );
                }
                for item in &arr.body.items {
                    validate_list_item_templates(item, decl, valid_ns, config, source_map, diags);
                }
            }
        }
    }
}

fn validate_body_templates(
    body: &Body,
    decl: &Declaration,
    valid_ns: &[&str],
    config: &DiagnosticConfig,
    source_map: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) {
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::Property(prop) => {
                validate_value_templates(
                    &prop.value.value,
                    decl,
                    valid_ns,
                    config,
                    source_map,
                    diags,
                );
            }
            BodyEntryKind::NestedBlock(nested) => {
                validate_body_templates(&nested.body, decl, valid_ns, config, source_map, diags);
            }
            BodyEntryKind::ListItem(item) => {
                validate_list_item_templates(item, decl, valid_ns, config, source_map, diags);
            }
            BodyEntryKind::SharedProperty(shared) => {
                validate_shared_property_templates(
                    shared, decl, valid_ns, config, source_map, diags,
                );
            }
            _ => {}
        }
    }
}

fn validate_list_item_templates(
    item: &ListItem,
    decl: &Declaration,
    valid_ns: &[&str],
    config: &DiagnosticConfig,
    source_map: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) {
    match &item.kind {
        ListItemKind::Named { body, .. } => {
            validate_body_templates(body, decl, valid_ns, config, source_map, diags);
        }
        ListItemKind::Shorthand(val) => {
            validate_value_templates(&val.value, decl, valid_ns, config, source_map, diags);
        }
        _ => {}
    }
}

fn validate_value_templates(
    value: &Value,
    decl: &Declaration,
    valid_ns: &[&str],
    config: &DiagnosticConfig,
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
                if !valid_ns.is_empty() && !valid_ns.contains(&namespace.as_str()) {
                    let start = source_map.location(span.start);
                    let end = source_map.location(span.end);
                    diags.push(Diagnostic {
                        range: Range {
                            start: Position::new(start.line as u32 - 1, start.column as u32 - 1),
                            end: Position::new(end.line as u32 - 1, end.column as u32 - 1),
                        },
                        severity: Some(DiagnosticSeverity::WARNING),
                        message: format!("unknown template namespace '{namespace}'"),
                        source: Some("nml".to_string()),
                        ..Default::default()
                    });
                }

                if let Some(ext) = &config.language_extension {
                    if let Some(known_ids) = ext.resolve_identifiers(decl, namespace) {
                        if let Some(id_name) = path.first() {
                            if !known_ids.contains(id_name.as_str()) {
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
                                        "unknown identifier '{id_name}' in '{namespace}' namespace"
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
        assert!(!diags.is_empty(), "parse error should produce diagnostics");
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

    fn config_with_namespaces(ns: &[&str]) -> DiagnosticConfig {
        DiagnosticConfig {
            template_namespaces: ns.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn valid_template_namespace_no_diagnostic() {
        let source = "service Svc:\n    instructions = \"{{args.instructions}} base\"\n";
        let config = config_with_namespaces(&["args", "steps"]);
        let diags = compute(source, &[], &[], &config);
        assert!(
            !diags.iter().any(|d| d.message.contains("namespace")),
            "valid namespace should not be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn empty_namespaces_accepts_all() {
        let source = "service Svc:\n    val = \"{{anything.goes}} ok\"\n";
        let diags = compute(source, &[], &[], &default_config());
        assert!(
            !diags.iter().any(|d| d.message.contains("namespace")),
            "empty namespace config should accept all namespaces: {:?}",
            diags
        );
    }

    #[test]
    fn unknown_namespace_flagged_when_configured() {
        let source = "service Svc:\n    val = \"{{foo.bar}} baz\"\n";
        let config = config_with_namespaces(&["args", "steps"]);
        let diags = compute(source, &[], &[], &config);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown template namespace 'foo'")),
            "unknown namespace should be flagged when namespaces are configured: {:?}",
            diags
        );
    }

    struct TestExtension {
        known: HashSet<String>,
    }

    impl LanguageExtension for TestExtension {
        fn resolve_identifiers(
            &self,
            _decl: &nml_core::ast::Declaration,
            namespace: &str,
        ) -> Option<HashSet<String>> {
            if namespace == "steps" {
                Some(self.known.clone())
            } else {
                None
            }
        }

        fn template_hover(&self, _namespace: &str, _path: &str) -> Option<String> {
            None
        }

        fn builtin_reference_completions(&self) -> Vec<(String, String)> {
            Vec::new()
        }
    }

    #[test]
    fn no_extension_skips_identifier_checking() {
        let source = concat!(
            "workflow W:\n",
            "    steps:\n",
            "        - classify:\n",
            "            prompt:\n",
            "                system = \"{{steps.nonexistent.intent}} generate\"\n",
        );
        let config = config_with_namespaces(&["args", "steps"]);
        let diags = compute(source, &[], &[], &config);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("unknown identifier")),
            "without extension, identifier checking should be skipped: {:?}",
            diags
        );
    }

    #[test]
    fn extension_valid_identifier_no_diagnostic() {
        let source = "service Svc:\n    val = \"{{steps.classify.intent}} x\"\n";
        let ext = TestExtension {
            known: ["classify".to_string()].into(),
        };
        let config = DiagnosticConfig {
            template_namespaces: vec!["steps".into()],
            language_extension: Some(Arc::new(ext)),
            ..Default::default()
        };
        let diags = compute(source, &[], &[], &config);
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("unknown identifier")),
            "valid identifier should not be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn extension_invalid_identifier_produces_warning() {
        let source = "service Svc:\n    val = \"{{steps.clasify.intent}} x\"\n";
        let ext = TestExtension {
            known: ["classify".to_string()].into(),
        };
        let config = DiagnosticConfig {
            template_namespaces: vec!["steps".into()],
            language_extension: Some(Arc::new(ext)),
            ..Default::default()
        };
        let diags = compute(source, &[], &[], &config);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown identifier 'clasify'")),
            "invalid identifier should be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn valid_bare_step_ref_no_diagnostic() {
        let source = concat!(
            "workflow W:\n",
            "    entrypoint = start\n",
            "    steps:\n",
            "        - start:\n",
            "            next = respond\n",
            "        - respond:\n",
            "            provider = \"groq\"\n",
        );
        let diags = compute(source, &[], &[], &default_config());
        assert!(
            !diags.iter().any(|d| d.message.contains("unresolved")),
            "valid bare step refs should not produce diagnostics: {:?}",
            diags
        );
    }

    #[test]
    fn invalid_bare_step_ref_produces_diagnostic() {
        let source = concat!(
            "workflow W:\n",
            "    entrypoint = start\n",
            "    steps:\n",
            "        - start:\n",
            "            next = nonexistent\n",
        );
        let diags = compute(source, &[], &[], &default_config());
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unresolved reference 'nonexistent'")),
            "invalid bare step ref should be flagged: {:?}",
            diags
        );
    }
}
