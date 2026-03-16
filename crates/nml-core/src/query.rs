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
//! let file = nml_core::parse(source).unwrap();
//! let doc = Document::new(&file);
//!
//! let port = doc.block("service", "MyApp")
//!     .property("port")
//!     .as_f64();
//! assert_eq!(port, Some(8080.0));
//!
//! let name = doc.block("service", "MyApp")
//!     .property("name")
//!     .as_str();
//! assert_eq!(name, Some("my-app"));
//! ```

use crate::ast::*;
use crate::types::Value;

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
        for decl in &self.file.declarations {
            if let DeclarationKind::Block(block) = &decl.kind {
                if block.keyword.name == keyword && block.name.name == name {
                    return BlockQuery::Found(&block.body);
                }
            }
        }
        BlockQuery::NotFound
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

    /// Extract as a number.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            ValueQuery::Found(v) => v.as_f64(),
            _ => None,
        }
    }

    /// Extract as an integer (truncates fractional part).
    pub fn as_i64(&self) -> Option<i64> {
        self.as_f64().map(|n| n as i64)
    }

    /// Extract as a boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ValueQuery::Found(v) => v.as_bool(),
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
    use crate::parser;

    fn parse_doc(source: &str) -> File {
        parser::parse(source).unwrap()
    }

    #[test]
    fn query_block_property() {
        let file = parse_doc("service MyApp:\n    port = 8080\n    name = \"my-app\"\n");
        let doc = Document::new(&file);

        assert_eq!(doc.block("service", "MyApp").property("port").as_f64(), Some(8080.0));
        assert_eq!(doc.block("service", "MyApp").property("name").as_str(), Some("my-app"));
        assert!(doc.block("service", "MyApp").property("missing").as_str().is_none());
        assert!(doc.block("service", "Other").property("port").as_f64().is_none());
    }

    #[test]
    fn query_nested_block() {
        let file = parse_doc("workflow W:\n    prompt:\n        system = \"hello\"\n");
        let doc = Document::new(&file);

        assert_eq!(
            doc.block("workflow", "W").nested("prompt").property("system").as_str(),
            Some("hello")
        );
        assert!(doc.block("workflow", "W").nested("missing").property("system").as_str().is_none());
    }

    #[test]
    fn query_const_value() {
        let file = parse_doc("const MaxRetries = 3\n");
        let doc = Document::new(&file);
        assert_eq!(doc.const_value("MaxRetries").as_f64(), Some(3.0));
        assert!(doc.const_value("Missing").as_f64().is_none());
    }

    #[test]
    fn query_bool_value() {
        let file = parse_doc("service App:\n    debug = true\n    verbose = false\n");
        let doc = Document::new(&file);
        assert_eq!(doc.block("service", "App").property("debug").as_bool(), Some(true));
        assert_eq!(doc.block("service", "App").property("verbose").as_bool(), Some(false));
    }

    #[test]
    fn query_string_array() {
        let file = parse_doc("service App:\n    tags = [\"web\", \"api\"]\n");
        let doc = Document::new(&file);
        assert_eq!(
            doc.block("service", "App").property("tags").as_string_array(),
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
        let result = doc.block("service", "Missing").property("port").as_f64();
        assert!(result.is_none());
    }

    #[test]
    fn query_nonexistent_keyword() {
        let file = parse_doc("service App:\n    port = 8080\n");
        let doc = Document::new(&file);
        let result = doc.block("workflow", "App").property("port").as_f64();
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
        assert!(doc.block("service", "App").property("port").as_str().is_none());
        assert!(doc.block("service", "App").property("port").as_bool().is_none());
    }

    #[test]
    fn query_bool_as_number_returns_none() {
        let file = parse_doc("service App:\n    debug = true\n");
        let doc = Document::new(&file);
        assert!(doc.block("service", "App").property("debug").as_f64().is_none());
    }

    #[test]
    fn query_number_as_bool_returns_none() {
        let file = parse_doc("service App:\n    port = 8080\n");
        let doc = Document::new(&file);
        assert!(doc.block("service", "App").property("port").as_bool().is_none());
    }

    #[test]
    fn query_string_array_on_non_array() {
        let file = parse_doc("service App:\n    port = 8080\n");
        let doc = Document::new(&file);
        assert!(doc.block("service", "App").property("port").as_string_array().is_none());
    }

    #[test]
    fn query_nested_block_deep() {
        let file = parse_doc("server S:\n    db:\n        pool:\n            size = 10\n");
        let doc = Document::new(&file);
        assert_eq!(
            doc.block("server", "S").nested("db").nested("pool").property("size").as_f64(),
            Some(10.0)
        );
    }

    #[test]
    fn query_nested_block_missing() {
        let file = parse_doc("server S:\n    port = 8080\n");
        let doc = Document::new(&file);
        assert!(doc.block("server", "S").nested("db").property("url").as_str().is_none());
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
        assert!(matches!(val.unwrap(), Value::Number(n) if *n == 8080.0));
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
    fn query_as_i64() {
        let file = parse_doc("service App:\n    port = 8080\n");
        let doc = Document::new(&file);
        assert_eq!(doc.block("service", "App").property("port").as_i64(), Some(8080));
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
