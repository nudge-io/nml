use nml_core::ast::*;
use nml_core::template;
use nml_core::types::Value;

const INDENT: &str = "    ";

fn format_field_type_expr_out(out: &mut String, expr: &FieldTypeExpr) {
    match expr {
        FieldTypeExpr::Named(id) => out.push_str(&id.name),
        FieldTypeExpr::Array(inner) => {
            out.push_str("[]");
            format_field_type_expr_out(out, inner);
        }
        FieldTypeExpr::Union(variants) => {
            out.push('(');
            for (i, v) in variants.iter().enumerate() {
                if i > 0 {
                    out.push_str(" | ");
                }
                format_field_type_expr_out(out, v);
            }
            out.push(')');
        }
    }
}

/// Format a parsed NML file into canonical form.
pub fn format(file: &File) -> String {
    let mut output = String::new();

    for (i, decl) in file.declarations.iter().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        format_declaration(&mut output, decl, 0);
    }

    output
}

fn format_declaration(out: &mut String, decl: &Declaration, depth: usize) {
    match &decl.kind {
        DeclarationKind::Block(block) => {
            write_indent(out, depth);
            out.push_str(&block.keyword.name);
            out.push(' ');
            out.push_str(&block.name.name);
            out.push_str(":\n");
            format_body(out, &block.body, depth + 1);
        }
        DeclarationKind::Array(arr) => {
            write_indent(out, depth);
            out.push_str("[]");
            out.push_str(&arr.item_keyword.name);
            out.push(' ');
            out.push_str(&arr.name.name);
            out.push_str(":\n");
            format_array_body(out, &arr.body, depth + 1);
        }
        DeclarationKind::Const(c) => {
            write_indent(out, depth);
            out.push_str("const ");
            out.push_str(&c.name.name);
            out.push_str(" = ");
            format_value(out, &c.value.value, depth);
            out.push('\n');
        }
        DeclarationKind::Template(t) => {
            write_indent(out, depth);
            out.push_str("template ");
            out.push_str(&t.name.name);
            out.push_str(":\n");
            write_indent(out, depth + 1);
            format_value(out, &t.value.value, depth + 1);
            out.push('\n');
        }
    }
}

fn format_body(out: &mut String, body: &Body, depth: usize) {
    for entry in &body.entries {
        format_body_entry(out, entry, depth);
    }
}

fn format_body_entry(out: &mut String, entry: &BodyEntry, depth: usize) {
    match &entry.kind {
        BodyEntryKind::Property(prop) => {
            write_indent(out, depth);
            out.push_str(&prop.name.name);
            out.push_str(" = ");
            format_value(out, &prop.value.value, depth);
            out.push('\n');
        }
        BodyEntryKind::NestedBlock(nb) => {
            write_indent(out, depth);
            out.push_str(&nb.name.name);
            out.push_str(":\n");
            format_body(out, &nb.body, depth + 1);
        }
        BodyEntryKind::Modifier(m) => {
            format_modifier(out, m, depth);
        }
        BodyEntryKind::SharedProperty(sp) => {
            write_indent(out, depth);
            out.push('.');
            out.push_str(&sp.name.name);
            out.push_str(":\n");
            format_body(out, &sp.body, depth + 1);
        }
        BodyEntryKind::ListItem(item) => {
            format_list_item(out, item, depth);
        }
        BodyEntryKind::FieldDefinition(f) => {
            write_indent(out, depth);
            out.push_str(&f.name.name);
            out.push(' ');
            format_field_type_expr_out(out, &f.field_type);
            if f.optional {
                out.push('?');
            }
            if let Some(ref default) = f.default_value {
                out.push_str(" = ");
                format_value(out, &default.value, depth);
            }
            out.push('\n');
        }
    }
}

fn format_modifier(out: &mut String, m: &Modifier, depth: usize) {
    write_indent(out, depth);
    out.push('|');
    out.push_str(&m.name.name);

    match &m.value {
        ModifierValue::Inline(val) => {
            out.push_str(" = ");
            format_value(out, &val.value, depth);
            out.push('\n');
        }
        ModifierValue::Block(items) => {
            out.push_str(":\n");
            for item in items {
                format_list_item(out, item, depth + 1);
            }
        }
        ModifierValue::TypeAnnotation { field_type, optional } => {
            out.push(' ');
            format_field_type_expr_out(out, field_type);
            if *optional {
                out.push('?');
            }
            out.push('\n');
        }
    }
}

