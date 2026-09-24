//! Convenience API for querying parsed NML documents.
//!
//! Provides a fluent interface for extracting typed values from the AST
//! without manual pattern matching.
//!
//! # Example
//!
//! ```rust
//! use nml_core::query::Document;
//!
//! let source = r#"
//! service MyApp:
//!     port = 8080
//!     name = "my-app"
//!     debug = true
//! "#;
//! let file = nml_core::cst::parse_to_ast(source).unwrap();
//! let doc = Document::new(&file);
//!
//! let port = doc.block("service", "MyApp")
//!     .property("port")
//!     .to_i64();
//! assert_eq!(port, Some(8080));
//!
//! let name = doc.block("service", "MyApp")
//!     .property("name")
//!     .as_str();
//! assert_eq!(name, Some("my-app"));
//! ```

use crate::ast::*;
use crate::span::Span;
use crate::types::{SpannedValue, Value};

/// A queryable wrapper around a parsed NML [`File`].
pub struct Document<'a> {
    file: &'a File,
}

impl<'a> Document<'a> {
    /// Create a new `Document` from a parsed `File`.
    pub fn new(file: &'a File) -> Self {
        Self { file }
    }

    /// Find a block declaration by keyword and name.
    ///
    /// Returns a [`BlockQuery`] for further drilling into properties and nested blocks.
    pub fn block(&self, keyword: &str, name: &str) -> BlockQuery<'a> {
        match self.block_decl(keyword, name) {
            Some(block) => BlockQuery::Found(&block.body),
            None => BlockQuery::NotFound,
        }
    }

    /// The full declaration for `keyword name`, when present — for callers
    /// that need header facts (the `uses` clause, spans) rather than just
    /// the body. `block()` is this with the header dropped.
    pub fn block_decl(&self, keyword: &str, name: &str) -> Option<&'a BlockDecl> {
        self.file
            .declarations
            .iter()
            .find_map(|decl| match &decl.kind {
                DeclarationKind::Block(block)
                    if block.keyword.name == keyword && block.name.name == name =>
                {
                    Some(block)
                }
                _ => None,
            })
    }

    /// Iterate over all block declarations matching a keyword.
    pub fn blocks(&self, keyword: &str) -> Vec<(&'a str, BlockQuery<'a>)> {
        let mut result = Vec::new();
        for decl in &self.file.declarations {
            if let DeclarationKind::Block(block) = &decl.kind {
                if block.keyword.name == keyword {
                    result.push((block.name.name.as_str(), BlockQuery::Found(&block.body)));
                }
            }
        }
        result
    }

    /// Get the value of a top-level const declaration.
    pub fn const_value(&self, name: &str) -> ValueQuery<'a> {
        for decl in &self.file.declarations {
            if let DeclarationKind::Const(c) = &decl.kind {
                if c.name.name == name {
                    return ValueQuery::Found(&c.value.value);
                }
            }
        }
        ValueQuery::NotFound
    }

    /// Get a top-level template string value.
    pub fn template_value(&self, name: &str) -> ValueQuery<'a> {
        for decl in &self.file.declarations {
            if let DeclarationKind::Template(t) = &decl.kind {
                if t.name.name == name {
                    return ValueQuery::Found(&t.value.value);
                }
            }
        }
        ValueQuery::NotFound
    }

    /// Look up a top-level array declaration (`[]keyword Name:`) by its
    /// name, returning its body — items, shared properties, properties,
    /// modifiers. Reference assignments (`endpoints = Name`) point at these;
    /// `defaults::from_document_defaulted` materializes them (RFC 0013).
    pub fn array_body(&self, name: &str) -> Option<&'a ArrayBody> {
        self.file
            .declarations
            .iter()
            .find_map(|decl| match &decl.kind {
                DeclarationKind::Array(a) if a.name.name == name => Some(&a.body),
                _ => None,
            })
    }

    /// Get all declaration names and keywords in the file.
    pub fn declarations(&self) -> Vec<(&'a str, &'a str)> {
        let mut result = Vec::new();
        for decl in &self.file.declarations {
            match &decl.kind {
                DeclarationKind::Block(b) => {
                    result.push((b.keyword.name.as_str(), b.name.name.as_str()));
                }
                DeclarationKind::Array(a) => {
                    result.push((a.item_keyword.name.as_str(), a.name.name.as_str()));
                }
                DeclarationKind::Const(c) => {
                    result.push(("const", c.name.name.as_str()));
                }
                DeclarationKind::Template(t) => {
                    result.push(("template", t.name.name.as_str()));
                }
                DeclarationKind::OneOf(o) => {
                    result.push(("oneof", o.name.name.as_str()));
                }
            }
        }
        result
    }
}

