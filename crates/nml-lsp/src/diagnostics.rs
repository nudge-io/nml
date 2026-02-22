use nml_core::span::SourceMap;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

/// Compute diagnostics for an NML source document.
pub fn compute(source: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let source_map = SourceMap::new(source);

    match nml_core::parse(source) {
        Ok(file) => {
            let mut resolver = nml_core::resolver::Resolver::new();
            resolver.register_file(&file);

            for err in resolver.find_duplicates() {
                let span = err.span();
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
                    severity: Some(DiagnosticSeverity::WARNING),
                    message: err.message().to_string(),
                    source: Some("nml".to_string()),
                    ..Default::default()
                });
            }
        }
        Err(err) => {
            let span = err.span();
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
                severity: Some(DiagnosticSeverity::ERROR),
                message: err.message().to_string(),
                source: Some("nml".to_string()),
                ..Default::default()
            });
        }
    }

    diagnostics
}
