//! Project configuration parsed from `nml-project.nml`.
//!
//! The project file is an NML file that configures language tooling behavior
//! for a workspace. It declares preferred schema paths, valid template
//! namespaces, allowed modifiers, and extra keywords to suggest in completions.
//! Individual tools may choose which fields to enforce.

use crate::ast::*;
use crate::types::Value;

/// Project-level configuration for NML tooling.
#[derive(Debug, Clone, Default)]
pub struct ProjectConfig {
    /// Glob patterns or paths to schema (`.model.nml`) files.
    /// Tooling that supports explicit schema selection can use this list.
    pub schema_files: Vec<String>,
    /// Valid template expression namespaces (e.g., `["args", "input", "steps"]`).
    /// If empty, any namespace is accepted without warnings.
    pub template_namespaces: Vec<String>,
    /// Valid modifier names (e.g., `["allow", "deny"]`).
    /// If empty, the default set is used.
    pub modifiers: Vec<String>,
    /// Additional block keywords to suggest in completions.
    pub keywords: Vec<String>,
}

impl ProjectConfig {
    /// Parse a `ProjectConfig` from a parsed NML file.
    ///
    /// Expects a top-level block like:
    /// ```nml
    /// project MyProject:
    ///     schema:
    ///         - "schemas/core.model.nml"
    ///         - "schemas/api.model.nml"
    ///     templateNamespaces = ["args", "input", "steps", "artifacts"]
    ///     modifiers = ["allow", "deny", "grant"]
    ///     keywords = ["service", "workflow", "database"]
    /// ```
    pub fn from_file(file: &File) -> Self {
        let mut config = Self::default();

        for decl in &file.declarations {
            if let DeclarationKind::Block(block) = &decl.kind {
                if block.keyword.name == "project" {
                    Self::parse_body(&block.body, &mut config);
                }
            }
        }

        config
    }

    fn parse_body(body: &Body, config: &mut ProjectConfig) {
        for entry in &body.entries {
            match &entry.kind {
                BodyEntryKind::Property(prop) => {
                    let name = &prop.name.name;
                    match name.as_str() {
                        "templateNamespaces" => {
                            config.template_namespaces = extract_string_array(&prop.value.value);
                        }
                        "modifiers" => {
                            config.modifiers = extract_string_array(&prop.value.value);
                        }
                        "keywords" => {
                            config.keywords = extract_string_array(&prop.value.value);
                        }
                        _ => {}
                    }
                }
                BodyEntryKind::NestedBlock(nested) => {
                    if nested.name.name == "schema" {
                        for schema_entry in &nested.body.entries {
                            if let BodyEntryKind::ListItem(item) = &schema_entry.kind {
                                if let ListItemKind::Shorthand(val) = &item.kind {
                                    if let Value::String(s) = &val.value {
                                        config.schema_files.push(s.clone());
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn extract_string_array(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|sv| match &sv.value {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn parse_full_project_config() {
        let source = r#"
project MyProject:
    schema:
        - "schemas/core.model.nml"
        - "schemas/api.model.nml"
    templateNamespaces = ["args", "input", "steps", "env"]
    modifiers = ["allow", "deny"]
    keywords = ["service", "workflow", "database"]
"#;
        let file = parser::parse(source).unwrap();
        let config = ProjectConfig::from_file(&file);
        assert_eq!(config.schema_files, vec![
            "schemas/core.model.nml",
            "schemas/api.model.nml",
        ]);
        assert_eq!(config.template_namespaces, vec!["args", "input", "steps", "env"]);
        assert_eq!(config.modifiers, vec!["allow", "deny"]);
        assert_eq!(config.keywords, vec!["service", "workflow", "database"]);
    }

    #[test]
    fn parse_empty_project_config() {
        let source = "project Empty:\n    templateNamespaces = []\n";
        let file = parser::parse(source).unwrap();
        let config = ProjectConfig::from_file(&file);
        assert!(config.schema_files.is_empty());
        assert!(config.template_namespaces.is_empty());
        assert!(config.modifiers.is_empty());
        assert!(config.keywords.is_empty());
    }

    #[test]
    fn no_project_block_returns_defaults() {
        let source = "service MyService:\n    port = 3000\n";
        let file = parser::parse(source).unwrap();
        let config = ProjectConfig::from_file(&file);
        assert!(config.schema_files.is_empty());
        assert!(config.template_namespaces.is_empty());
    }
}