/// Result of looking up a block in the AST.
pub enum BlockQuery<'a> {
    Found(&'a Body),
    NotFound,
}

impl<'a> BlockQuery<'a> {
    /// Look up a property by name within this block.
    pub fn property(&self, name: &str) -> ValueQuery<'a> {
        match self {
            BlockQuery::Found(body) => find_property(body, name),
            BlockQuery::NotFound => ValueQuery::NotFound,
        }
    }

    /// Access a nested block by name.
    pub fn nested(&self, name: &str) -> BlockQuery<'a> {
        match self {
            BlockQuery::Found(body) => {
                for entry in &body.entries {
                    if let BodyEntryKind::NestedBlock(nested) = &entry.kind {
                        if nested.name.name == name {
                            return BlockQuery::Found(&nested.body);
                        }
                    }
                }
                BlockQuery::NotFound
            }
            BlockQuery::NotFound => BlockQuery::NotFound,
        }
    }

    /// Check if this block was found.
    pub fn is_found(&self) -> bool {
        matches!(self, BlockQuery::Found(_))
    }

    /// Get the body if found.
    pub fn body(&self) -> Option<&'a Body> {
        match self {
            BlockQuery::Found(b) => Some(b),
            BlockQuery::NotFound => None,
        }
    }

    /// The strings of the list-valued entry `name`, in EITHER spelling
    /// the language admits for a `[]string` field — the block form
    /// (`files:` + `- "x"` items) and the inline array (`files = ["x"]`)
    /// — each with its own span: a quoted literal's, or a bare name's
    /// (`- server`, lowered as a reference). The ONE read for a consumer
    /// that judged the field through a schema: a loader reading one
    /// spelling silently disagreed with the meta-schema that accepted
    /// both. Elements of any other shape are left out (the schema's
    /// finding, not the reader's). `None` when the block has no entry
    /// `name`; a body naming it twice does not parse (NML2093), so no
    /// reader ever runs over one. A template-string element is reported
    /// in [`StringList::templates`], never read as text.
    pub fn string_list(&self, name: &str) -> Option<StringList<'a>> {
        self.body()?
            .entries
            .iter()
            .find_map(|entry| match &entry.kind {
                BodyEntryKind::NestedBlock(nb) if nb.name.name == name => {
                    let mut list = StringList::new(nb.name.span);
                    for e in &nb.body.entries {
                        let BodyEntryKind::ListItem(item) = &e.kind else {
                            continue;
                        };
                        match &item.kind {
                            ListItemKind::Shorthand { value, .. } => list.push(value),
                            ListItemKind::Reference(id) => list.items.push(StringItem {
                                text: &id.name,
                                span: id.span,
                            }),
                            _ => {}
                        }
                    }
                    Some(list)
                }
                BodyEntryKind::Property(p) if p.name.name == name => {
                    let mut list = StringList::new(p.name.span);
                    if let Value::Array(elements) = &p.value.value {
                        for element in elements {
                            list.push(element);
                        }
                    }
                    Some(list)
                }
                _ => None,
            })
    }
}

/// One element of a list-valued entry read by [`BlockQuery::string_list`]:
/// a quoted literal or a bare name, with its own span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StringItem<'a> {
    pub text: &'a str,
    pub span: Span,
}

/// A list-valued entry read by [`BlockQuery::string_list`]: the key's span
/// (for a finding about the list as a whole) and its strings in source
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringList<'a> {
    pub key: Span,
    pub items: Vec<StringItem<'a>>,
    /// The spans of elements that are TEMPLATE strings (`"a{{x}}"`) — a
    /// value the meta-schema admits as a `string` that names no plain text
    /// (its expression segments are the embedder's). Never in `items`; a
    /// reader that needs literals refuses them located, so an element it
    /// cannot read is never an element it silently dropped (a manifest's
    /// `denyRefs` veto that never fired, a `files` glob that claimed
    /// less than it said). Elements of any other shape stay the schema's
    /// finding.
    pub templates: Vec<Span>,
}

impl<'a> StringList<'a> {
    fn new(key: Span) -> Self {
        Self {
            key,
            items: Vec::new(),
            templates: Vec::new(),
        }
    }

