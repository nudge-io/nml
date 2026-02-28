use std::collections::HashMap;

use crate::ast::*;
use crate::error::NmlError;
use crate::span::Span;
use crate::types::Value;

/// Tracks named declarations for cross-reference resolution.
#[derive(Debug, Default)]
pub struct Resolver {
    declarations: HashMap<String, Vec<DeclInfo>>,
    const_values: HashMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct DeclInfo {
    pub keyword: String,
    pub name: String,
    pub span: Span,
}

impl Resolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register all declarations from a parsed file.
    pub fn register_file(&mut self, file: &File) {
        for decl in &file.declarations {
            let info = match &decl.kind {
                DeclarationKind::Block(block) => DeclInfo {
                    keyword: block.keyword.name.clone(),
                    name: block.name.name.clone(),
                    span: decl.span,
                },
                DeclarationKind::Array(arr) => DeclInfo {
                    keyword: format!("[]{}", arr.item_keyword.name),
                    name: arr.name.name.clone(),
                    span: decl.span,
                },
                DeclarationKind::Const(c) => {
                    self.const_values
                        .insert(c.name.name.clone(), c.value.value.clone());
                    DeclInfo {
                        keyword: "const".into(),
                        name: c.name.name.clone(),
                        span: decl.span,
                    }
                }
                DeclarationKind::Template(t) => {
                    self.const_values
                        .insert(t.name.name.clone(), t.value.value.clone());
                    DeclInfo {
                        keyword: "template".into(),
                        name: t.name.name.clone(),
                        span: decl.span,
                    }
                }
            };
            self.declarations
                .entry(info.name.clone())
                .or_default()
                .push(info);
        }
    }

    /// Resolve a const reference to its value. Returns None if the name is not a const.
    pub fn resolve_const_value(&self, name: &str) -> Option<&Value> {
        self.const_values.get(name)
    }

    /// Resolve a reference name to its declaration info.
    pub fn resolve(&self, name: &str) -> Option<&[DeclInfo]> {
        self.declarations.get(name).map(|v| v.as_slice())
    }

    /// Check for duplicate declarations and return errors.
    pub fn find_duplicates(&self) -> Vec<NmlError> {
        let mut errors = Vec::new();
        for (name, decls) in &self.declarations {
            if decls.len() > 1 {
                for dup in &decls[1..] {
                    errors.push(NmlError::Validation {
                        message: format!("duplicate declaration: '{name}'"),
                        span: dup.span,
                    });
                }
            }
        }
        errors
    }

    /// Return all registered declaration names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.declarations.keys().map(|s| s.as_str())
    }

    /// Find all unresolved references in the file.
    /// Walks property values looking for `Value::Reference` that don't match
    /// any registered declaration name.
    pub fn find_unresolved_references(&self, file: &File) -> Vec<NmlError> {
        let mut errors = Vec::new();
        for decl in &file.declarations {
            match &decl.kind {
                DeclarationKind::Block(block) => {
                    let is_schema_def =
                        matches!(block.keyword.name.as_str(), "model" | "trait" | "enum");
                    if !is_schema_def {
                        self.check_body_refs(&block.body, &mut errors);
                    }
                }
                DeclarationKind::Array(arr) => {
                    let is_schema_def =
                        matches!(arr.item_keyword.name.as_str(), "model" | "trait" | "enum");
                    if !is_schema_def {
                        for item in &arr.body.items {
                            if let ListItemKind::Named { body, .. } = &item.kind {
                                self.check_body_refs(body, &mut errors);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        errors
    }

    fn check_body_refs(&self, body: &Body, errors: &mut Vec<NmlError>) {
        for entry in &body.entries {
            match &entry.kind {
                BodyEntryKind::Property(prop) => {
                    if let Value::Reference(name) = &prop.value.value {
                        if self.resolve(name).is_none() {
                            errors.push(NmlError::Validation {
                                message: format!("unresolved reference '{name}'"),
                                span: prop.value.span,
                            });
                        }
                    }
                }
                BodyEntryKind::NestedBlock(nb) => {
                    self.check_body_refs(&nb.body, errors);
                }
                BodyEntryKind::ListItem(item) => {
                    if let ListItemKind::Named { body, .. } = &item.kind {
                        self.check_body_refs(body, errors);
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn test_resolve_reference() {
        let source = "service Svc:\n    localMount = \"/\"\n\nresource Res:\n    path = \"/\"\n";
        let file = parser::parse(source).unwrap();
        let mut resolver = Resolver::new();
        resolver.register_file(&file);

        assert!(resolver.resolve("Svc").is_some());
        assert!(resolver.resolve("Res").is_some());
        assert!(resolver.resolve("Unknown").is_none());
    }

    #[test]
    fn test_unresolved_reference() {
        let source = "provider Groq:\n    type = \"groq\"\n\nworkflow W:\n    entrypoint = lasda\n";
        let file = parser::parse(source).unwrap();
        let mut resolver = Resolver::new();
        resolver.register_file(&file);

        let errors = resolver.find_unresolved_references(&file);
        assert!(
            errors.iter().any(|e| e.message().contains("unresolved reference 'lasda'")),
            "should flag unresolved reference; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_valid_reference_no_error() {
        let source = "provider Groq:\n    type = \"groq\"\n\nworkflow W:\n    provider = Groq\n";
        let file = parser::parse(source).unwrap();
        let mut resolver = Resolver::new();
        resolver.register_file(&file);

        let errors = resolver.find_unresolved_references(&file);
        assert!(
            errors.is_empty(),
            "valid reference should not be flagged; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_unresolved_ref_in_list_item() {
        let source = "workflow W:\n    entrypoint = \"start\"\n    steps:\n        - s1:\n            provider = NonExistent\n";
        let file = parser::parse(source).unwrap();
        let mut resolver = Resolver::new();
        resolver.register_file(&file);

        let errors = resolver.find_unresolved_references(&file);
        assert!(
            errors.iter().any(|e| e.message().contains("unresolved reference 'NonExistent'")),
            "should flag unresolved reference inside list item; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_model_definitions_skipped() {
        let source = "model step:\n    provider string?\n    prompt prompt?\n";
        let file = parser::parse(source).unwrap();
        let mut resolver = Resolver::new();
        resolver.register_file(&file);

        let errors = resolver.find_unresolved_references(&file);
        assert!(
            errors.is_empty(),
            "model/trait/enum definitions should not be checked for value refs; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_find_duplicates() {
        let source =
            "service Svc:\n    localMount = \"/\"\n\nservice Svc:\n    localMount = \"/other\"\n";
        let file = parser::parse(source).unwrap();
        let mut resolver = Resolver::new();
        resolver.register_file(&file);

        let errors = resolver.find_duplicates();
        assert_eq!(errors.len(), 1);
    }
}
