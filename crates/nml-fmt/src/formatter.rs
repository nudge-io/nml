use nml_core::ast::*;
use nml_core::cst::Comment;
use nml_core::error::NmlResult;
use nml_core::template;
use nml_core::types::Value;

const INDENT: &str = "    ";
const INDENT_WIDTH: usize = 4;

/// Format a parsed NML file into canonical form.
///
/// This operates on the AST alone, which carries no comments; any comments
/// in the original source are not reproduced. Use [`format_source`] (or
/// [`format_with_comments`]) to format while preserving comments.
pub fn format(file: &File) -> String {
    Formatter::new(&[], "").format_file(file)
}

/// Parse and format NML source text into canonical form, preserving
/// comments. Parses via the lossless CST (so it accepts the full CST grammar,
/// and comments are read from the tree rather than a side-channel).
pub fn format_source(source: &str) -> NmlResult<String> {
    let (file, comments) = nml_core::cst::parse_with_comments(source)?;
    Ok(format_with_comments(&file, &comments, source))
}

/// Format a parsed NML file into canonical form, re-interleaving the given
/// comments (as returned by [`nml_core::cst::parse_with_comments`]).
///
/// `source` must be the text the file and comments were parsed from; it is
/// needed to map comment byte offsets to lines and columns. Own-line
/// comments are emitted before the construct that follows them, indented
/// at the deeper of the surrounding depth and their original indentation;
/// trailing comments stay at the end of their line. No comment is ever
/// dropped.
pub fn format_with_comments(file: &File, comments: &[Comment], source: &str) -> String {
    Formatter::new(comments, source).format_file(file)
}

struct Formatter<'a> {
    out: String,
    comments: &'a [Comment],
    /// Index of the next unemitted comment (comments are in source order).
    next: usize,
    /// Byte offsets of line starts in the original source.
    line_starts: Vec<usize>,
}