    /// Classify one element: a quoted literal or a bare name is a
    /// string of the list; a template string is reported, not read.
    fn push(&mut self, value: &'a SpannedValue) {
        match &value.value {
            Value::String(s) | Value::Reference(s) => self.items.push(StringItem {
                text: s,
                span: value.span,
            }),
            Value::TemplateString(_) => self.templates.push(value.span),
            _ => {}
        }
    }
}

/// Result of looking up a value in the AST.
pub enum ValueQuery<'a> {
    Found(&'a Value),
    NotFound,
}

impl<'a> ValueQuery<'a> {
    /// Extract as a string slice. Returns `None` for template strings with expressions.
    pub fn as_str(&self) -> Option<&'a str> {
        match self {
            ValueQuery::Found(v) => v.as_str().or_else(|| {
                if let Value::TemplateString(segs) = v {
                    if segs.len() == 1 {
                        if let crate::types::TemplateSegment::Literal(s) = &segs[0] {
                            return Some(s.as_str());
                        }
                    }
                }
                None
            }),
            _ => None,
        }
    }

    /// Extract as an owned string, resolving template strings to their raw form.
    pub fn as_string(&self) -> Option<String> {
        match self {
            ValueQuery::Found(v) => String::try_from(*v).ok(),
            _ => None,
        }
    }

    /// Extract as an `f64` via the correctly-rounded conversion (lossy
    /// above 2^53 and beyond binary64 precision; the stored decimal is
    /// exact — use [`ValueQuery::to_i64`] or the [`Value::Number`]
    /// variant when exactness matters).
    pub fn to_f64(&self) -> Option<f64> {
        match self {
            ValueQuery::Found(v) => v.to_f64(),
            _ => None,
        }
    }

    /// Extract as an exact integer. Returns `None` for fractional or
    /// out-of-range numbers rather than silently truncating.
    pub fn to_i64(&self) -> Option<i64> {
        match self {
            ValueQuery::Found(v) => v.to_i64(),
            _ => None,
        }
    }

    /// Extract as a boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ValueQuery::Found(v) => v.as_bool(),
            _ => None,
        }
    }

    /// Extract as an exact [`crate::types::Duration`] (RFC 0017; `Copy` —
    /// convert with [`crate::types::Duration::as_std`] for a
    /// `std::time::Duration`). Serde-free parity with the other typed
    /// reads: a duration-typed literal never needs re-parsing at the
    /// call site.
    pub fn as_duration(&self) -> Option<crate::types::Duration> {
        match self {
            ValueQuery::Found(v) => v.as_duration(),
            _ => None,
        }
    }

    /// Extract as a string array.
    pub fn as_string_array(&self) -> Option<Vec<&'a str>> {
        match self {
            ValueQuery::Found(v) => v.as_array().map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.value.as_str())
                    .collect()
            }),
            _ => None,
        }
    }

    /// Get the raw `Value` reference.
    pub fn value(&self) -> Option<&'a Value> {
        match self {
            ValueQuery::Found(v) => Some(v),
            ValueQuery::NotFound => None,
        }
    }

    /// Check if a value was found.
    pub fn is_found(&self) -> bool {
        matches!(self, ValueQuery::Found(_))
    }
}

