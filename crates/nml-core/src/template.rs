use crate::span::Span;
use crate::types::TemplateSegment;

/// Parse a string containing `{{...}}` template expressions into segments.
///
/// The `string_start` byte offset is the position of the opening quote in the
/// source file, used to compute accurate spans for each expression.
pub fn parse_template_string(s: &str, string_start: usize) -> Vec<TemplateSegment> {
    let mut segments = Vec::new();
    let mut remaining = s;
    let mut offset = 0;

    while let Some(start) = remaining.find("{{") {
        if start > 0 {
            segments.push(TemplateSegment::Literal(remaining[..start].to_string()));
        }

        let after_open = &remaining[start + 2..];
        if let Some(end) = after_open.find("}}") {
            let raw = &after_open[..end];
            let expr = raw.trim();
            let expr_byte_start = string_start + offset + start;
            let expr_byte_end = expr_byte_start + 2 + end + 2;
            let span = Span::new(expr_byte_start, expr_byte_end);

            let parts: Vec<&str> = expr.splitn(2, '.').collect();
            let (namespace, path) = if parts.len() == 2 {
                (
                    parts[0].to_string(),
                    parts[1].split('.').map(|s| s.to_string()).collect(),
                )
            } else {
                (expr.to_string(), Vec::new())
            };

            segments.push(TemplateSegment::Expression {
                namespace,
                path,
                raw: raw.to_string(),
                span,
            });

            let consumed = start + 2 + end + 2;
            offset += consumed;
            remaining = &remaining[consumed..];
        } else {
            segments.push(TemplateSegment::Literal(remaining[start..].to_string()));
            return segments;
        }
    }

    if !remaining.is_empty() {
        segments.push(TemplateSegment::Literal(remaining.to_string()));
    }

    segments
}

