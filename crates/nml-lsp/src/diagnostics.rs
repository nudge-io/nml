use nml_core::model::{EnumDef, ModelDef};
use nml_core::span::SourceMap;
use nml_validate::diagnostics::Severity;
use nml_validate::schema::SchemaValidator;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

/// Compute diagnostics for an NML source document.
pub fn compute(source: &str, models: &[ModelDef], enums: &[EnumDef]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let source_map = SourceMap::new(source);

    match nml_core::parse(source) {
        Ok(file) => {
            let mut resolver = nml_core::resolver::Resolver::new();
            resolver.register_file(&file);

            for err in resolver.find_duplicates() {
                diagnostics.push(nml_error_to_diagnostic(&err, &source_map));
            }

            if !models.is_empty() || !enums.is_empty() {
                let validator = SchemaValidator::new(models.to_vec(), enums.to_vec());
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
        }
        Err(err) => {
            diagnostics.push(nml_error_to_diagnostic(&err, &source_map));
        }
    }

    diagnostics
}

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
