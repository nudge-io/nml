use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use nml_core::ast::*;
use nml_core::model::{EnumDef, ModelDef};
use nml_core::span::SourceMap;

use crate::diagnostics;

pub struct NmlLanguageServer {
    client: Client,
    documents: Mutex<HashMap<Url, String>>,
    indexed_uris: Mutex<HashSet<Url>>,
    models: Mutex<Vec<ModelDef>>,
    enums: Mutex<Vec<EnumDef>>,
}

impl NmlLanguageServer {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Mutex::new(HashMap::new()),
            indexed_uris: Mutex::new(HashSet::new()),
            models: Mutex::new(Vec::new()),
            enums: Mutex::new(Vec::new()),
        }
    }

    fn find_nml_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name != "node_modules" && name != ".git" && !name.starts_with('.') {
                        Self::find_nml_files(&path, files);
                    }
                } else if path.extension().map_or(false, |e| e == "nml") {
                    files.push(path);
                }
            }
        }
    }

    fn index_workspace(&self, roots: &[Url]) {
        let mut docs = self.documents.lock().unwrap();
        let mut indexed = self.indexed_uris.lock().unwrap();
        for root in roots {
            let path = match root.to_file_path() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let mut files = Vec::new();
            Self::find_nml_files(&path, &mut files);
            for path in files {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(uri) = Url::from_file_path(&path) {
                        docs.insert(uri.clone(), content);
                        indexed.insert(uri);
                    }
                }
            }
        }
    }

    fn rebuild_schema_registry(&self) {
        let docs = self.documents.lock().unwrap();
        let mut all_models = Vec::new();
        let mut all_enums = Vec::new();

        for (uri, source) in docs.iter() {
            if !uri.as_str().ends_with(".model.nml") {
                continue;
            }
            if let Ok(file) = nml_core::parse(source) {
                let schema = nml_core::model_extract::extract(&file);
                all_models.extend(schema.models);
                all_enums.extend(schema.enums);
            }
        }

        *self.models.lock().unwrap() = all_models;
        *self.enums.lock().unwrap() = all_enums;
    }

    async fn on_change(&self, uri: Url, text: String) {
        self.documents
            .lock()
            .unwrap()
            .insert(uri.clone(), text.clone());

        let is_model_file = uri.as_str().ends_with(".model.nml");
        if is_model_file {
            self.rebuild_schema_registry();
        }

        let models = self.models.lock().unwrap().clone();
        let enums = self.enums.lock().unwrap().clone();
        let diags = diagnostics::compute(&text, &models, &enums);

        self.client
            .publish_diagnostics(uri.clone(), diags, None)
            .await;

        if is_model_file {
            self.revalidate_all_documents().await;
        }
    }

    async fn revalidate_all_documents(&self) {
        let docs: Vec<(Url, String)> = {
            let d = self.documents.lock().unwrap();
            d.iter().map(|(u, s)| (u.clone(), s.clone())).collect()
        };
        let models = self.models.lock().unwrap().clone();
        let enums = self.enums.lock().unwrap().clone();

        for (uri, source) in docs {
            if uri.as_str().ends_with(".model.nml") {
                continue;
            }
            let diags = diagnostics::compute(&source, &models, &enums);
            self.client.publish_diagnostics(uri, diags, None).await;
        }
    }

    fn find_definition(&self, name: &str, current_uri: &Url) -> Option<(Url, Range)> {
        let docs = self.documents.lock().unwrap();

        // Priority 1: Field definitions in .model.nml files
        for (uri, source) in docs.iter() {
            if !uri.as_str().ends_with(".model.nml") {
                continue;
            }
            if let Ok(file) = nml_core::parse(source) {
                let source_map = SourceMap::new(source);
                if let Some(range) = find_field_definition(&file, name, &source_map) {
                    return Some((uri.clone(), range));
                }
            }
        }

        // Priority 2: Top-level declaration names across all files
        for (uri, source) in docs.iter() {
            if let Ok(file) = nml_core::parse(source) {
                let source_map = SourceMap::new(source);
                if let Some(range) = find_top_level_decl(&file, name, &source_map) {
                    return Some((uri.clone(), range));
                }
            }
        }

        // Priority 3: Nested names in current file only
        if let Some(source) = docs.get(current_uri) {
            if let Ok(file) = nml_core::parse(source) {
                let source_map = SourceMap::new(source);
                if let Some(range) = find_name_in_file(&file, name, &source_map) {
                    return Some((current_uri.clone(), range));
                }
            } else if let Some(range) = find_name_by_text(source, name) {
                return Some((current_uri.clone(), range));
            }
        }

        None
    }
}

