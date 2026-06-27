//! Symbol table for NML declarations.
//!
//! [`SymbolTable`] indexes the named declarations of a parsed file (blocks,
//! arrays, consts, templates) and answers static questions about them:
//! reference lookup, duplicate detection, const-chain resolution, and
//! const-cycle / unresolved-reference diagnostics.
//!
//! This is *static* (parse-time) resolution. Runtime value resolution --
//! `$ENV.KEY` secrets and `a | b` fallback chains -- lives in
//! [`crate::resolve::ValueResolver`].

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::error::NmlError;
use crate::span::Span;
use crate::types::Value;

/// Tracks named declarations for cross-reference resolution.
#[derive(Debug, Default)]
pub struct SymbolTable {
    declarations: HashMap<String, Vec<DeclInfo>>,
    const_values: HashMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct DeclInfo {
    pub keyword: String,
    pub name: String,
    pub span: Span,
}

impl SymbolTable {
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
                DeclarationKind::OneOf(oneof) => DeclInfo {
                    keyword: "oneof".into(),
                    name: oneof.name.name.clone(),
                    span: decl.span,
                },
            };
            self.declarations
                .entry(info.name.clone())
                .or_default()
                .push(info);
        }
    }

    /// Resolve a const reference to its value, following chains of
    /// const-to-const references (`const A = B`, `const B = "x"` resolves
    /// `A` to `"x"`). Returns `None` if the name is not a const, or if the
    /// chain is cyclic (cycles are reported separately by
    /// [`SymbolTable::find_const_cycles`]).
    pub fn resolve_const_value(&self, name: &str) -> Option<&Value> {
        let mut seen = HashSet::new();
        let mut current = name;
        loop {
            if !seen.insert(current) {
                return None; // cyclic chain
            }
            let value = self.const_values.get(current)?;
            match value {
                Value::Reference(next) if self.const_values.contains_key(next.as_str()) => {
                    current = next;
                }
                _ => return Some(value),
            }
        }
    }

    /// Snapshot every `const` to its fully chain-resolved value, dropping cyclic
    /// references (which resolve to nothing — same as [`Self::resolve_const_value`]).
    ///
    /// Produces an owned map suitable for a `'static` const lookup, e.g.
    /// `ValueResolver::env().with_symbols(move |name| snapshot.get(name).cloned())`,
    /// so deserialization resolves `Value::Reference` consts the same way the
    /// hand-written `resolve_string` does.
    pub fn resolved_const_snapshot(&self) -> HashMap<String, Value> {
        self.const_values
            .keys()
            .filter_map(|name| {
                self.resolve_const_value(name)
                    .map(|value| (name.clone(), value.clone()))
            })
            .collect()
    }

    /// Look up a declaration name, returning every declaration that uses it
    /// (more than one indicates a duplicate).
    pub fn lookup(&self, name: &str) -> Option<&[DeclInfo]> {
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
    /// Uses scope-aware resolution: named list items within a block (e.g. workflow
    /// steps) are valid targets only within that same block, not globally.
    pub fn find_unresolved_references(&self, file: &File) -> Vec<NmlError> {
        let mut errors = Vec::new();
        for decl in &file.declarations {
            match &decl.kind {
                DeclarationKind::Block(block) => {
                    let is_schema_def = matches!(block.keyword.name.as_str(), "model" | "enum");
                    if !is_schema_def {
                        let local_names = collect_local_names(&block.body);
                        self.check_body_refs(&block.body, &local_names, &mut errors);
                    }
                }
                DeclarationKind::Array(arr) => {
                    let is_schema_def = matches!(arr.item_keyword.name.as_str(), "model" | "enum");
                    if !is_schema_def {
                        for item in &arr.body.items {
                            if let ListItemKind::Named { name: _, body } = &item.kind {
                                let local_names = collect_local_names(body);
                                self.check_body_refs(body, &local_names, &mut errors);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        errors
    }

    /// Detect cycles in const/template reference chains.
    ///
    /// A cycle exists when `const A = B` and `const B = A` (or longer chains).
    /// Returns an error for each const that participates in a cycle.
    pub fn find_const_cycles(&self) -> Vec<NmlError> {
        let mut errors = Vec::new();
        let mut globally_visited = HashSet::new();

        for name in self.const_values.keys() {
            if globally_visited.contains(name.as_str()) {
                continue;
            }
            let mut path = Vec::new();
            self.walk_const_chain(name, &mut path, &mut globally_visited, &mut errors);
        }
        errors
    }

    fn walk_const_chain(
        &self,
        name: &str,
        path: &mut Vec<String>,
        globally_visited: &mut HashSet<String>,
        errors: &mut Vec<NmlError>,
    ) {
        if let Some(pos) = path.iter().position(|n| n == name) {
            let cycle: Vec<_> = path[pos..].to_vec();
            for member in &cycle {
                let span = self
                    .declarations
                    .get(member.as_str())
                    .and_then(|v| v.first())
                    .map(|d| d.span)
                    .unwrap_or(Span::empty(0));
                errors.push(NmlError::Validation {
                    message: format!(
                        "circular reference in const/template chain: {}",
                        cycle
                            .iter()
                            .chain(std::iter::once(&cycle[0]))
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(" -> ")
                    ),
                    span,
                });
            }
            return;
        }

        if globally_visited.contains(name) {
            return;
        }

        if let Some(Value::Reference(ref_name)) = self.const_values.get(name) {
            if self.const_values.contains_key(ref_name.as_str()) {
                path.push(name.to_string());
                self.walk_const_chain(ref_name, path, globally_visited, errors);
                path.pop();
            }
        }

        globally_visited.insert(name.to_string());
    }

    fn check_body_refs(
        &self,
        body: &Body,
        local_names: &HashSet<String>,
        errors: &mut Vec<NmlError>,
    ) {
        for entry in &body.entries {
            match &entry.kind {
                BodyEntryKind::Property(prop) => {
                    if let Value::Reference(name) = &prop.value.value {
                        if self.lookup(name).is_none() && !local_names.contains(name.as_str()) {
                            errors.push(NmlError::Validation {
                                message: format!("unresolved reference '{name}'"),
                                span: prop.value.span,
                            });
                        }
                    }
                }
                BodyEntryKind::NestedBlock(nb) => {
                    self.check_body_refs(&nb.body, local_names, errors);
                }
                BodyEntryKind::ListItem(item) => {
                    if let ListItemKind::Named { body, .. } = &item.kind {
                        self.check_body_refs(body, local_names, errors);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Collect all named list item names from a body tree recursively.
/// These serve as locally-scoped references within the enclosing block.
fn collect_local_names(body: &Body) -> HashSet<String> {
    let mut names = HashSet::new();
    collect_local_names_recursive(body, &mut names);
    names
}

fn collect_local_names_recursive(body: &Body, names: &mut HashSet<String>) {
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::ListItem(item) => {
                if let ListItemKind::Named { name, body } = &item.kind {
                    names.insert(name.name.clone());
                    collect_local_names_recursive(body, names);
                }
            }
            BodyEntryKind::NestedBlock(nb) => {
                collect_local_names_recursive(&nb.body, names);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cst::parse_to_ast;

    #[test]
    fn test_resolve_reference() {
        let source = "service Svc:\n    localMount = \"/\"\n\nresource Res:\n    path = \"/\"\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        assert!(symbols.lookup("Svc").is_some());
        assert!(symbols.lookup("Res").is_some());
        assert!(symbols.lookup("Unknown").is_none());
    }

    #[test]
    fn test_unresolved_reference() {
        let source = "provider Groq:\n    type = \"groq\"\n\nworkflow W:\n    entrypoint = lasda\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_unresolved_references(&file);
        assert!(
            errors
                .iter()
                .any(|e| e.message().contains("unresolved reference 'lasda'")),
            "should flag unresolved reference; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_valid_reference_no_error() {
        let source = "provider Groq:\n    type = \"groq\"\n\nworkflow W:\n    provider = Groq\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_unresolved_references(&file);
        assert!(
            errors.is_empty(),
            "valid reference should not be flagged; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_unresolved_ref_in_list_item() {
        let source = "workflow W:\n    entrypoint = \"start\"\n    steps:\n        - s1:\n            provider = NonExistent\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_unresolved_references(&file);
        assert!(
            errors
                .iter()
                .any(|e| e.message().contains("unresolved reference 'NonExistent'")),
            "should flag unresolved reference inside list item; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_local_step_ref_resolves() {
        let source = "workflow W:\n    entrypoint = classify\n    steps:\n        - classify:\n            next = respond\n        - respond:\n            provider = \"groq\"\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_unresolved_references(&file);
        assert!(
            errors.is_empty(),
            "step refs within same workflow should resolve; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_step_ref_not_in_other_workflow() {
        let source = "workflow A:\n    entrypoint = respond\n    steps:\n        - start:\n            next = respond\n\nworkflow B:\n    entrypoint = respond\n    steps:\n        - respond:\n            provider = \"groq\"\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_unresolved_references(&file);
        assert!(
            errors
                .iter()
                .any(|e| e.message().contains("unresolved reference 'respond'")),
            "step 'respond' in workflow A should be unresolved (only exists in B); errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_top_level_ref_resolves_from_any_workflow() {
        let source = "provider Groq:\n    type = \"groq\"\n\nworkflow W:\n    steps:\n        - s1:\n            provider = Groq\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_unresolved_references(&file);
        assert!(
            errors.is_empty(),
            "top-level provider ref should resolve from any workflow; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_model_definitions_skipped() {
        let source = "model step:\n    provider string?\n    prompt prompt?\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_unresolved_references(&file);
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
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_duplicates();
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn test_const_cycle_direct() {
        let source = "const A = B\n\nconst B = A\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_const_cycles();
        assert!(
            !errors.is_empty(),
            "should detect cycle between A and B; errors: {:?}",
            errors
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message().contains("circular reference")),
            "error should mention circular reference; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_const_cycle_three_way() {
        let source = "const A = B\n\nconst B = C\n\nconst C = A\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_const_cycles();
        assert!(
            !errors.is_empty(),
            "should detect three-way cycle; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_const_no_cycle() {
        let source = "const A = B\n\nconst B = \"hello\"\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_const_cycles();
        assert!(
            errors.is_empty(),
            "should not detect cycle in valid chain; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_const_self_reference() {
        let source = "const A = A\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_const_cycles();
        assert!(
            !errors.is_empty(),
            "should detect self-referencing const; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_template_no_false_positive() {
        // Templates always hold string values, not references, so they can't form cycles.
        let source = "template Greeting:\n    \"Hello world\"\n\nconst Name = \"Alice\"\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_const_cycles();
        assert!(
            errors.is_empty(),
            "templates with string values should not trigger cycle detection; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_const_referencing_template_no_cycle() {
        // A const referencing a template name: const uses Value::Reference, but the
        // template stores a string value, so the chain terminates.
        let source = "template Prompt:\n    \"You are a helpful assistant.\"\n\nconst SystemPrompt = Prompt\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_const_cycles();
        assert!(
            errors.is_empty(),
            "const referencing a template should not be a cycle; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_multiple_disjoint_cycles() {
        let source = "const A = B\n\nconst B = A\n\nconst X = Y\n\nconst Y = X\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_const_cycles();
        assert!(
            errors.len() >= 4,
            "should detect both independent cycles (at least 4 errors); got: {:?}",
            errors
        );
    }

    #[test]
    fn test_long_acyclic_chain_no_false_positive() {
        let mut source = String::new();
        for i in 0..20 {
            source.push_str(&format!("const c{} = c{}\n\n", i, i + 1));
        }
        source.push_str("const c20 = \"end\"\n");
        let file = parse_to_ast(&source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_const_cycles();
        assert!(
            errors.is_empty(),
            "long acyclic chain should not produce false positives; errors: {:?}",
            errors
        );
    }

    #[test]
    fn test_resolve_const_value_follows_chain() {
        let source = "const A = B\n\nconst B = \"hello\"\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        assert_eq!(
            symbols.resolve_const_value("A"),
            Some(&Value::String("hello".into()))
        );
        assert_eq!(
            symbols.resolve_const_value("B"),
            Some(&Value::String("hello".into()))
        );
        assert_eq!(symbols.resolve_const_value("Missing"), None);
    }

    #[test]
    fn test_resolved_const_snapshot_chain_resolves_and_drops_cycles() {
        let source = "const A = B\n\nconst B = \"hello\"\n\nconst C = D\n\nconst D = C\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let snapshot = symbols.resolved_const_snapshot();
        // A and B both chain-resolve to "hello".
        assert_eq!(snapshot.get("A"), Some(&Value::String("hello".into())));
        assert_eq!(snapshot.get("B"), Some(&Value::String("hello".into())));
        // The C/D cycle is dropped (resolves to nothing).
        assert!(!snapshot.contains_key("C"));
        assert!(!snapshot.contains_key("D"));
    }

    #[test]
    fn test_resolve_const_value_cycle_returns_none() {
        let source = "const A = B\n\nconst B = A\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        assert_eq!(symbols.resolve_const_value("A"), None);
        assert_eq!(symbols.resolve_const_value("B"), None);
    }

    #[test]
    fn test_resolve_const_value_reference_to_non_const_kept() {
        // A reference to something that is not a const (e.g. a block name)
        // is returned as-is; it is not this symbols's job to fail it.
        let source = "service Svc:\n    x = 1\n\nconst A = Svc\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        assert_eq!(
            symbols.resolve_const_value("A"),
            Some(&Value::Reference("Svc".into()))
        );
    }

    #[test]
    fn test_const_cycle_error_message_contains_path() {
        let source = "const A = B\n\nconst B = C\n\nconst C = A\n";
        let file = parse_to_ast(source).unwrap();
        let mut symbols = SymbolTable::new();
        symbols.register_file(&file);

        let errors = symbols.find_const_cycles();
        let has_path = errors.iter().any(|e| {
            let msg = e.message();
            msg.contains("A -> B") || msg.contains("B -> C") || msg.contains("C -> A")
        });
        assert!(
            has_path,
            "error message should include the cycle path; errors: {:?}",
            errors
        );
    }
}
