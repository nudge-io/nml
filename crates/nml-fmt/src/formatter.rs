use nml_core::ast::*;
use nml_core::types::Value;

const INDENT: &str = "    ";

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
            match &f.field_type {
                nml_core::ast::FieldTypeExpr::Named(id) => out.push_str(&id.name),
                nml_core::ast::FieldTypeExpr::Array(id) => {
                    out.push_str("[]");
                    out.push_str(&id.name);
                }
            }
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
            match field_type {
                nml_core::ast::FieldTypeExpr::Named(id) => out.push_str(&id.name),
                nml_core::ast::FieldTypeExpr::Array(id) => {
                    out.push_str("[]");
                    out.push_str(&id.name);
                }
            }
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
    }
}

fn write_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}
