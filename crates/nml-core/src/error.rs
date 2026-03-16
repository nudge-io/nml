//! Error types for NML parsing and validation.

use crate::span::Span;
use thiserror::Error;

/// All errors that can occur during NML parsing, lexing, or validation.
///
/// Each variant carries a human-readable message and a [`Span`] pointing
/// to the location in source where the error occurred.
#[derive(Debug, Error)]
pub enum NmlError {
    /// A syntax error during parsing.
    #[error("{message}")]
    Parse {
        message: String,
        span: Span,
    },

    /// A tokenization error during lexing.
    #[error("{message}")]
    Lex {
        message: String,
        span: Span,
    },

    /// A semantic validation error (e.g., duplicate declarations).
    #[error("{message}")]
    Validation {
        message: String,
        span: Span,
    },

    /// An invalid money literal (e.g., bad currency code).
    #[error("invalid money value: {message}")]
    InvalidMoney {
        message: String,
        span: Span,
    },
}

impl NmlError {
    /// Returns the source span where this error occurred.
    pub fn span(&self) -> Span {
        match self {
            NmlError::Parse { span, .. }
            | NmlError::Lex { span, .. }
            | NmlError::Validation { span, .. }
            | NmlError::InvalidMoney { span, .. } => *span,
        }
    }

    /// Returns the human-readable error message.
    pub fn message(&self) -> &str {
        match self {
            NmlError::Parse { message, .. }
            | NmlError::Lex { message, .. }
            | NmlError::Validation { message, .. }
            | NmlError::InvalidMoney { message, .. } => message,
        }
    }

    pub fn lex(message: impl Into<String>, span: Span) -> Self {
        NmlError::Lex {
            message: message.into(),
            span,
        }
    }

    pub fn parse(message: impl Into<String>, span: Span) -> Self {
        NmlError::Parse {
            message: message.into(),
            span,
        }
    }
}

/// Convenience type alias for results with [`NmlError`].
pub type NmlResult<T> = Result<T, NmlError>;