/// Reconstruct the original string from template segments (for formatting/round-tripping).
pub fn segments_to_string(segments: &[TemplateSegment]) -> String {
    let mut out = String::new();
    for seg in segments {
        match seg {
            TemplateSegment::Literal(s) => out.push_str(s),
            TemplateSegment::Expression { raw, .. } => {
                out.push_str("{{");
                out.push_str(raw);
                out.push_str("}}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_expression() {
        let segs = parse_template_string("{{args.instructions}}", 0);
        assert_eq!(segs.len(), 1);
        match &segs[0] {
            TemplateSegment::Expression {
                namespace, path, ..
            } => {
                assert_eq!(namespace, "args");
                assert_eq!(path, &["instructions"]);
            }
            _ => panic!("expected expression"),
        }
    }

    #[test]
    fn literal_only() {
        let segs = parse_template_string("just a plain string", 0);
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], TemplateSegment::Literal(s) if s == "just a plain string"));
    }

    #[test]
    fn mixed_literal_and_expression() {
        let segs = parse_template_string("Hello {{args.name}}, welcome!", 0);
        assert_eq!(segs.len(), 3);
        assert!(matches!(&segs[0], TemplateSegment::Literal(s) if s == "Hello "));
        match &segs[1] {
            TemplateSegment::Expression {
                namespace, path, ..
            } => {
                assert_eq!(namespace, "args");
                assert_eq!(path, &["name"]);
            }
            _ => panic!("expected expression"),
        }
        assert!(matches!(&segs[2], TemplateSegment::Literal(s) if s == ", welcome!"));
    }

    #[test]
    fn multiple_expressions() {
        let segs = parse_template_string("{{args.a}} and {{steps.classify.intent}}", 0);
        assert_eq!(segs.len(), 3);
        match &segs[0] {
            TemplateSegment::Expression {
                namespace, path, ..
            } => {
                assert_eq!(namespace, "args");
                assert_eq!(path, &["a"]);
            }
            _ => panic!("expected expression"),
        }
        assert!(matches!(&segs[1], TemplateSegment::Literal(s) if s == " and "));
        match &segs[2] {
            TemplateSegment::Expression {
                namespace, path, ..
            } => {
                assert_eq!(namespace, "steps");
                assert_eq!(path, &["classify", "intent"]);
            }
            _ => panic!("expected expression"),
        }
    }

    #[test]
    fn unclosed_brace_treated_as_literal() {
        let segs = parse_template_string("Hello {{args.name", 0);
        assert_eq!(segs.len(), 2);
        assert!(matches!(&segs[0], TemplateSegment::Literal(s) if s == "Hello "));
        assert!(matches!(&segs[1], TemplateSegment::Literal(s) if s == "{{args.name"));
    }

    #[test]
    fn expression_with_whitespace() {
        let segs = parse_template_string("{{ args.instructions }}", 0);
        assert_eq!(segs.len(), 1);
        match &segs[0] {
            TemplateSegment::Expression {
                namespace, path, ..
            } => {
                assert_eq!(namespace, "args");
                assert_eq!(path, &["instructions"]);
            }
            _ => panic!("expected expression"),
        }
    }

    #[test]
    fn bare_namespace_no_path() {
        let segs = parse_template_string("{{input}}", 0);
        assert_eq!(segs.len(), 1);
        match &segs[0] {
            TemplateSegment::Expression {
                namespace, path, ..
            } => {
                assert_eq!(namespace, "input");
                assert!(path.is_empty());
            }
            _ => panic!("expected expression"),
        }
    }

    #[test]
    fn round_trip() {
        let original = "{{args.instructions}}\n\nBase rules with {{steps.classify.intent}}.";
        let segs = parse_template_string(original, 0);
        let reconstructed = segments_to_string(&segs);
        assert_eq!(reconstructed, original);
    }

    #[test]
    fn round_trip_with_whitespace_in_expression() {
        let original = "{{ args.instructions }}";
        let segs = parse_template_string(original, 0);
        let reconstructed = segments_to_string(&segs);
        assert_eq!(reconstructed, original);
    }

    // -------------------------------------------------------------------
    // Phase 5: Template parsing edge cases
    // -------------------------------------------------------------------

    #[test]
    fn deeply_nested_path() {
        let segs = parse_template_string("{{steps.generate.items.0.name}}", 0);
        assert_eq!(segs.len(), 1);
        match &segs[0] {
            TemplateSegment::Expression {
                namespace, path, ..
            } => {
                assert_eq!(namespace, "steps");
                assert_eq!(path, &["generate", "items", "0", "name"]);
            }
            _ => panic!("expected expression"),
        }
    }

    #[test]
    fn empty_braces() {
        let segs = parse_template_string("{{}}", 0);
        // Empty braces: should be literal or empty expression
        assert!(!segs.is_empty());
    }

    #[test]
    fn adjacent_expressions() {
        let segs = parse_template_string("{{a.b}}{{c.d}}", 0);
        assert_eq!(segs.len(), 2);
        assert!(
            matches!(&segs[0], TemplateSegment::Expression { namespace, .. } if namespace == "a")
        );
        assert!(
            matches!(&segs[1], TemplateSegment::Expression { namespace, .. } if namespace == "c")
        );
    }

    #[test]
    fn expression_at_start() {
        let segs = parse_template_string("{{args.x}} suffix", 0);
        assert_eq!(segs.len(), 2);
        assert!(matches!(&segs[0], TemplateSegment::Expression { .. }));
        assert!(matches!(&segs[1], TemplateSegment::Literal(s) if s == " suffix"));
    }

    #[test]
    fn expression_at_end() {
        let segs = parse_template_string("prefix {{args.x}}", 0);
        assert_eq!(segs.len(), 2);
        assert!(matches!(&segs[0], TemplateSegment::Literal(s) if s == "prefix "));
        assert!(matches!(&segs[1], TemplateSegment::Expression { .. }));
    }

    #[test]
    fn lone_closing_braces() {
        let segs = parse_template_string("hello }} world", 0);
        // Should be treated as literal text
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], TemplateSegment::Literal(s) if s == "hello }} world"));
    }

    #[test]
    fn single_open_brace_literal() {
        let segs = parse_template_string("hello { world", 0);
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], TemplateSegment::Literal(s) if s == "hello { world"));
    }

    #[test]
    fn mixed_literal_expression_literal() {
        let segs = parse_template_string("Hello {{args.name}}, welcome!", 0);
        assert_eq!(segs.len(), 3);
        assert!(matches!(&segs[0], TemplateSegment::Literal(s) if s == "Hello "));
        assert!(matches!(&segs[1], TemplateSegment::Expression { .. }));
        assert!(matches!(&segs[2], TemplateSegment::Literal(s) if s == ", welcome!"));
    }

    #[test]
    fn empty_string_input() {
        let segs = parse_template_string("", 0);
        assert!(
            segs.is_empty()
                || (segs.len() == 1
                    && matches!(&segs[0], TemplateSegment::Literal(s) if s.is_empty()))
        );
    }

    #[test]
    fn only_whitespace_input() {
        let segs = parse_template_string("   ", 0);
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], TemplateSegment::Literal(s) if s == "   "));
    }

    #[test]
    fn expression_with_single_segment_path() {
        let segs = parse_template_string("{{args.name}}", 0);
        match &segs[0] {
            TemplateSegment::Expression {
                namespace, path, ..
            } => {
                assert_eq!(namespace, "args");
                assert_eq!(path, &["name"]);
            }
            _ => panic!("expected expression"),
        }
    }

    #[test]
    fn round_trip_complex() {
        let original =
            "Hello {{args.name}}, your order {{steps.order.id}} is {{steps.status.value}}.";
        let segs = parse_template_string(original, 0);
        let reconstructed = segments_to_string(&segs);
        assert_eq!(reconstructed, original);
    }
}