fn format_array_body(out: &mut String, body: &ArrayBody, depth: usize) {
    for m in &body.modifiers {
        format_modifier(out, m, depth);
    }
    for sp in &body.shared_properties {
        write_indent(out, depth);
        out.push('.');
        out.push_str(&sp.name.name);
        out.push_str(":\n");
        format_body(out, &sp.body, depth + 1);
    }
    for prop in &body.properties {
        write_indent(out, depth);
        out.push_str(&prop.name.name);
        out.push_str(" = ");
        format_value(out, &prop.value.value, depth);
        out.push('\n');
    }
    if (!body.modifiers.is_empty() || !body.shared_properties.is_empty() || !body.properties.is_empty())
        && !body.items.is_empty()
    {
        out.push('\n');
    }
    for item in &body.items {
        format_list_item(out, item, depth);
    }
}

fn format_list_item(out: &mut String, item: &ListItem, depth: usize) {
    write_indent(out, depth);
    out.push_str("- ");

    match &item.kind {
        ListItemKind::Named { name, body } => {
            out.push_str(&name.name);
            out.push_str(":\n");
            format_body(out, body, depth + 1);
        }
        ListItemKind::Shorthand(val) => {
            format_value(out, &val.value, depth);
            out.push('\n');
        }
        ListItemKind::Reference(ident) => {
            out.push_str(&ident.name);
            out.push('\n');
        }
        ListItemKind::RoleRef(r) => {
            out.push_str(r);
            out.push('\n');
        }
    }
}

