use nml_core::model::Constraint;
use nml_core::types::Value;

use crate::diagnostics::Diagnostic;

/// Evaluate a single constraint against a value.
pub fn evaluate(constraint: &Constraint, value: &Value) -> Option<Diagnostic> {
    match constraint {
        Constraint::Integer => {
            if let Value::Number(n) = value {
                if n.fract() != 0.0 {
                    return Some(Diagnostic::error(format!(
                        "expected an integer, got {n}"
                    )));
                }
            }
            None
        }
        Constraint::Min(min) => {
            if let Value::Number(n) = value {
                if n < min {
                    return Some(Diagnostic::error(format!(
                        "value {n} is less than minimum {min}"
                    )));
                }
            }
            None
        }
        Constraint::Max(max) => {
            if let Value::Number(n) = value {
                if n > max {
                    return Some(Diagnostic::error(format!(
                        "value {n} exceeds maximum {max}"
                    )));
                }
            }
            None
        }
        Constraint::MinLength(min) => {
            if let Value::String(s) = value {
                if s.len() < *min {
                    return Some(Diagnostic::error(format!(
                        "string length {} is less than minimum {min}",
                        s.len()
                    )));
                }
            }
            None
        }
        Constraint::MaxLength(max) => {
            if let Value::String(s) = value {
                if s.len() > *max {
                    return Some(Diagnostic::error(format!(
                        "string length {} exceeds maximum {max}",
                        s.len()
                    )));
                }
            }
            None
        }
        Constraint::Currency(allowed) => {
            if let Value::Money(m) = value {
                if !allowed.contains(&m.currency) {
                    return Some(Diagnostic::error(format!(
                        "currency '{}' is not in the allowed list: {:?}",
                        m.currency, allowed
                    )));
                }
            }
            None
        }
        _ => None,
    }
}
