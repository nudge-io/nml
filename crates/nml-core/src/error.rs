use crate::span::Span;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NmlError {
    #[error("{message}")]
    Parse {
        message: String,
        span: Span,
    },

    #[error("{message}")]
    Lex {
        message: String,
        span: Span,
    },

    #[error("{message}")]
    Validation {
        message: String,
        span: Span,
    },

    #[error("invalid money value: {message}")]
    InvalidMoney {
        message: String,
        span: Span,
    },
}

impl NmlError {
    pub fn span(&self) -> Span {
        match self {
            NmlError::Parse { span, .. }
            | NmlError::Lex { span, .. }
            | NmlError::Validation { span, .. }
            | NmlError::InvalidMoney { span, .. } => *span,
        }
    }

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

pub type NmlResult<T> = Result<T, NmlError>;