fn format_value(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::String(s) => {
            if s.contains('\n') {
                out.push_str("\"\"\"\n");
                for line in s.split('\n') {
                    write_indent(out, depth + 1);
                    for ch in line.chars() {
                        match ch {
                            '\\' => out.push_str("\\\\"),
                            c => out.push(c),
                        }
                    }
                    out.push('\n');
                }
                write_indent(out, depth + 1);
                out.push_str("\"\"\"");
            } else {
                out.push('"');
                for ch in s.chars() {
                    match ch {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\t' => out.push_str("\\t"),
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
        }
        Value::TemplateString(segments) => {
            let s = template::segments_to_string(segments);
            if s.contains('\n') {
                out.push_str("\"\"\"\n");
                for line in s.split('\n') {
                    write_indent(out, depth + 1);
                    for ch in line.chars() {
                        match ch {
                            '\\' => out.push_str("\\\\"),
                            c => out.push(c),
                        }
                    }
                    out.push('\n');
                }
                write_indent(out, depth + 1);
                out.push_str("\"\"\"");
            } else {
                out.push('"');
                for ch in s.chars() {
                    match ch {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\t' => out.push_str("\\t"),
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
        }
        Value::Number(n) => {
            if n.fract() == 0.0 {
                out.push_str(&format!("{}", *n as i64));
            } else {
                out.push_str(&format!("{n}"));
            }
        }
        Value::Money(m) => {
            out.push_str(&m.format_display());
        }
        Value::Bool(b) => {
            out.push_str(if *b { "true" } else { "false" });
        }
        Value::Duration(d) => {
            out.push('"');
            out.push_str(d);
            out.push('"');
        }
        Value::Path(p) => {
            out.push('"');
            out.push_str(p);
            out.push('"');
        }
        Value::Secret(s) => {
            out.push_str(s);
        }
        Value::RoleRef(r) => {
            out.push_str(r);
        }
        Value::Reference(r) => {
            out.push_str(r);
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_value(out, &item.value, depth);
            }
            out.push(']');
        }
        Value::Fallback(primary, fallback) => {
            format_value(out, &primary.value, depth);
            out.push_str(" | ");
            format_value(out, &fallback.value, depth);
        }
    }
}

fn write_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nml_core::parse;

    fn roundtrip(source: &str) {
        let file = parse(source).unwrap();
        let formatted = format(&file);
        let reparsed = parse(&formatted).unwrap_or_else(|e| {
            panic!(
                "failed to reparse formatted output:\n{}\nerror: {}",
                formatted,
                e.message()
            )
        });
        assert_eq!(
            file.declarations.len(),
            reparsed.declarations.len(),
            "declaration count mismatch after round-trip"
        );
    }

    fn idempotent(source: &str) {
        let file = parse(source).unwrap();
        let first = format(&file);
        let file2 = parse(&first).unwrap();
        let second = format(&file2);
        assert_eq!(first, second, "formatting is not idempotent");
    }

    // -------------------------------------------------------------------
    // Round-trip tests
    // -------------------------------------------------------------------

    #[test]
    fn roundtrip_simple_block() {
        roundtrip("service App:\n    port = 8080\n    host = \"localhost\"\n");
    }

    #[test]
    fn roundtrip_nested_block() {
        roundtrip("server App:\n    port = 8080\n    db:\n        backend = \"postgres\"\n");
    }

    #[test]
    fn roundtrip_const() {
        roundtrip("const Port = 8080\n");
    }

    #[test]
    fn roundtrip_multiple_blocks() {
        roundtrip("service A:\n    x = 1\n\nservice B:\n    y = 2\n");
    }

    #[test]
    fn roundtrip_list_items() {
        roundtrip("workflow W:\n    steps:\n        - step1:\n            x = 1\n        - step2:\n            y = 2\n");
    }

    #[test]
    fn roundtrip_shorthand_list() {
        roundtrip("service S:\n    items:\n        - \"a\"\n        - \"b\"\n");
    }

    #[test]
    fn roundtrip_array_property() {
        roundtrip("service App:\n    tags = [\"web\", \"api\"]\n");
    }

    #[test]
    fn roundtrip_bool_values() {
        roundtrip("service App:\n    debug = true\n    verbose = false\n");
    }

    #[test]
    fn roundtrip_secret() {
        roundtrip("service App:\n    key = $ENV.SECRET\n");
    }

    #[test]
    fn roundtrip_fallback() {
        roundtrip("const Port = $ENV.PORT | 3000\n");
    }

    #[test]
    fn roundtrip_model() {
        roundtrip("model User:\n    name string\n    age number\n");
    }

    #[test]
    fn roundtrip_enum() {
        roundtrip("enum Status:\n    - \"active\"\n    - \"inactive\"\n");
    }

    #[test]
    fn roundtrip_shared_property() {
        roundtrip("workflow W:\n    .defaults:\n        retries = 3\n    - step1:\n        x = 1\n");
    }

    #[test]
    fn roundtrip_modifier() {
        roundtrip("service App:\n    port = 8080\n    |allow:\n        - @role/admin\n");
    }

    // -------------------------------------------------------------------
    // Idempotency tests
    // -------------------------------------------------------------------

    #[test]
    fn idempotent_simple() {
        idempotent("service App:\n    port = 8080\n    host = \"localhost\"\n");
    }

    #[test]
    fn idempotent_nested() {
        idempotent("server S:\n    db:\n        url = \"postgres://localhost\"\n");
    }

    #[test]
    fn idempotent_complex() {
        idempotent("workflow W:\n    steps:\n        - s1:\n            provider = \"fast\"\n        - s2:\n            provider = \"slow\"\n");
    }

    // -------------------------------------------------------------------
    // Edge cases
    // -------------------------------------------------------------------

    #[test]
    fn format_empty_file() {
        let file = parse("").unwrap();
        let formatted = format(&file);
        assert!(formatted.is_empty() || formatted.trim().is_empty());
    }

    #[test]
    fn format_negative_number() {
        roundtrip("service App:\n    offset = -10\n");
    }

    #[test]
    fn format_float_number() {
        roundtrip("service App:\n    rate = 0.75\n");
    }

    #[test]
    fn format_empty_array() {
        roundtrip("service App:\n    tags = []\n");
    }

    #[test]
    fn format_single_item_array() {
        roundtrip("service App:\n    tags = [\"web\"]\n");
    }

    // -------------------------------------------------------------------
    // Fixture round-trip tests
    // -------------------------------------------------------------------

    #[test]
    fn roundtrip_fixture_minimal_service() {
        let source = std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/valid/minimal-service.nml")
        ).unwrap();
        roundtrip(&source);
        idempotent(&source);
    }

    #[test]
    fn roundtrip_fixture_full_service() {
        let source = std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/valid/full-service.nml")
        ).unwrap();
        roundtrip(&source);
        idempotent(&source);
    }

    #[test]
    fn roundtrip_fixture_web_server() {
        let source = std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/valid/web-server.nml")
        ).unwrap();
        roundtrip(&source);
        idempotent(&source);
    }

    #[test]
    fn roundtrip_fixture_role_templates() {
        let source = std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/valid/role-templates.nml")
        ).unwrap();
        roundtrip(&source);
        idempotent(&source);
    }

    #[test]
    fn roundtrip_fixture_secret_values() {
        let source = std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/valid/secret-values.nml")
        ).unwrap();
        roundtrip(&source);
        idempotent(&source);
    }

    #[test]
    fn roundtrip_fixture_money_values() {
        let source = std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/valid/money-values.nml")
        ).unwrap();
        roundtrip(&source);
        idempotent(&source);
    }

    #[test]
    fn roundtrip_fixture_pricing() {
        let source = std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/valid/pricing.nml")
        ).unwrap();
        roundtrip(&source);
        idempotent(&source);
    }
}
