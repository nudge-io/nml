use std::collections::HashMap;

use crate::ast::*;
use crate::error::NmlError;
use crate::span::Span;

/// Tracks named declarations for cross-reference resolution.
#[derive(Debug, Default)]
pub struct Resolver {
    declarations: HashMap<String, Vec<DeclInfo>>,
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
            };
            self.declarations
                .entry(info.name.clone())
                .or_default()
                .push(info);
        }
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
