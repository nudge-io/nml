use nml_core::ast::File;
use nml_core::model::ModelDef;

use crate::diagnostics::Diagnostic;

/// Validates instance declarations against model definitions.
pub struct SchemaValidator {
    models: Vec<ModelDef>,
}

impl SchemaValidator {
    pub fn new(models: Vec<ModelDef>) -> Self {
        Self { models }
    }

    /// Validate a parsed NML file against the loaded models.
    pub fn validate(&self, _file: &File) -> Vec<Diagnostic> {
        // TODO: implement full validation
        Vec::new()
    }

    pub fn find_model(&self, name: &str) -> Option<&ModelDef> {
        self.models.iter().find(|m| m.name == name)
    }
}