fn find_property<'a>(body: &'a Body, name: &str) -> ValueQuery<'a> {
    for entry in &body.entries {
        if let BodyEntryKind::Property(prop) = &entry.kind {
            if prop.name.name == name {
                return ValueQuery::Found(&prop.value.value);
            }
        }
    }
    ValueQuery::NotFound
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `string_list` reads both spellings alike — the block form and the
    /// inline array — with each string's own span and the key's; a bare
    /// name is a string, anything else is not; a missing entry is `None`.
    #[test]
    fn string_list_reads_both_spellings_with_spans() {
        let src = "package p:\n    files:\n        - \"a/**\"\n        - server\n        - 1\n        - \"t/{{x}}\"\n    \
                   inline = [\"a/**\", server, 1, \"t/{{x}}\"]\n    scalar = \"x\"\n";
        let file = crate::parse(src).unwrap();
        let doc = Document::new(&file);
        let p = doc.block("package", "p");
        let block = p.string_list("files").expect("block form");
        let inline = p.string_list("inline").expect("inline form");
        let texts = |l: &StringList<'_>| {
            l.items
                .iter()
                .map(|i| i.text.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(&block), ["a/**", "server"]);
        assert_eq!(texts(&inline), ["a/**", "server"]);
        assert_eq!(&src[block.key.start..block.key.end], "files");
        assert_eq!(&src[inline.key.start..inline.key.end], "inline");
        for l in [&block, &inline] {
            assert_eq!(&src[l.items[0].span.start..l.items[0].span.end], "\"a/**\"");
            assert_eq!(&src[l.items[1].span.start..l.items[1].span.end], "server");
            // The template element is reported by span, never read as
            // text — and the number is neither (the schema's finding).
            assert_eq!(l.templates.len(), 1, "{l:?}");
            assert_eq!(
                &src[l.templates[0].start..l.templates[0].end],
                "\"t/{{x}}\""
            );
        }
        assert!(p.string_list("scalar").is_some_and(|l| l.items.is_empty()));
        assert!(p.string_list("absent").is_none());
        assert!(BlockQuery::NotFound.string_list("files").is_none());
    }
    use crate::cst::parse_to_ast;

    fn parse_doc(source: &str) -> File {
        parse_to_ast(source).unwrap()
    }

    #[test]
    fn query_block_property() {
        let file = parse_doc("service MyApp:\n    port = 8080\n    name = \"my-app\"\n");
        let doc = Document::new(&file);

        assert_eq!(
            doc.block("service", "MyApp").property("port").to_f64(),
            Some(8080.0)
        );
        assert_eq!(
            doc.block("service", "MyApp").property("name").as_str(),
            Some("my-app")
        );
        assert!(
            doc.block("service", "MyApp")
                .property("missing")
                .as_str()
                .is_none()
        );
        assert!(
            doc.block("service", "Other")
                .property("port")
                .to_f64()
                .is_none()
        );
    }

    #[test]
    fn query_nested_block() {
        let file = parse_doc("workflow W:\n    prompt:\n        system = \"hello\"\n");
        let doc = Document::new(&file);

        assert_eq!(
            doc.block("workflow", "W")
                .nested("prompt")
                .property("system")
                .as_str(),
            Some("hello")
        );
        assert!(
            doc.block("workflow", "W")
                .nested("missing")
                .property("system")
                .as_str()
                .is_none()
        );
    }

    #[test]
    fn query_const_value() {
        let file = parse_doc("const MaxRetries = 3\n");
        let doc = Document::new(&file);
        assert_eq!(doc.const_value("MaxRetries").to_f64(), Some(3.0));
        assert!(doc.const_value("Missing").to_f64().is_none());
    }

    #[test]
    fn query_bool_value() {
        let file = parse_doc("service App:\n    debug = true\n    verbose = false\n");
        let doc = Document::new(&file);
        assert_eq!(
            doc.block("service", "App").property("debug").as_bool(),
            Some(true)
        );
        assert_eq!(
            doc.block("service", "App").property("verbose").as_bool(),
            Some(false)
        );
    }

    #[test]
    fn query_string_array() {
        let file = parse_doc("service App:\n    tags = [\"web\", \"api\"]\n");
        let doc = Document::new(&file);
        assert_eq!(
            doc.block("service", "App")
                .property("tags")
                .as_string_array(),
            Some(vec!["web", "api"])
        );
    }

    #[test]
    fn query_blocks_by_keyword() {
        let file = parse_doc("service A:\n    port = 1\n\nservice B:\n    port = 2\n");
        let doc = Document::new(&file);
        let services = doc.blocks("service");
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].0, "A");
        assert_eq!(services[1].0, "B");
    }

    #[test]
    fn query_declarations() {
        let file = parse_doc("service A:\n    x = 1\n\nconst B = 2\n");
        let doc = Document::new(&file);
        let decls = doc.declarations();
        assert_eq!(decls, vec![("service", "A"), ("const", "B")]);
    }

    // -------------------------------------------------------------------
    // Phase 7: Query API edge cases
    // -------------------------------------------------------------------

    #[test]
    fn query_nonexistent_block() {
        let file = parse_doc("service App:\n    port = 8080\n");
        let doc = Document::new(&file);
        let result = doc.block("service", "Missing").property("port").to_f64();
        assert!(result.is_none());
    }

    #[test]
    fn query_nonexistent_keyword() {
        let file = parse_doc("service App:\n    port = 8080\n");
        let doc = Document::new(&file);
        let result = doc.block("workflow", "App").property("port").to_f64();
        assert!(result.is_none());
    }

    #[test]
    fn query_nonexistent_property() {
        let file = parse_doc("service App:\n    port = 8080\n");
        let doc = Document::new(&file);
        let result = doc.block("service", "App").property("missing").as_str();
        assert!(result.is_none());
    }

    #[test]
    fn query_property_wrong_type() {
        let file = parse_doc("service App:\n    port = 8080\n");
        let doc = Document::new(&file);
        assert!(
            doc.block("service", "App")
                .property("port")
                .as_str()
                .is_none()
        );
        assert!(
            doc.block("service", "App")
                .property("port")
                .as_bool()
                .is_none()
        );
    }

    #[test]
    fn query_bool_as_number_returns_none() {
        let file = parse_doc("service App:\n    debug = true\n");
        let doc = Document::new(&file);
        assert!(
            doc.block("service", "App")
                .property("debug")
                .to_f64()
                .is_none()
        );
    }

    #[test]
    fn query_number_as_bool_returns_none() {
        let file = parse_doc("service App:\n    port = 8080\n");
        let doc = Document::new(&file);
        assert!(
            doc.block("service", "App")
                .property("port")
                .as_bool()
                .is_none()
        );
    }

    /// RFC 0017: the serde-free typed read for durations — the literal
    /// arrives as a typed value, `as_std()` away from `std::time`, and a
    /// string is never silently reinterpreted as one.
    #[test]
    fn query_duration_accessor() {
        let file = parse_doc("service App:\n    timeout = 90m\n    label = \"90m\"\n");
        let doc = Document::new(&file);
        let d = doc
            .block("service", "App")
            .property("timeout")
            .as_duration()
            .expect("typed duration");
        assert_eq!(d.to_string(), "90m");
        assert_eq!(d.as_std(), std::time::Duration::from_secs(90 * 60));
        assert!(
            doc.block("service", "App")
                .property("label")
                .as_duration()
                .is_none(),
            "a string is a string — coercion is de's job"
        );
    }

    #[test]
    fn query_string_array_on_non_array() {
        let file = parse_doc("service App:\n    port = 8080\n");
        let doc = Document::new(&file);
        assert!(
            doc.block("service", "App")
                .property("port")
                .as_string_array()
                .is_none()
        );
    }

    #[test]
    fn query_nested_block_deep() {
        let file = parse_doc("server S:\n    db:\n        pool:\n            size = 10\n");
        let doc = Document::new(&file);
        assert_eq!(
            doc.block("server", "S")
                .nested("db")
                .nested("pool")
                .property("size")
                .to_f64(),
            Some(10.0)
        );
    }

    #[test]
    fn query_nested_block_missing() {
        let file = parse_doc("server S:\n    port = 8080\n");
        let doc = Document::new(&file);
        assert!(
            doc.block("server", "S")
                .nested("db")
                .property("url")
                .as_str()
                .is_none()
        );
    }

    #[test]
    fn query_value_found_vs_not_found() {
        let file = parse_doc("service App:\n    port = 8080\n");
        let doc = Document::new(&file);
        assert!(doc.block("service", "App").property("port").is_found());
        assert!(!doc.block("service", "App").property("missing").is_found());
    }

    #[test]
    fn query_value_raw() {
        let file = parse_doc("service App:\n    port = 8080\n");
        let doc = Document::new(&file);
        let val = doc.block("service", "App").property("port").value();
        assert!(val.is_some());
        assert!(matches!(val.unwrap(), Value::Number(n) if *n == 8080));
    }

    #[test]
    fn query_empty_file() {
        let file = parse_doc("");
        let doc = Document::new(&file);
        assert!(doc.declarations().is_empty());
        assert!(doc.blocks("service").is_empty());
    }

    #[test]
    fn query_const_missing() {
        let file = parse_doc("service App:\n    x = 1\n");
        let doc = Document::new(&file);
        assert!(doc.const_value("Missing").value().is_none());
    }

    #[test]
    fn query_to_i64() {
        let file = parse_doc("service App:\n    port = 8080\n");
        let doc = Document::new(&file);
        assert_eq!(
            doc.block("service", "App").property("port").to_i64(),
            Some(8080)
        );
    }

    #[test]
    fn query_as_string_on_template() {
        let file = parse_doc("service App:\n    greeting = \"Hello {{args.name}}\"\n");
        let doc = Document::new(&file);
        let result = doc.block("service", "App").property("greeting").as_string();
        assert!(result.is_some());
        assert!(result.unwrap().contains("args.name"));
    }

    #[test]
    fn query_blocks_empty() {
        let file = parse_doc("const X = 1\n");
        let doc = Document::new(&file);
        assert!(doc.blocks("service").is_empty());
    }
}