impl<'a> Formatter<'a> {
    fn new(comments: &'a [Comment], source: &str) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self {
            out: String::new(),
            comments,
            next: 0,
            line_starts,
        }
    }

    fn line_of(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= offset) - 1
    }

    fn col_of(&self, offset: usize) -> usize {
        offset - self.line_starts[self.line_of(offset)]
    }

    /// Emit every pending comment that begins before `pos`, each on its
    /// own line. Own-line comments keep their original nesting when it is
    /// deeper than the surrounding depth (e.g. a comment closing out a
    /// nested block); trailing comments that no construct claimed degrade
    /// to own-line at the surrounding depth rather than being dropped.
    fn emit_comments_before(&mut self, pos: usize, depth: usize) {
        while let Some(c) = self.comments.get(self.next) {
            if c.span.start >= pos {
                break;
            }
            let d = if c.own_line {
                depth.max(self.col_of(c.span.start) / INDENT_WIDTH)
            } else {
                depth
            };
            self.write_indent(d);
            self.out.push_str("//");
            self.out.push_str(&c.text);
            self.out.push('\n');
            self.next += 1;
        }
    }

    /// If the next pending comment trails code on the same source line as
    /// `anchor_offset`, emit it at the end of the current output line.
    fn emit_trailing_comment(&mut self, anchor_offset: usize) {
        if let Some(c) = self.comments.get(self.next) {
            if !c.own_line && self.line_of(c.span.start) == self.line_of(anchor_offset) {
                self.out.push_str(" //");
                self.out.push_str(&c.text);
                self.next += 1;
            }
        }
    }

    /// Emit all remaining comments (e.g. at end of file), keeping each
    /// own-line comment's original indentation.
    fn flush_remaining_comments(&mut self) {
        self.emit_comments_before(usize::MAX, 0);
    }

    fn write_indent(&mut self, depth: usize) {
        for _ in 0..depth {
            self.out.push_str(INDENT);
        }
    }

    fn format_file(mut self, file: &File) -> String {
        for (i, decl) in file.declarations.iter().enumerate() {
            if i > 0 {
                self.out.push('\n');
            }
            self.emit_comments_before(decl.span.start, 0);
            self.declaration(decl, 0);
        }
        self.flush_remaining_comments();
        self.out
    }

    fn declaration(&mut self, decl: &Declaration, depth: usize) {
        match &decl.kind {
            DeclarationKind::Block(block) => {
                self.write_indent(depth);
                self.out.push_str(&block.keyword.name);
                self.out.push(' ');
                self.out.push_str(&block.name.name);
                if !block.extends.is_empty() {
                    self.out.push_str(" is ");
                    for (i, parent) in block.extends.iter().enumerate() {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        self.out.push_str(&parent.name);
                    }
                }
                let body_empty = block.body.entries.is_empty();
                if body_empty && !block.extends.is_empty() {
                    self.emit_trailing_comment(decl.span.start);
                    self.out.push('\n');
                } else {
                    self.out.push(':');
                    self.emit_trailing_comment(decl.span.start);
                    self.out.push('\n');
                    self.body(&block.body, depth + 1);
                }
            }
            DeclarationKind::Array(arr) => {
                self.write_indent(depth);
                self.out.push_str("[]");
                self.out.push_str(&arr.item_keyword.name);
                self.out.push(' ');
                self.out.push_str(&arr.name.name);
                self.out.push(':');
                self.emit_trailing_comment(decl.span.start);
                self.out.push('\n');
                self.array_body(&arr.body, depth + 1);
            }
            DeclarationKind::Const(c) => {
                self.write_indent(depth);
                self.out.push_str("const ");
                self.out.push_str(&c.name.name);
                self.out.push_str(" = ");
                format_value(&mut self.out, &c.value.value, depth);
                self.emit_trailing_comment(c.value.span.end.saturating_sub(1));
                self.out.push('\n');
            }
            DeclarationKind::Template(t) => {
                self.write_indent(depth);
                self.out.push_str("template ");
                self.out.push_str(&t.name.name);
                self.out.push(':');
                self.emit_trailing_comment(decl.span.start);
                self.out.push('\n');
                self.emit_comments_before(t.value.span.start, depth + 1);
                self.write_indent(depth + 1);
                format_value(&mut self.out, &t.value.value, depth + 1);
                self.emit_trailing_comment(t.value.span.end.saturating_sub(1));
                self.out.push('\n');
            }
            DeclarationKind::OneOf(oneof) => {
                self.write_indent(depth);
                self.out.push_str("oneof ");
                self.out.push_str(&oneof.name.name);
                self.out.push_str(" by ");
                self.out.push_str(&oneof.discriminator.name);
                if let Some(type_id) = &oneof.discriminator_type {
                    self.out.push_str(" as ");
                    self.out.push_str(&type_id.name);
                }
                if let Some(default) = &oneof.default_discriminator {
                    self.out.push_str(" = ");
                    format_value(&mut self.out, &default.value, depth);
                }
                self.out.push(':');
                self.emit_trailing_comment(decl.span.start);
                self.out.push('\n');

                // Align the `->` arrows on the widest quoted value so arms
                // read as a tidy table.
                let quoted: Vec<String> =
                    oneof.arms.iter().map(|a| quote_string(&a.value)).collect();
                let max_w = quoted.iter().map(|q| q.chars().count()).max().unwrap_or(0);
                for (arm, q) in oneof.arms.iter().zip(&quoted) {
                    self.emit_comments_before(arm.value_span.start, depth + 1);
                    self.write_indent(depth + 1);
                    self.out.push_str(q);
                    for _ in 0..(max_w - q.chars().count()) {
                        self.out.push(' ');
                    }
                    self.out.push_str(" -> ");
                    self.out.push_str(&arm.model.name);
                    self.emit_trailing_comment(arm.model.span.start);
                    self.out.push('\n');
                }
            }
        }
    }

    fn body(&mut self, body: &Body, depth: usize) {
        let entries = &body.entries;
        let mut i = 0;
        while i < entries.len() {
            // A run of consecutive arms formats as one aligned table
            // (RFC 0007), matching `oneof` arm alignment.
            if matches!(entries[i].kind, BodyEntryKind::Arm(_)) {
                let run = entries[i..]
                    .iter()
                    .take_while(|e| matches!(e.kind, BodyEntryKind::Arm(_)))
                    .count();
                self.arm_run(&entries[i..i + run], depth);
                i += run;
            } else {
                // Canonical grouping, matching `array_body`: a blank line
                // separates a shared-property header from the items it
                // covers — the same construct must not format differently
                // by nesting context.
                if i > 0
                    && matches!(entries[i].kind, BodyEntryKind::ListItem(_))
                    && matches!(entries[i - 1].kind, BodyEntryKind::SharedProperty(_))
                {
                    self.out.push('\n');
                }
                self.emit_comments_before(entries[i].span.start, depth);
                self.body_entry(&entries[i], depth);
                i += 1;
            }
        }
    }

    /// Emit a run of consecutive arms with the `->` arrows aligned on the
    /// widest selector, so a routing block reads as a tidy table — the same
    /// treatment `oneof` arms get.
    fn arm_run(&mut self, entries: &[BodyEntry], depth: usize) {
        let selector_text = |arm: &Arm| match &arm.selector {
            ArmSelector::Role(r) => r.clone(),
            ArmSelector::Else => "else".to_string(),
        };
        let max_w = entries
            .iter()
            .filter_map(|e| match &e.kind {
                BodyEntryKind::Arm(a) => Some(selector_text(a).chars().count()),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        for entry in entries {
            let BodyEntryKind::Arm(arm) = &entry.kind else {
                continue;
            };
            self.emit_comments_before(entry.span.start, depth);
            self.write_indent(depth);
            let selector = selector_text(arm);
            self.out.push_str(&selector);
            for _ in 0..(max_w - selector.chars().count()) {
                self.out.push(' ');
            }
            self.out.push_str(" -> ");
            match &arm.target {
                ArmTarget::Reference(id) => self.out.push_str(&id.name),
                // A path/url literal (RFC 0007 §6) renders quoted, like a
                // `oneof` arm's string key.
                ArmTarget::Literal { value, .. } => self.out.push_str(&quote_string(value)),
            }
            self.emit_trailing_comment(arm.target.span().start);
            self.out.push('\n');
        }
    }

    fn body_entry(&mut self, entry: &BodyEntry, depth: usize) {
        match &entry.kind {
            BodyEntryKind::Property(prop) => {
                self.property(prop, depth);
            }
            BodyEntryKind::NestedBlock(nb) => {
                self.write_indent(depth);
                self.out.push_str(&nb.name.name);
                // RFC 0015: re-emit the `as <Variant>` annotation. LOAD-BEARING —
                // fmt reconstructs from the AST, so dropping it would silently
                // re-disambiguate a same-class union (typed → anonymous →
                // structural first-wins), changing the config's meaning. The
                // round-trip test locks this.
                self.emit_type_annotation(&nb.body);
                self.out.push(':');
                self.emit_trailing_comment(nb.name.span.start);
                self.out.push('\n');
                self.body(&nb.body, depth + 1);
            }
            BodyEntryKind::Modifier(m) => {
                self.modifier(m, depth);
            }
            BodyEntryKind::SharedProperty(sp) => {
                self.shared_property(sp, depth);
            }
            BodyEntryKind::ListItem(item) => {
                self.list_item(item, depth);
            }
            // Arms are grouped and aligned by `body()`; a lone arm reaching
            // here formats as a run of one.
            BodyEntryKind::Arm(_) => {
                self.arm_run(std::slice::from_ref(entry), depth);
            }
            BodyEntryKind::FieldDefinition(f) => {
                self.write_indent(depth);
                self.out.push_str(&f.name.name);
                self.out.push(' ');
                self.out.push_str(&f.field_type.to_string());
                // Canonical suffix order is `?+` (RFC 0005 §7, §16); the flags
                // are independent, so they always render canonically.
                if f.optional {
                    self.out.push('?');
                }
                if f.shorthand {
                    self.out.push('+');
                }
                if let Some(ref default) = f.default_value {
                    self.out.push_str(" = ");
                    format_value(&mut self.out, &default.value, depth);
                }
                render_directives(&mut self.out, &f.directives, depth);
                self.emit_trailing_comment(entry.span.end.saturating_sub(1));
                self.out.push('\n');
            }
        }
    }

    fn property(&mut self, prop: &Property, depth: usize) {
        self.write_indent(depth);
        self.out.push_str(&prop.name.name);
        self.out.push_str(" = ");
        format_value(&mut self.out, &prop.value.value, depth);
        self.emit_trailing_comment(prop.value.span.end.saturating_sub(1));
        self.out.push('\n');
    }

    fn modifier(&mut self, m: &Modifier, depth: usize) {
        self.write_indent(depth);
        self.out.push('|');
        self.out.push_str(&m.name.name);

        match &m.value {
            ModifierValue::Inline(val) => {
                self.out.push_str(" = ");
                format_value(&mut self.out, &val.value, depth);
                self.emit_trailing_comment(val.span.end.saturating_sub(1));
                self.out.push('\n');
            }
            ModifierValue::Block(items) => {
                self.out.push(':');
                self.emit_trailing_comment(m.name.span.start);
                self.out.push('\n');
                for item in items {
                    self.emit_comments_before(item.span.start, depth + 1);
                    self.list_item(item, depth + 1);
                }
            }
            ModifierValue::TypeAnnotation {
                field_type,
                optional,
                directives,
            } => {
                self.out.push(' ');
                self.out.push_str(&field_type.to_string());
                if *optional {
                    self.out.push('?');
                }
                render_directives(&mut self.out, directives, depth);
                self.emit_trailing_comment(m.name.span.start);
                self.out.push('\n');
            }
        }
    }

    fn array_body(&mut self, body: &ArrayBody, depth: usize) {
        for m in &body.modifiers {
            self.emit_comments_before(m.name.span.start, depth);
            self.modifier(m, depth);
        }
        for sp in &body.shared_properties {
            self.emit_comments_before(sp.name.span.start, depth);
            self.shared_property(sp, depth);
        }
        for prop in &body.properties {
            self.emit_comments_before(prop.name.span.start, depth);
            self.property(prop, depth);
        }
        if (!body.modifiers.is_empty()
            || !body.shared_properties.is_empty()
            || !body.properties.is_empty())
            && !body.items.is_empty()
        {
            self.out.push('\n');
        }
        for item in &body.items {
            self.emit_comments_before(item.span.start, depth);
            self.list_item(item, depth);
        }
    }

    /// Emit an RFC 0015 nominal type annotation (` as <Variant>`) when the body
    /// carries one. The single point both the field and list-element headers use,
    /// so the annotation is never emitted inconsistently or dropped.
    fn emit_type_annotation(&mut self, body: &Body) {
        if let Some(ann) = &body.type_annotation {
            self.out.push_str(" as ");
            self.out.push_str(&ann.name);
        }
    }

    fn list_item(&mut self, item: &ListItem, depth: usize) {
        self.write_indent(depth);
        self.out.push_str("- ");

        match &item.kind {
            ListItemKind::Named { name, body } => {
                self.out.push_str(&name.name);
                // RFC 0015: re-emit `as <Variant>` at the list-element level too
                // (see `body_entry`). Same data-integrity requirement.
                self.emit_type_annotation(body);
                self.out.push(':');
                self.emit_trailing_comment(item.span.start);
                self.out.push('\n');
                self.body(body, depth + 1);
            }
            ListItemKind::Shorthand { value, body } => {
                format_value(&mut self.out, &value.value, depth);
                if let Some(body) = body {
                    // `- "/api":` + indented body (scalar-key-with-body).
                    self.out.push(':');
                    self.emit_trailing_comment(item.span.start);
                    self.out.push('\n');
                    self.body(body, depth + 1);
                } else {
                    self.emit_trailing_comment(item.span.end.saturating_sub(1));
                    self.out.push('\n');
                }
            }
            ListItemKind::Reference(ident) => {
                self.out.push_str(&ident.name);
                self.emit_trailing_comment(item.span.end.saturating_sub(1));
                self.out.push('\n');
            }
            ListItemKind::Role(r) => {
                self.out.push_str(r);
                self.emit_trailing_comment(item.span.end.saturating_sub(1));
                self.out.push('\n');
            }
        }
    }

    fn shared_property(&mut self, sp: &SharedProperty, depth: usize) {
        self.write_indent(depth);
        self.out.push('.');
        self.out.push_str(&sp.name.name);
        match &sp.kind {
            SharedPropertyKind::Block(body) => {
                self.out.push(':');
                self.emit_trailing_comment(sp.name.span.start);
                self.out.push('\n');
                self.body(body, depth + 1);
            }
            SharedPropertyKind::Scalar(val) => {
                self.out.push_str(" = ");
                format_value(&mut self.out, &val.value, depth);
                self.emit_trailing_comment(val.span.end.saturating_sub(1));
                self.out.push('\n');
            }
        }
    }
}

/// Render a string as a single-line, double-quoted NML literal with the same
/// escaping rules the lexer accepts. Used for `oneof` discriminator values.
fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn format_value(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::String(s) => format_string(out, s, depth, true),
        Value::TemplateString(segments) => {
            // Braces stay raw: the reparse re-detects the template from
            // them, so the value's TYPE survives the round-trip.
            let s = template::segments_to_string(segments);
            format_string(out, &s, depth, false);
        }
        // Number's Display is exact and scale-preserving (RFC 0016):
        // the stored decimal renders with its own written scale, so
        // `2.50` survives formatting.
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::Money(m) => {
            out.push_str(&m.format_display());
        }
        // Canonical duration form is ATTACHED (`30s`) — a unit is a
        // suffix, unlike a currency code (RFC 0017 §6). Display renders
        // the stored value: the authored unit survives (never rescaled),
        // magnitude spelling normalizes (`030s` → `30s`).
        Value::Duration(d) => {
            out.push_str(&d.to_string());
        }
        Value::Bool(b) => {
            out.push_str(if *b { "true" } else { "false" });
        }
        Value::Secret(s) => {
            out.push_str(s);
        }
        Value::Role(r) => {
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

/// Render trailing field directives (RFC 0032) canonically: one space,
/// `#name` / `#name(arg)`, source order (stable ⇒ idempotent). Rendering is
/// LOAD-BEARING — fmt formats from the AST, so an unrendered construct would
/// be silently deleted. Shared by plain fields and modifier declarations.
fn render_directives(out: &mut String, directives: &[nml_core::types::Directive], depth: usize) {
    for d in directives {
        out.push_str(" #");
        out.push_str(&d.name);
        if let Some(ref arg) = d.arg {
            out.push('(');
            format_value(out, &arg.value, depth);
            out.push(')');
        }
    }
}

/// `escape_braces` distinguishes literal strings (true — a `{{` renders as
/// `\u{7B}{`, so raw-text template detection cannot re-fire on reparse and
/// the value stays a String) from template strings (false — braces are the
/// syntax the reparse must re-detect).
fn format_string(out: &mut String, s: &str, depth: usize, escape_braces: bool) {
    if s.contains('\n') {
        out.push_str("\"\"\"\n");
        for line in s.split('\n') {
            for _ in 0..(depth + 1) {
                out.push_str(INDENT);
            }
            // Edge spaces are CONTENT the transport layer would eat: a raw
            // leading space is indistinguishable from indentation on
            // reparse (min-indent would strip it) and a raw trailing space
            // is one editor trim-on-save away from a changed value — so a
            // line's first and last space render as `\s` (Java's escape,
            // adopted for exactly these emissions).
            let bytes = line.as_bytes();
            let protect_first = bytes.first() == Some(&b' ');
            let protect_last = line.len() > 1 && bytes.last() == Some(&b' ');
            let mut chars = line.char_indices().peekable();
            // A raw `"""` inside the body would close the string early on
            // reparse — escape the THIRD quote of any consecutive run (and
            // only that one), so quote-heavy content (JSON) stays readable
            // while a closing-delimiter run can never form.
            let mut quote_run = 0usize;
            while let Some((i, ch)) = chars.next() {
                if (i == 0 && protect_first) || (i == line.len() - 1 && protect_last) {
                    out.push_str("\\s");
                    quote_run = 0;
                    continue;
                }
                if ch == '"' {
                    quote_run += 1;
                    if quote_run == 3 {
                        out.push_str("\\\"");
                        quote_run = 0;
                        continue;
                    }
                } else {
                    quote_run = 0;
                }
                if ch == '\t' {
                    // A raw tab at the start of a rendered line would sit in
                    // the indentation run and fail NML0005 on reparse; escape
                    // uniformly (matching the single-line branch) so the
                    // formatter can never emit a document the parser rejects.
                    out.push_str("\\t");
                } else if escape_braces && ch == '{' && chars.peek().map(|(_, c)| *c) == Some('{') {
                    out.push_str("\\u{7B}");
                } else {
                    push_value_char(out, ch);
                }
            }
            out.push('\n');
        }
        for _ in 0..(depth + 1) {
            out.push_str(INDENT);
        }
        out.push_str("\"\"\"");
    } else {
        out.push('"');
        let mut chars = s.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '"' => out.push_str("\\\""),
                '\t' => out.push_str("\\t"),
                '{' if escape_braces && chars.peek() == Some(&'{') => out.push_str("\\u{7B}"),
                c => push_value_char(out, c),
            }
        }
        out.push('"');
    }
}

/// Render one string-value character, escaping what the parser's
/// source-character policy bans raw (`must_escape` is the policy's own
/// predicate, so formatted output can never carry a character the parser
/// rejects — a formatter emitting a raw ESC or bidi control would be
/// writing NML0017/NML0018 into the file it produces).
fn push_value_char(out: &mut String, ch: char) {
    use std::fmt::Write as _;
    match ch {
        '\\' => out.push_str("\\\\"),
        '\r' => out.push_str("\\r"),
        c if nml_core::source_policy::must_escape(c) => {
            let _ = write!(out, "\\u{{{:X}}}", c as u32);
        }
        c => out.push(c),
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

    /// RFC 0017 §6: canonical duration form is ATTACHED and faithful —
    /// a spaced unit joins (`30 s` → `30s`), magnitude spelling
    /// normalizes (`030s` → `30s`), and the authored unit is NEVER
    /// rescaled (`72h` stays `72h`, not `259200s`) — unlike money, whose
    /// canonical form is spaced (`19.99 USD`): a currency code is a noun,
    /// a duration unit is a suffix.
    #[test]
    fn duration_formatting_attached_and_faithful() {
        for (source_value, expected) in [
            ("30s", "30s"),
            ("30 s", "30s"),
            ("030s", "30s"),
            ("72h", "72h"),
            ("30000ms", "30000ms"),
            ("0s", "0s"),
        ] {
            let file = parse(&format!("service App:\n    x = {source_value}\n")).unwrap();
            let formatted = format(&file);
            assert!(
                formatted.contains(&format!("x = {expected}\n")),
                "{source_value}: got {formatted:?}"
            );
        }
        idempotent("service App:\n    x = 72h\n    y = 500 ms\n");
        roundtrip("service App:\n    x = 30s\n");
    }

    /// RFC 0016 §1.10: the exact-decimal fixed-point table. Every output
    /// of the pre-decimal formatter re-formats to itself (no save-churn on
    /// previously formatted files) — with exactly one exception, `-0.0`,
    /// whose sign was presentation the model no longer fakes.
    #[test]
    fn number_formatting_fixed_point_table() {
        for (source_value, expected) in [
            // Old-formatter outputs: all fixed points.
            ("2.5", "2.5"),
            ("8080.0", "8080.0"),
            ("8080", "8080"),
            ("-5", "-5"),
            ("0.75", "0.75"),
            ("9007199254740993", "9007199254740993"),
            // Written scale now survives (the old formatter collapsed it —
            // a change only where information was previously destroyed).
            ("2.50", "2.50"),
            ("0.20", "0.20"),
            ("8080.000", "8080.000"),
            // Leading zeros still canonicalize.
            ("007", "7"),
            // The one non-fixed-point: −0 is unrepresentable.
            ("-0.0", "0.0"),
        ] {
            let file = parse(&format!("service App:\n    x = {source_value}\n")).unwrap();
            let formatted = format(&file);
            assert!(
                formatted.contains(&format!("x = {expected}\n")),
                "{source_value}: got {formatted:?}"
            );
            // And the output is itself a fixed point.
            let again = format(&parse(&formatted).unwrap());
            assert_eq!(formatted, again, "{source_value} must reach a fixed point");
        }
    }

    /// RFC 0018: facet lists canonicalize (single spaces, ", " joins)
    /// and round-trip idempotently — fmt renders types through the one
    /// shared `FieldTypeExpr` renderer.
    #[test]
    fn facet_lists_format_canonically() {
        let src = "model server:\n    port number( min=1 ,  max =65535 )\n    step set<number(multipleOf   =0.01)>\n";
        let once = format(&parse(src).unwrap());
        assert!(
            once.contains("port number(min = 1, max = 65535)"),
            "{once:?}"
        );
        assert!(
            once.contains("step set<number(multipleOf = 0.01)>"),
            "{once:?}"
        );
        let twice = format(&parse(&once).unwrap());
        assert_eq!(once, twice, "facet rendering must be a fixed point");
    }

    /// Every extreme literal the language accepts survives formatting:
    /// the boundaries fixture (34-digit integers, clamped 10^34, scale
    /// preservation) formats idempotently and value-preservingly, and
    /// synthesized subnormal-band / 6176-scale-zero literals — too long
    /// to live readably in a fixture file — are appended in-test. fmt
    /// is Display-driven, so this pins the scale-preservation contract
    /// at the domain's edges.
    #[test]
    fn number_boundaries_fixture_formats_losslessly() {
        let fixture = include_str!("../../../tests/fixtures/valid/number-boundaries.nml");
        // Extremes the fixture omits (a 6178-char line is not a readable
        // fixture): the subnormal band and the deepest zero scale.
        let src = format!(
            "{fixture}\nextremes Edge:\n    subnormal = 0.{}5\n    deepZero = 0.{}\n",
            "0".repeat(6150),
            "0".repeat(6176),
        );
        let src = src.as_str();
        let once = format(&parse(src).unwrap());
        let twice = format(&parse(&once).unwrap());
        assert_eq!(once, twice, "fmt must be idempotent on boundary literals");
        // Value preservation: reparse and compare every number
        // semantically (numeric Eq — cosmetic moves allowed, value
        // drift not).
        let a = nml_core::cst::parse_to_ast(src).unwrap();
        let b = nml_core::cst::parse_to_ast(&once).unwrap();
        let nums = |f: &nml_core::ast::File| {
            let mut out = Vec::new();
            fn walk_body(b: &nml_core::ast::Body, out: &mut Vec<nml_core::types::Number>) {
                for e in &b.entries {
                    match &e.kind {
                        nml_core::ast::BodyEntryKind::Property(p) => {
                            walk_value(&p.value.value, out)
                        }
                        nml_core::ast::BodyEntryKind::NestedBlock(n) => walk_body(&n.body, out),
                        nml_core::ast::BodyEntryKind::ListItem(i) => {
                            if let nml_core::ast::ListItemKind::Named { body, .. } = &i.kind {
                                walk_body(body, out);
                            }
                        }
                        _ => {}
                    }
                }
            }
            fn walk_value(v: &nml_core::types::Value, out: &mut Vec<nml_core::types::Number>) {
                match v {
                    nml_core::types::Value::Number(n) => out.push(*n),
                    nml_core::types::Value::Array(items) => {
                        for it in items {
                            walk_value(&it.value, out);
                        }
                    }
                    _ => {}
                }
            }
            for d in &f.declarations {
                if let nml_core::ast::DeclarationKind::Block(blk) = &d.kind {
                    walk_body(&blk.body, &mut out);
                }
            }
            out
        };
        let (na, nb) = (nums(&a), nums(&b));
        assert_eq!(na.len(), nb.len(), "number count must survive fmt");
        assert!(!na.is_empty(), "fixture must actually contain numbers");
        for (x, y) in na.iter().zip(&nb) {
            assert_eq!(x, y, "value drift through fmt");
            assert_eq!(x.scale(), y.scale(), "scale drift through fmt");
        }
    }

    /// The formatter can never emit a document the parser rejects: string
    /// values holding policy-banned characters (reachable only via `\u{…}`
    /// escapes) render back AS escapes. `roundtrip` is the enforcement —
    /// a raw ESC/bidi/CR in the output would fail the reparse with
    /// NML0016–NML0018.
    #[test]
    fn banned_value_characters_render_as_escapes() {
        let source = "service App:\n    ansi = \"\\u{1B}[0m\"\n    bidi = \"\\u{202E}\"\n    cr = \"a\\rb\"\n";
        roundtrip(source);
        idempotent(source);
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("\\u{1B}[0m"), "{formatted}");
        assert!(formatted.contains("\\u{202E}"), "{formatted}");
        assert!(formatted.contains("a\\rb"), "{formatted}");
    }

    /// Edge spaces in multiline values (writable via `\u{20}`, which
    /// survives dedent under the text-block order) round-trip: the
    /// formatter re-escapes a line's first/last space so reparse dedent
    /// and editor whitespace-trimming can't eat them. Idempotence is the
    /// enforcement — a lost space would change the second format.
    #[test]
    fn edge_spaces_in_multiline_values_are_protected() {
        let source = "service App:\n    x = \"\"\"\n        \\u{20}lead\n        tail\\u{20}\n        \"\"\"\n";
        roundtrip(source);
        idempotent(source);
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("\\slead"), "{formatted}");
        assert!(formatted.contains("tail\\s"), "{formatted}");
    }

    /// A value line beginning with tab content (writable via `\t`) must
    /// re-emit the tab AS an escape: raw, it would land inside the rendered
    /// line's indentation run and fail NML0005 on reparse — the formatter
    /// may never produce a document the parser rejects.
    #[test]
    fn leading_tab_content_renders_escaped() {
        let source = "service App:\n    x = \"\"\"\n        \\ta\n        b\n        \"\"\"\n";
        roundtrip(source);
        idempotent(source);
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("\\ta"), "{formatted}");
    }

    /// A multiline value containing `"""` (writable via `\"\"\"`) must not
    /// re-emit a raw closing run — the third quote of any consecutive run
    /// renders escaped, so the reparse sees the same value instead of a
    /// truncated string (this was a live data-destroying round-trip bug).
    /// Quote PAIRS stay raw so JSON-ish content remains readable.
    #[test]
    fn triple_quotes_in_multiline_values_roundtrip() {
        let source = "service App:\n    x = \"\"\"\n        a\\\"\\\"\\\"b {\"k\": \"\"}\n        c\n        \"\"\"\n";
        roundtrip(source);
        idempotent(source);
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("a\"\"\\\"b"), "{formatted}");
        assert!(formatted.contains("{\"k\": \"\"}"), "{formatted}");
    }

    /// Value TYPES survive the round-trip across the template boundary: a
    /// literal `{{` (writable only via `\u{7B}`) re-escapes its first brace
    /// so raw-text template detection cannot re-fire, while a real template
    /// keeps raw braces and re-detects.
    #[test]
    fn literal_braces_stay_string_templates_stay_templates() {
        let source = "service App:\n    lit = \"\\u{7B}\\u{7B}x}}\"\n    tpl = \"{{args.x}}\"\n";
        roundtrip(source);
        idempotent(source);
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("\\u{7B}{x}}"), "{formatted}");
        assert!(formatted.contains("\"{{args.x}}\""), "{formatted}");
    }

    /// RFC 0032: directives SURVIVE formatting (rendering them is load-bearing
    /// — fmt formats from the AST, so an unrendered field would be silently
    /// deleted), in canonical one-space form, args included; and formatting is
    /// idempotent over them.
    #[test]
    fn test_directives_survive_formatting_canonically() {
        let source = "model server:\n    ceiling capabilitySet   #live\n    hosts set<string> #live   #key(\"host\")\n";
        let formatted = format_source(source).unwrap();
        assert!(
            formatted.contains("ceiling capabilitySet #live\n"),
            "{formatted}"
        );
        assert!(
            formatted.contains("hosts set<string> #live #key(\"host\")\n"),
            "{formatted}"
        );
        idempotent(source);
    }

    /// RFC 0032: `set<T>` canonicalization comes from the canonical renderer —
    /// `set <T>` loses the space, and redundant union-grouping parens inside
    /// the angles are stripped (`set<(a | b)>` → `set<a | b>`).
    #[test]
    fn test_set_type_canonicalizes() {
        let formatted =
            format_source("model m:\n    xs set <string>\n    ys set<(string | number)>\n")
                .unwrap();
        assert!(formatted.contains("xs set<string>\n"), "{formatted}");
        assert!(
            formatted.contains("ys set<string | number>\n"),
            "{formatted}"
        );
        idempotent("model m:\n    xs set <string>\n    ys set<(string | number)>\n");
    }

    /// RFC 0032: MODIFIER-declaration directives survive formatting too (the
    /// same deletion class the field fix closed) and stay idempotent.
    #[test]
    fn test_modifier_directives_survive_formatting() {
        let source = "model sandboxCeiling:\n    |block []string?   #live\n";
        let formatted = format_source(source).unwrap();
        assert!(
            formatted.contains("|block []string? #live\n"),
            "{formatted}"
        );
        idempotent(source);
    }

    #[test]
    fn test_format_scalar_item_with_body_roundtrips() {
        // `- "/admin":` + body survives formatting (scalar-key-with-body).
        let source = "[]resource resources:\n    - \"/admin\":\n        method = \"POST\"\n";
        let formatted = format(&parse(source).unwrap());
        assert!(formatted.contains("- \"/admin\":"), "{formatted}");
        assert!(formatted.contains("method = \"POST\""), "{formatted}");
        roundtrip(source);
        idempotent(source);
    }

    #[test]
    fn test_format_shorthand_and_optional_suffixes_roundtrip() {
        // `+` (positional shorthand), `?` (optional), and canonical `?+` survive formatting.
        let source = "model resource:\n    name string?\n    path path+\n    slug string?+\n";
        let formatted = format(&parse(source).unwrap());
        assert!(formatted.contains("path path+"), "{formatted}");
        assert!(formatted.contains("slug string?+"), "{formatted}");
        assert!(formatted.contains("name string?"), "{formatted}");
        roundtrip(source);
        idempotent(source);
    }

    #[test]
    fn test_format_oneof_aligns_arrows_and_roundtrips() {
        let source = "oneof email by provider:\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n";
        let formatted = format(&parse(source).unwrap());
        // Arrows are aligned on the widest quoted value ("postmark" = 10 cols).
        assert!(
            formatted.contains("    \"log\"      -> emailLog\n"),
            "arrows should be aligned:\n{formatted}"
        );
        assert!(formatted.contains("    \"postmark\" -> emailPostmark\n"));
        roundtrip(source);
        idempotent(source);
    }

    #[test]
    fn test_format_arm_set_field_types_roundtrip() {
        // RFC 0007 §5: the arm-set TYPE renders canonically as `(K -> V)`,
        // composes with unions on either side, keeps the field-suffix `?`
        // outside the parens, round-trips, and is idempotent.
        let source =
            "model mount:\n    denial (string | (role -> denial))?\n    route (role -> (a | b))\n";
        let formatted = format(&parse(source).unwrap());
        assert!(
            formatted.contains("denial (string | (role -> denial))?"),
            "union-of-scalar-and-arm-set renders canonically:\n{formatted}"
        );
        assert!(
            formatted.contains("route (role -> (a | b))"),
            "arm set to a union target renders canonically:\n{formatted}"
        );
        roundtrip(source);
        idempotent(source);
    }

    #[test]
    fn test_format_denial_arms_roundtrip() {
        // Arms inside a plain block (RFC 0007): a homogeneous run aligns its
        // arrows on the widest selector (matching `oneof` arm alignment),
        // survives a round-trip, and is idempotent.
        let source =
            "service App:\n    denial:\n        @plan/Pro -> ProUpsell\n        else -> Generic\n";
        let formatted = format(&parse(source).unwrap());
        assert!(
            formatted.contains("        @plan/Pro -> ProUpsell\n"),
            "widest arm sets the column:\n{formatted}"
        );
        assert!(
            formatted.contains("        else      -> Generic\n"),
            "'else' pads to the widest selector (@plan/Pro = 9 cols):\n{formatted}"
        );
        roundtrip(source);
        idempotent(source);
    }

    #[test]
    fn test_format_arm_literal_targets_roundtrip() {
        // RFC 0007 §6: a string-literal arm target (path/url for flat routers)
        // renders quoted, aligns like any arm, round-trips, and is idempotent.
        let source = "service App:\n    dispatch:\n        @role/admin -> \"admin.workflow.nml\"\n        else -> \"default.workflow.nml\"\n";
        let formatted = format(&parse(source).unwrap());
        assert!(
            formatted.contains("        @role/admin -> \"admin.workflow.nml\"\n"),
            "literal target renders quoted:\n{formatted}"
        );
        assert!(
            formatted.contains("        else        -> \"default.workflow.nml\"\n"),
            "'else' pads to the widest selector (@role/admin = 11 cols):\n{formatted}"
        );
        roundtrip(source);
        idempotent(source);
    }

    #[test]
    fn test_format_oneof_default_discriminator_roundtrips() {
        let source = "oneof email by provider = \"log\":\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n";
        let formatted = format(&parse(source).unwrap());
        assert!(
            formatted.contains("oneof email by provider = \"log\":"),
            "default discriminator must survive formatting:\n{formatted}"
        );
        roundtrip(source);
        idempotent(source);
    }

    #[test]
    fn test_format_oneof_enum_typed_discriminator_roundtrips() {
        let source = "oneof email by provider as providerKind = \"log\":\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n";
        let formatted = format(&parse(source).unwrap());
        assert!(
            formatted.contains("oneof email by provider as providerKind = \"log\":"),
            "enum type + default must survive formatting:\n{formatted}"
        );
        roundtrip(source);
        idempotent(source);
    }

    /// Comment-preserving formatting must reparse cleanly, keep every
    /// comment, and be idempotent.
    fn roundtrip_comments(source: &str) -> String {
        let formatted = format_source(source)
            .unwrap_or_else(|e| panic!("failed to format:\n{}\nerror: {}", source, e.message()));
        let (_, original_comments) = nml_core::cst::parse_with_comments(source).unwrap();
        let (_, kept_comments) =
            nml_core::cst::parse_with_comments(&formatted).unwrap_or_else(|e| {
                panic!(
                    "failed to reparse formatted output:\n{}\nerror: {}",
                    formatted,
                    e.message()
                )
            });
        let original: Vec<&str> = original_comments.iter().map(|c| c.text.trim()).collect();
        let kept: Vec<&str> = kept_comments.iter().map(|c| c.text.trim()).collect();
        assert_eq!(original, kept, "comments lost or reordered:\n{formatted}");

        let again = format_source(&formatted).unwrap();
        assert_eq!(formatted, again, "comment formatting is not idempotent");
        formatted
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
    fn roundtrip_oneof_with_comments() {
        // Own-line comment before an arm, trailing comment on the header, and
        // trailing comment on an arm must all survive and be idempotent.
        roundtrip_comments(
            "// email transport\noneof email by provider: // tagged\n    // dev default\n    \"log\" -> emailLog // no delivery\n    \"postmark\" -> emailPostmark\n",
        );
    }

    #[test]
    fn roundtrip_multiple_blocks() {
        roundtrip("service A:\n    x = 1\n\nservice B:\n    y = 2\n");
    }

    #[test]
    fn roundtrip_preserves_nominal_annotation() {
        use nml_core::ast::{Body, BodyEntryKind, DeclarationKind};
        // RFC 0015 data-integrity guarantee: fmt reconstructs from the AST, so
        // it MUST re-emit `as <Variant>`; dropping it would silently change a
        // same-class union's variant. Assert the text is preserved AND that the
        // annotation survives a parse→format→parse round-trip semantically.
        let field = "host H:\n    slot as modelB:\n        b = \"x\"\n";
        let elem = "host H:\n    slots:\n        - one as modelB:\n            b = \"x\"\n";

        fn slot_annotation(body: &Body, name: &str) -> Option<String> {
            body.entries.iter().find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == name => {
                    Some(nb.body.type_annotation.as_ref()?.name.clone())
                }
                _ => None,
            })
        }
        fn host_body(src: &str) -> Body {
            let file = parse(src).unwrap();
            let formatted = format(&file);
            // Text preserved verbatim (idempotent canonical form).
            assert_eq!(formatted, src, "annotation must survive formatting");
            match &parse(&formatted).unwrap().declarations[0].kind {
                DeclarationKind::Block(b) => b.body.clone(),
                _ => panic!("expected block"),
            }
        }

        assert_eq!(
            slot_annotation(&host_body(field), "slot").as_deref(),
            Some("modelB"),
            "field-level annotation must survive parse→format→parse"
        );
        // List-element level: descend into `slots` then find the named item.
        let elem_body = host_body(elem);
        let slots = elem_body
            .entries
            .iter()
            .find_map(|e| match &e.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == "slots" => Some(&nb.body),
                _ => None,
            })
            .unwrap();
        let item_ann = slots.entries.iter().find_map(|e| match &e.kind {
            BodyEntryKind::ListItem(it) => match &it.kind {
                nml_core::ast::ListItemKind::Named { body, .. } => {
                    Some(body.type_annotation.as_ref()?.name.clone())
                }
                _ => None,
            },
            _ => None,
        });
        assert_eq!(
            item_ann.as_deref(),
            Some("modelB"),
            "list-element annotation must survive parse→format→parse"
        );
    }

    #[test]
    fn roundtrip_list_items() {
        roundtrip(
            "workflow W:\n    steps:\n        - step1:\n            x = 1\n        - step2:\n            y = 2\n",
        );
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
    fn roundtrip_block_extends_with_body() {
        roundtrip("model plan is role:\n    name string\n");
    }

    #[test]
    fn roundtrip_block_extends_no_body() {
        roundtrip("model plan is role\n");
    }

    #[test]
    fn roundtrip_block_extends_multi_parent() {
        roundtrip("model admin is role, auditable:\n    level number\n");
    }

    #[test]
    fn roundtrip_enum() {
        roundtrip("enum Status:\n    - \"active\"\n    - \"inactive\"\n");
    }

    #[test]
    fn roundtrip_shared_property() {
        roundtrip(
            "workflow W:\n    .defaults:\n        retries = 3\n    - step1:\n        x = 1\n",
        );
    }

    #[test]
    fn roundtrip_scalar_shared_property() {
        roundtrip("workflow W:\n    .interval = 7200\n    - step1:\n        x = 1\n");
    }

    #[test]
    fn roundtrip_scalar_and_block_shared_property() {
        roundtrip(
            "workflow W:\n    .interval = 900\n    .defaults:\n        retries = 3\n    - step1:\n        x = 1\n",
        );
    }

    #[test]
    fn roundtrip_array_scalar_shared_property() {
        roundtrip("[]mount mounts:\n    .interval = 300\n    - Main:\n        path = \"/\"\n");
    }

    #[test]
    fn roundtrip_modifier() {
        roundtrip("service App:\n    port = 8080\n    |allow:\n        - @role/admin\n");
    }

    #[test]
    fn roundtrip_array_with_modifier() {
        roundtrip(
            "[]mount mounts:\n    |allow = [@authenticated]\n    - Main:\n        path = \"/\"\n",
        );
    }

    #[test]
    fn roundtrip_array_with_block_modifier() {
        roundtrip(
            "[]resource resources:\n    |allow:\n        - @role/admin\n        - @role/editor\n\n    - Dashboard:\n        path = \"/dashboard\"\n",
        );
    }

    #[test]
    fn roundtrip_array_with_multiple_modifiers() {
        roundtrip(
            "[]route routes:\n    |allow = [@authenticated]\n    |deny = [@anonymous]\n\n    - Home:\n        path = \"/\"\n",
        );
    }

    #[test]
    fn idempotent_array_with_modifier() {
        idempotent(
            "[]mount mounts:\n    |allow = [@authenticated]\n\n    - Main:\n        path = \"/\"\n",
        );
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
        idempotent(
            "workflow W:\n    steps:\n        - s1:\n            provider = \"fast\"\n        - s2:\n            provider = \"slow\"\n",
        );
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

    #[test]
    fn formats_cst_only_syntax() {
        // `format_source` parses via the CST, which accepts syntax the legacy
        // parser rejects (nested array types). The formatter must handle it and
        // be idempotent — a capability the legacy-`parse`-based `roundtrip`
        // helper can't cover (legacy can't parse this input).
        let src = "model M:\n    grid [][]string\n    pairs [](string | int)\n";
        let out = format_source(src).expect("CST-only syntax should format");
        assert!(out.contains("grid [][]string"), "{out}");
        assert_eq!(out, format_source(&out).unwrap(), "must be idempotent");
    }

    // -------------------------------------------------------------------
    // Comment preservation
    // -------------------------------------------------------------------

    #[test]
    fn comments_file_header() {
        let out = roundtrip_comments(
            "// Application config\n// Edit with care.\nservice App:\n    port = 8080\n",
        );
        assert!(out.starts_with("// Application config\n// Edit with care.\nservice App:"));
    }

    #[test]
    fn comments_between_declarations() {
        let out = roundtrip_comments(
            "service A:\n    x = 1\n\n// second service\nservice B:\n    y = 2\n",
        );
        assert!(out.contains("\n\n// second service\nservice B:"));
    }

    #[test]
    fn comments_inside_body() {
        let out = roundtrip_comments(
            "service App:\n    // network settings\n    port = 8080\n    host = \"localhost\"\n",
        );
        assert!(out.contains("\n    // network settings\n    port = 8080\n"));
    }

    #[test]
    fn comments_trailing_property() {
        let out = roundtrip_comments("service App:\n    port = 8080 // default port\n");
        assert!(out.contains("port = 8080 // default port\n"));
    }

    #[test]
    fn comments_trailing_block_header() {
        let out = roundtrip_comments("service App: // main entry\n    port = 8080\n");
        assert!(out.contains("service App: // main entry\n"));
    }

    #[test]
    fn comments_trailing_nested_block_header() {
        let out = roundtrip_comments(
            "server S:\n    db: // database settings\n        url = \"postgres://x\"\n",
        );
        assert!(out.contains("    db: // database settings\n"));
    }

    #[test]
    fn comments_in_nested_body() {
        let out = roundtrip_comments(
            "server S:\n    db:\n        // connection\n        url = \"postgres://x\"\n",
        );
        assert!(out.contains("\n        // connection\n        url = \"postgres://x\"\n"));
    }

    #[test]
    fn comments_after_last_entry_keep_nesting() {
        let out = roundtrip_comments(
            "server S:\n    db:\n        url = \"x\"\n        // todo: add pool size\n    port = 1\n",
        );
        assert!(
            out.contains("\n        // todo: add pool size\n    port = 1\n"),
            "comment should keep its original nesting:\n{out}"
        );
    }

    #[test]
    fn comments_at_end_of_file() {
        let out = roundtrip_comments("service App:\n    port = 8080\n    // end of config\n");
        assert!(out.ends_with("    // end of config\n"));
    }

    #[test]
    fn comments_only_file() {
        let out = roundtrip_comments("// nothing here yet\n");
        assert_eq!(out, "// nothing here yet\n");
    }

    #[test]
    fn comments_on_const_and_template() {
        let out = roundtrip_comments(
            "// the port\nconst Port = 8080 // tcp\n\ntemplate Greeting: // says hi\n    \"hello\"\n",
        );
        assert!(out.contains("// the port\nconst Port = 8080 // tcp\n"));
        assert!(out.contains("template Greeting: // says hi\n"));
    }

    #[test]
    fn comments_in_list_items() {
        let out = roundtrip_comments(
            "workflow W:\n    steps:\n        // first step\n        - s1: // classify\n            x = 1\n",
        );
        assert!(out.contains("\n        // first step\n        - s1: // classify\n"));
    }

    #[test]
    fn comments_in_array_declaration() {
        let out = roundtrip_comments(
            "[]mount mounts:\n    // defaults for every mount\n    .interval = 300\n\n    // the root mount\n    - Main:\n        path = \"/\"\n",
        );
        assert!(out.contains("// defaults for every mount"));
        assert!(out.contains("// the root mount"));
    }

    #[test]
    fn comments_divider_preserved_verbatim() {
        let out = roundtrip_comments("//// section ////\nservice App:\n    port = 1\n");
        assert!(out.starts_with("//// section ////\n"));
    }

    #[test]
    fn comment_like_string_untouched() {
        let out =
            roundtrip_comments("service App:\n    url = \"https://example.com\" // real comment\n");
        assert!(out.contains("url = \"https://example.com\" // real comment\n"));
    }

    #[test]
    fn comments_format_source_full_document() {
        let source = "\
// Demo configuration
service App: // main
    // network
    port = 8080 // tcp
    host = \"localhost\"
    db:
        // creds come from env
        url = $ENV.DB_URL | \"postgres://localhost\"
    // misc below
    debug = true

// roles
[]mount mounts:
    - Main: // root
        path = \"/\"
// eof note
";
        roundtrip_comments(source);
    }

    #[test]
    fn format_without_source_still_works() {
        // The AST-only entry point is comment-less by contract.
        let file = parse("service App:\n    port = 8080 // gone\n").unwrap();
        let formatted = format(&file);
        assert_eq!(formatted, "service App:\n    port = 8080\n");
    }

    /// RFC 0014: the formatter renders role conjunctions from the LOWERED
    /// value, so tight/ragged source spacing canonicalizes to `" & "`
    /// automatically — and the output is idempotent.
    #[test]
    fn role_conjunction_canonicalizes_and_is_idempotent() {
        let src = "service App:\n    gate = @role/a&@role/b\n    allow = [@role/x  &  @role/y, @plan/Pro]\n";
        let formatted = format_source(src).unwrap();
        assert!(
            formatted.contains("gate = @role/a & @role/b"),
            "{formatted}"
        );
        assert!(
            formatted.contains("[@role/x & @role/y, @plan/Pro]"),
            "{formatted}"
        );
        let again = format_source(&formatted).unwrap();
        assert_eq!(formatted, again, "conjunction formatting is not idempotent");
    }

    // -------------------------------------------------------------------
    // Fixture round-trip tests
    // -------------------------------------------------------------------

    #[test]
    fn roundtrip_fixture_minimal_service() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/minimal-service.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_fixture_full_service() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/full-service.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_fixture_web_server() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/web-server.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_fixture_role_templates() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/role-templates.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_fixture_secret_values() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/secret-values.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_fixture_money_values() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/money-values.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_fixture_pricing() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/valid/pricing.nml"
        ))
        .unwrap();
        roundtrip(&source);
        idempotent(&source);
        roundtrip_comments(&source);
    }

    #[test]
    fn roundtrip_inline_role_refs() {
        roundtrip("mount Api:\n    path = \"/api\"\n    |allow = [@public, @role/admin]\n");
    }

    #[test]
    fn roundtrip_block_role_ref_list() {
        roundtrip(
            "role admin:\n    members:\n        - @role/editor\n        - @user/test@example.com\n",
        );
    }

    #[test]
    fn roundtrip_value_role_property() {
        roundtrip("service App:\n    access = @role/admin\n");
    }
}