fn span_to_range(span: nml_core::span::Span, source_map: &SourceMap) -> Range {
    let start = source_map.location(span.start);
    let end = source_map.location(span.end);
    Range {
        start: Position::new(start.line as u32 - 1, start.column as u32 - 1),
        end: Position::new(end.line as u32 - 1, end.column as u32 - 1),
    }
}

fn find_field_definition(file: &File, name: &str, source_map: &SourceMap) -> Option<Range> {
    for decl in &file.declarations {
        if let DeclarationKind::Block(block) = &decl.kind {
            if matches!(block.keyword.name.as_str(), "model" | "trait") {
                for entry in &block.body.entries {
                    if let BodyEntryKind::FieldDefinition(fd) = &entry.kind {
                        if fd.name.name == name {
                            return Some(span_to_range(fd.name.span, source_map));
                        }
                    }
                }
            }
        }
    }
    None
}

fn find_top_level_decl(file: &File, name: &str, source_map: &SourceMap) -> Option<Range> {
    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => {
                if block.name.name == name {
                    return Some(span_to_range(block.name.span, source_map));
                }
            }
            DeclarationKind::Array(arr) => {
                if arr.name.name == name {
                    return Some(span_to_range(arr.name.span, source_map));
                }
            }
            DeclarationKind::Const(c) => {
                if c.name.name == name {
                    return Some(span_to_range(c.name.span, source_map));
                }
            }
            DeclarationKind::Template(t) => {
                if t.name.name == name {
                    return Some(span_to_range(t.name.span, source_map));
                }
            }
        }
    }
    None
}

fn find_name_in_file(file: &File, name: &str, source_map: &SourceMap) -> Option<Range> {
    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => {
                if block.name.name == name {
                    return Some(span_to_range(block.name.span, source_map));
                }
                if let Some(r) = find_name_in_body(&block.body, name, source_map) {
                    return Some(r);
                }
            }
            DeclarationKind::Array(arr) => {
                if arr.name.name == name {
                    return Some(span_to_range(arr.name.span, source_map));
                }
                for item in &arr.body.items {
                    if let Some(r) = find_name_in_list_item(item, name, source_map) {
                        return Some(r);
                    }
                }
            }
            DeclarationKind::Const(c) => {
                if c.name.name == name {
                    return Some(span_to_range(c.name.span, source_map));
                }
            }
            DeclarationKind::Template(t) => {
                if t.name.name == name {
                    return Some(span_to_range(t.name.span, source_map));
                }
            }
        }
    }
    None
}

