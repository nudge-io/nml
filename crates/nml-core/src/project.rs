//! Project configuration parsed from `nml-project.nml`.
//!
//! The project file is an NML file that configures language tooling behavior
//! for a workspace root. It declares schema-package pins (RFC 0030), valid
//! template namespaces, allowed modifiers, and extra keywords to suggest in
//! completions. Individual tools may choose which fields to enforce. Tools
//! resolve the file per root, nearest-ancestor-wins, and never merge configs
//! across nesting levels.

use crate::ast::*;
use crate::types::Value;

/// Project-level configuration for NML tooling.
#[derive(Debug, Clone)]
pub struct ProjectConfig {
    /// Schema-package pins (RFC 0030): package *names* this root binds. A pin
    /// is authoritative over auto-association; the definition it resolves to
    /// is always the freshest available (workspace manifest, else the store's
    /// `current`).
    pub schema_packages: Vec<String>,
    /// Whether unpinned files may auto-associate with a known schema package
    /// (RFC 0030). `false` opts this root out — the escape hatch for repos
    /// holding doc/example files that happen to match a package's globs.
    pub auto_associate: bool,
    /// Valid template expression namespaces (e.g., `["args", "input", "steps"]`).
    /// If empty, any namespace is accepted without warnings.
    pub template_namespaces: Vec<String>,
    /// Valid modifier names (e.g., `["allow", "deny"]`).
    /// If empty, all modifier names are accepted without warnings.
    pub modifiers: Vec<String>,
    /// Additional block keywords to suggest in completions.
    pub keywords: Vec<String>,
    /// Block keywords whose bodies participate in membership cycle detection
    /// (e.g., `["role", "plan"]`).  If empty, no cycle detection is performed.
    pub member_keywords: Vec<String>,
    /// Reserved built-in references that should not appear in member lists
    /// (e.g., `["@public", "@authenticated"]`).
    pub builtin_refs: Vec<String>,
    /// Prefix for references targeting individual principals, warned about
    /// when used in access-control modifier rules (e.g., `"@user/"`).
    pub user_ref_prefix: Option<String>,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            schema_packages: Vec::new(),
            // Auto-association is the zero-config path; opting out is the
            // explicit act.
            auto_associate: true,
            template_namespaces: Vec::new(),
            modifiers: Vec::new(),
            keywords: Vec::new(),
            member_keywords: Vec::new(),
            builtin_refs: Vec::new(),
            user_ref_prefix: None,
        }
    }
}

impl ProjectConfig {
    /// Parse a `ProjectConfig` from a parsed NML file.
    ///
    /// Expects a top-level block like:
    /// ```nml
    /// project MyProject:
    ///     autoAssociate = false
    ///     schemaPackages:
    ///         - nudge
    ///     templateNamespaces = ["args", "input", "steps", "artifacts"]
    ///     modifiers = ["allow", "deny", "grant"]
    ///     keywords = ["service", "workflow", "database"]
    ///     memberKeywords = ["role", "plan"]
    ///     builtinRefs = ["@public", "@authenticated"]
    ///     userRefPrefix = "@user/"
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
                        "memberKeywords" => {
                            config.member_keywords = extract_string_array(&prop.value.value);
                        }
                        "builtinRefs" => {
                            config.builtin_refs = extract_string_array(&prop.value.value);
                        }
                        "userRefPrefix" => {
                            if let Value::String(s) = &prop.value.value {
                                config.user_ref_prefix = Some(s.clone());
                            }
                        }
                        "autoAssociate" => {
                            if let Value::Bool(b) = &prop.value.value {
                                config.auto_associate = *b;
                            }
                        }
                        _ => {}
                    }
                }
                BodyEntryKind::NestedBlock(nested) if nested.name.name == "schemaPackages" => {
                    for pin_entry in &nested.body.entries {
                        if let BodyEntryKind::ListItem(item) = &pin_entry.kind {
                            match &item.kind {
                                ListItemKind::Reference(id) => {
                                    config.schema_packages.push(id.name.clone());
                                }
                                ListItemKind::Shorthand { value, .. } => {
                                    if let Value::String(s) = &value.value {
                                        config.schema_packages.push(s.clone());
                                    }
                                }
                                _ => {}
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
    use crate::cst::parse_to_ast;

    #[test]
    fn parse_full_project_config() {
        let source = r#"
project MyProject:
    autoAssociate = false
    schemaPackages:
        - nudge
        - "other-pkg"
    templateNamespaces = ["args", "input", "steps", "env"]
    modifiers = ["allow", "deny"]
    keywords = ["service", "workflow", "database"]
    memberKeywords = ["role", "plan"]
    builtinRefs = ["@public", "@authenticated"]
    userRefPrefix = "@user/"
"#;
        let file = parse_to_ast(source).unwrap();
        let config = ProjectConfig::from_file(&file);
        assert_eq!(config.schema_packages, vec!["nudge", "other-pkg"]);
        assert!(!config.auto_associate);
        assert_eq!(
            config.template_namespaces,
            vec!["args", "input", "steps", "env"]
        );
        assert_eq!(config.modifiers, vec!["allow", "deny"]);
        assert_eq!(config.keywords, vec!["service", "workflow", "database"]);
        assert_eq!(config.member_keywords, vec!["role", "plan"]);
        assert_eq!(config.builtin_refs, vec!["@public", "@authenticated"]);
        assert_eq!(config.user_ref_prefix, Some("@user/".to_string()));
    }

    #[test]
    fn parse_empty_project_config() {
        let source = "project Empty:\n    templateNamespaces = []\n";
        let file = parse_to_ast(source).unwrap();
        let config = ProjectConfig::from_file(&file);
        assert!(config.schema_packages.is_empty());
        assert!(config.auto_associate, "auto-association defaults on");
        assert!(config.template_namespaces.is_empty());
        assert!(config.modifiers.is_empty());
        assert!(config.keywords.is_empty());
    }

    #[test]
    fn no_project_block_returns_defaults() {
        let source = "service MyService:\n    port = 3000\n";
        let file = parse_to_ast(source).unwrap();
        let config = ProjectConfig::from_file(&file);
        assert!(config.schema_packages.is_empty());
        assert!(config.auto_associate, "auto-association defaults on");
        assert!(config.template_namespaces.is_empty());
    }
}
