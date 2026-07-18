use std::fmt;

use nml_core::span::Span;

/// The severity level of a validation diagnostic.
#[derive(Debug, Clone)]
pub enum Severity {
    Error,
    Warning,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Error => write!(f, "error"),
            Severity::Warning => write!(f, "warning"),
        }
    }
}

/// A machine-applicable fix carried alongside a diagnostic (RFC 0030): the
/// exact replacement text and the exact span it replaces. Produced wherever
/// the validator *derives* a correction (e.g. the enum did-you-mean), so
/// editors can offer it as a one-keystroke quick-fix instead of leaving the
/// suggestion trapped in message prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The text to insert at `span` (for a string value: the bare content,
    /// without quotes — `span` covers the string's content, not its quotes).
    pub replacement: String,
    /// The exact range the replacement substitutes.
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub message: String,
    pub severity: Severity,
    pub span: Option<Span>,
    /// The source document this diagnostic belongs to, for multi-source
    /// loads (RFC 0030 schema packages) — spans from different sources are
    /// numerically ambiguous without it. `None` for single-source contexts
    /// and for cross-source findings (e.g. an inheritance cycle spanning
    /// files) that no one file owns.
    pub source: Option<String>,
    /// A machine-applicable fix, when one is derivable.
    pub suggestion: Option<Suggestion>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: Severity::Error,
            span: None,
            source: None,
            suggestion: None,
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: Severity::Warning,
            span: None,
            source: None,
            suggestion: None,
        }
    }

    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn with_suggestion(mut self, replacement: impl Into<String>, span: Span) -> Self {
        self.suggestion = Some(Suggestion {
            replacement: replacement.into(),
            span,
        });
        self
    }
}

/// Display: `[<source>: ]<severity>: <message>[ [start..end]]`. The suggestion
/// is deliberately not rendered — it is machine-facing; the human-facing hint
/// is already part of the message text.
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(source) = &self.source {
            write!(f, "{source}: ")?;
        }
        write!(f, "{}: {}", self.severity, self.message)?;
        if let Some(span) = self.span {
            write!(f, " [{}..{}]", span.start, span.end)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_without_span() {
        let diag = Diagnostic::error("something went wrong");
        assert_eq!(diag.to_string(), "error: something went wrong");
    }

    #[test]
    fn test_display_with_span() {
        let diag = Diagnostic::warning("looks odd").with_span(Span::new(4, 17));
        assert_eq!(diag.to_string(), "warning: looks odd [4..17]");
    }
}