fn find_name_in_body(body: &Body, name: &str, source_map: &SourceMap) -> Option<Range> {
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::ListItem(item) => {
                if let Some(r) = find_name_in_list_item(item, name, source_map) {
                    return Some(r);
                }
            }
            BodyEntryKind::NestedBlock(nb) => {
                if nb.name.name == name {
                    return Some(span_to_range(nb.name.span, source_map));
                }
                if let Some(r) = find_name_in_body(&nb.body, name, source_map) {
                    return Some(r);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_name_in_list_item(item: &ListItem, name: &str, source_map: &SourceMap) -> Option<Range> {
    match &item.kind {
        ListItemKind::Named { name: ident, body } => {
            if ident.name == name {
                return Some(span_to_range(ident.span, source_map));
            }
            find_name_in_body(body, name, source_map)
        }
        _ => None,
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for NmlLanguageServer {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let roots: Vec<Url> = params
            .workspace_folders
            .as_ref()
            .map(|folders| folders.iter().map(|f| f.uri.clone()).collect())
            .or_else(|| params.root_uri.clone().map(|u| vec![u]))
            .unwrap_or_default();
        if !roots.is_empty() {
            self.index_workspace(&roots);
            self.rebuild_schema_registry();
            self.client
                .log_message(
                    MessageType::INFO,
                    format!("NML: indexed {} workspace root(s)", roots.len()),
                )
                .await;
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "@".to_string(),
                        "|".to_string(),
                        ".".to_string(),
                        "$".to_string(),
                    ]),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "NML language server initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.on_change(
            params.text_document.uri,
            params.text_document.text,
        )
        .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            self.on_change(params.text_document.uri, change.text).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        let was_model = uri.as_str().ends_with(".model.nml");
        let indexed = self.indexed_uris.lock().unwrap().contains(&uri);
        if !indexed {
            self.documents.lock().unwrap().remove(&uri);
        }

        if was_model {
            self.rebuild_schema_registry();
            self.revalidate_all_documents().await;
        }
    }

    async fn completion(&self, _params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let mut items = Vec::new();

        let keywords = [
            "model", "trait", "enum", "service", "resource", "endpoint",
            "roleTemplate", "role", "member", "restriction", "webServer",
            "peer", "accessControl", "action", "trigger",
        ];
        for kw in keywords {
            items.push(CompletionItem {
                label: kw.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }

        let types = ["string", "number", "money", "bool", "duration", "path", "secret"];
        for t in types {
            items.push(CompletionItem {
                label: t.to_string(),
                kind: Some(CompletionItemKind::TYPE_PARAMETER),
                ..Default::default()
            });
        }

        let built_in_roles = ["@public", "@private", "@anyone", "@loggedIn", "@admin"];
        for role in built_in_roles {
            items.push(CompletionItem {
                label: role.to_string(),
                kind: Some(CompletionItemKind::CONSTANT),
                ..Default::default()
            });
        }

        let constraints = [
            "unique", "secret", "token", "distinct", "shorthand",
            "integer", "min", "max", "minLength", "maxLength", "pattern", "currency",
        ];
        for c in constraints {
            items.push(CompletionItem {
                label: c.to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some("constraint".to_string()),
                ..Default::default()
            });
        }

        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let docs = self.documents.lock().unwrap();
        let Some(source) = docs.get(&uri) else {
            return Ok(None);
        };

        let lines: Vec<&str> = source.lines().collect();
        let Some(line) = lines.get(pos.line as usize) else {
            return Ok(None);
        };

        let col = pos.character as usize;
        let word = extract_word_at(line, col);

        let info = match word.as_str() {
            "string" => Some("**string** -- Quoted text value"),
            "number" => Some("**number** -- General-purpose numeric (integer or decimal)"),
            "money" => Some("**money** -- Exact currency value with ISO 4217 code (e.g., `19.99 USD`)"),
            "bool" => Some("**bool** -- Boolean value (`true` or `false`)"),
            "duration" => Some("**duration** -- Time duration (e.g., `\"72h\"`, `\"30s\"`)"),
            "path" => Some("**path** -- URL path with variables and wildcards"),
            "secret" => Some("**secret** -- Value resolved from environment (`$ENV.X`)"),
            "model" => Some("**model** -- Define a custom object type"),
            "trait" => Some("**trait** -- Define a reusable group of fields"),
            "enum" => Some("**enum** -- Define a restricted set of allowed values"),
            _ => None,
        };

        Ok(info.map(|text| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: text.to_string(),
            }),
            range: None,
        }))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;

        let word = {
            let docs = self.documents.lock().unwrap();
            let Some(source) = docs.get(&uri) else {
                return Ok(None);
            };
            let lines: Vec<&str> = source.lines().collect();
            let Some(line) = lines.get(pos.line as usize) else {
                return Ok(None);
            };
            extract_word_at(line, pos.character as usize)
        };

        if word.is_empty() {
            return Ok(None);
        }

        if let Some((target_uri, range)) = self.find_definition(&word, &uri) {
            Ok(Some(GotoDefinitionResponse::Scalar(
                tower_lsp::lsp_types::Location {
                    uri: target_uri,
                    range,
                },
            )))
        } else {
            Ok(None)
        }
    }
}

fn find_name_by_text(source: &str, name: &str) -> Option<Range> {
    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.ends_with(':') {
            let before_colon = &trimmed[..trimmed.len() - 1];
            let parts: Vec<&str> = before_colon.split_whitespace().collect();
            if parts.len() == 2 && parts[1] == name {
                let col_start = line.find(name).unwrap_or(0) as u32;
                let col_end = col_start + name.len() as u32;
                return Some(Range {
                    start: Position::new(line_idx as u32, col_start),
                    end: Position::new(line_idx as u32, col_end),
                });
            }
        }
        if trimmed.starts_with('-') && trimmed.ends_with(':') {
            let inner = trimmed[1..trimmed.len() - 1].trim();
            if inner == name {
                let col_start = line.find(name).unwrap_or(0) as u32;
                let col_end = col_start + name.len() as u32;
                return Some(Range {
                    start: Position::new(line_idx as u32, col_start),
                    end: Position::new(line_idx as u32, col_end),
                });
            }
        }
    }
    None
}

fn extract_word_at(line: &str, col: usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    let col = col.min(chars.len());

    let start = chars[..col]
        .iter()
        .rposition(|c| !c.is_alphanumeric() && *c != '_' && *c != '-')
        .map(|p| p + 1)
        .unwrap_or(0);

    let end = chars[col..]
        .iter()
        .position(|c| !c.is_alphanumeric() && *c != '_' && *c != '-')
        .map(|p| col + p)
        .unwrap_or(chars.len());

    chars[start..end].iter().collect()
}
