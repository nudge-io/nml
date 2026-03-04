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
use nml_core::types::Value;

use crate::diagnostics;

const MAX_DIR_DEPTH: usize = 20;
const MAX_FILE_COUNT: usize = 10_000;

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

    fn find_nml_files(dir: &Path, files: &mut Vec<std::path::PathBuf>, depth: usize) {
        if depth > MAX_DIR_DEPTH || files.len() >= MAX_FILE_COUNT {
            return;
        }
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let is_symlink = entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(false);
            if is_symlink {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name != "node_modules" && name != ".git" && !name.starts_with('.') {
                    Self::find_nml_files(&path, files, depth + 1);
                }
            } else if path.extension().map_or(false, |e| e == "nml") {
                files.push(path);
                if files.len() >= MAX_FILE_COUNT {
                    return;
                }
            }
        }
    }

    fn index_workspace(&self, roots: &[Url]) {
        let mut docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
        let mut indexed = self.indexed_uris.lock().unwrap_or_else(|e| e.into_inner());
        for root in roots {
            let path = match root.to_file_path() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let mut files = Vec::new();
            Self::find_nml_files(&path, &mut files, 0);
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
        let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
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

        *self.models.lock().unwrap_or_else(|e| e.into_inner()) = all_models;
        *self.enums.lock().unwrap_or_else(|e| e.into_inner()) = all_enums;
    }

    async fn on_change(&self, uri: Url, text: String) {
        self.documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(uri.clone(), text.clone());

        let is_model_file = uri.as_str().ends_with(".model.nml");
        if is_model_file {
            self.rebuild_schema_registry();
        }

        let models = self
            .models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let enums = self
            .enums
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
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
            let d = self
                .documents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            d.iter().map(|(u, s)| (u.clone(), s.clone())).collect()
        };
        let models = self
            .models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let enums = self
            .enums
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        for (uri, source) in docs {
            if uri.as_str().ends_with(".model.nml") {
                continue;
            }
            let diags = diagnostics::compute(&source, &models, &enums);
            self.client.publish_diagnostics(uri, diags, None).await;
        }
    }

    fn find_definition(&self, name: &str, current_uri: &Url) -> Option<(Url, Range)> {
        let docs: HashMap<Url, String> = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        find_definition_in_docs(&docs, name, current_uri)
    }

    fn collect_declaration_names(&self) -> Vec<(String, String)> {
        let docs = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut names = Vec::new();
        for (_, source) in docs.iter() {
            if let Ok(file) = nml_core::parse(source) {
                for decl in &file.declarations {
                    match &decl.kind {
                        DeclarationKind::Block(block) => {
                            names.push((
                                block.name.name.clone(),
                                block.keyword.name.clone(),
                            ));
                        }
                        DeclarationKind::Array(arr) => {
                            names.push((
                                arr.name.name.clone(),
                                format!("[]{}", arr.item_keyword.name),
                            ));
                        }
                        DeclarationKind::Const(c) => {
                            names.push((c.name.name.clone(), "const".into()));
                        }
                        DeclarationKind::Template(t) => {
                            names.push((t.name.name.clone(), "template".into()));
                        }
                    }
                }
            }
        }
        names
    }
}

// ── Definition resolution ─────────────────────────────────────

fn find_definition_in_docs(
    docs: &HashMap<Url, String>,
    name: &str,
    current_uri: &Url,
) -> Option<(Url, Range)> {
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

    // Priority 2: Names in current file (top-level + nested)
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

    // Priority 3: Top-level declarations in other files
    for (uri, source) in docs.iter() {
        if uri == current_uri {
            continue;
        }
        if let Ok(file) = nml_core::parse(source) {
            let source_map = SourceMap::new(source);
            if let Some(range) = find_top_level_decl(&file, name, &source_map) {
                return Some((uri.clone(), range));
            }
        }
    }

    None
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

// ── Document symbols ──────────────────────────────────────────

fn build_document_symbols(file: &File, source_map: &SourceMap) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();
    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => {
                let range = span_to_range(decl.span, source_map);
                let selection_range = span_to_range(block.name.span, source_map);
                let children = build_body_symbols(&block.body, source_map);
                #[allow(deprecated)]
                symbols.push(DocumentSymbol {
                    name: block.name.name.clone(),
                    detail: Some(block.keyword.name.clone()),
                    kind: SymbolKind::CLASS,
                    tags: None,
                    deprecated: None,
                    range,
                    selection_range,
                    children: if children.is_empty() {
                        None
                    } else {
                        Some(children)
                    },
                });
            }
            DeclarationKind::Array(arr) => {
                let range = span_to_range(decl.span, source_map);
                let selection_range = span_to_range(arr.name.span, source_map);
                let children = build_array_body_symbols(&arr.body, source_map);
                #[allow(deprecated)]
                symbols.push(DocumentSymbol {
                    name: arr.name.name.clone(),
                    detail: Some(format!("[]{}", arr.item_keyword.name)),
                    kind: SymbolKind::ARRAY,
                    tags: None,
                    deprecated: None,
                    range,
                    selection_range,
                    children: if children.is_empty() {
                        None
                    } else {
                        Some(children)
                    },
                });
            }
            DeclarationKind::Const(c) => {
                let range = span_to_range(decl.span, source_map);
                let selection_range = span_to_range(c.name.span, source_map);
                #[allow(deprecated)]
                symbols.push(DocumentSymbol {
                    name: c.name.name.clone(),
                    detail: Some("const".into()),
                    kind: SymbolKind::CONSTANT,
                    tags: None,
                    deprecated: None,
                    range,
                    selection_range,
                    children: None,
                });
            }
            DeclarationKind::Template(t) => {
                let range = span_to_range(decl.span, source_map);
                let selection_range = span_to_range(t.name.span, source_map);
                #[allow(deprecated)]
                symbols.push(DocumentSymbol {
                    name: t.name.name.clone(),
                    detail: Some("template".into()),
                    kind: SymbolKind::STRING,
                    tags: None,
                    deprecated: None,
                    range,
                    selection_range,
                    children: None,
                });
            }
        }
    }
    symbols
}

fn build_body_symbols(body: &Body, source_map: &SourceMap) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::Property(prop) => {
                let range = span_to_range(entry.span, source_map);
                let selection_range = span_to_range(prop.name.span, source_map);
                #[allow(deprecated)]
                symbols.push(DocumentSymbol {
                    name: prop.name.name.clone(),
                    detail: None,
                    kind: SymbolKind::PROPERTY,
                    tags: None,
                    deprecated: None,
                    range,
                    selection_range,
                    children: None,
                });
            }
            BodyEntryKind::NestedBlock(nb) => {
                let range = span_to_range(entry.span, source_map);
                let selection_range = span_to_range(nb.name.span, source_map);
                let children = build_body_symbols(&nb.body, source_map);
                #[allow(deprecated)]
                symbols.push(DocumentSymbol {
                    name: nb.name.name.clone(),
                    detail: None,
                    kind: SymbolKind::FIELD,
                    tags: None,
                    deprecated: None,
                    range,
                    selection_range,
                    children: if children.is_empty() {
                        None
                    } else {
                        Some(children)
                    },
                });
            }
            BodyEntryKind::FieldDefinition(fd) => {
                let range = span_to_range(entry.span, source_map);
                let selection_range = span_to_range(fd.name.span, source_map);
                let type_name = match &fd.field_type {
                    FieldTypeExpr::Named(id) => id.name.clone(),
                    FieldTypeExpr::Array(id) => format!("[]{}", id.name),
                };
                #[allow(deprecated)]
                symbols.push(DocumentSymbol {
                    name: fd.name.name.clone(),
                    detail: Some(type_name),
                    kind: SymbolKind::FIELD,
                    tags: None,
                    deprecated: None,
                    range,
                    selection_range,
                    children: None,
                });
            }
            BodyEntryKind::ListItem(item) => {
                if let ListItemKind::Named { name, body } = &item.kind {
                    let range = span_to_range(item.span, source_map);
                    let selection_range = span_to_range(name.span, source_map);
                    let children = build_body_symbols(body, source_map);
                    #[allow(deprecated)]
                    symbols.push(DocumentSymbol {
                        name: name.name.clone(),
                        detail: None,
                        kind: SymbolKind::FIELD,
                        tags: None,
                        deprecated: None,
                        range,
                        selection_range,
                        children: if children.is_empty() {
                            None
                        } else {
                            Some(children)
                        },
                    });
                }
            }
            _ => {}
        }
    }
    symbols
}

fn build_array_body_symbols(body: &ArrayBody, source_map: &SourceMap) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();
    for item in &body.items {
        if let ListItemKind::Named { name, body } = &item.kind {
            let range = span_to_range(item.span, source_map);
            let selection_range = span_to_range(name.span, source_map);
            let children = build_body_symbols(body, source_map);
            #[allow(deprecated)]
            symbols.push(DocumentSymbol {
                name: name.name.clone(),
                detail: None,
                kind: SymbolKind::FIELD,
                tags: None,
                deprecated: None,
                range,
                selection_range,
                children: if children.is_empty() {
                    None
                } else {
                    Some(children)
                },
            });
        }
    }
    symbols
}

// ── References ────────────────────────────────────────────────

fn find_references_in_source(source: &str, name: &str, source_map: &SourceMap) -> Vec<Range> {
    let mut ranges = Vec::new();
    if let Ok(file) = nml_core::parse(source) {
        collect_references(&file, name, source_map, &mut ranges);
    }
    ranges
}

fn collect_references(
    file: &File,
    name: &str,
    source_map: &SourceMap,
    ranges: &mut Vec<Range>,
) {
    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => {
                if block.name.name == name {
                    ranges.push(span_to_range(block.name.span, source_map));
                }
                collect_body_references(&block.body, name, source_map, ranges);
            }
            DeclarationKind::Array(arr) => {
                if arr.name.name == name {
                    ranges.push(span_to_range(arr.name.span, source_map));
                }
                for item in &arr.body.items {
                    collect_list_item_references(item, name, source_map, ranges);
                }
            }
            DeclarationKind::Const(c) => {
                if c.name.name == name {
                    ranges.push(span_to_range(c.name.span, source_map));
                }
                if let Value::Reference(ref_name) = &c.value.value {
                    if ref_name == name {
                        ranges.push(span_to_range(c.value.span, source_map));
                    }
                }
            }
            DeclarationKind::Template(t) => {
                if t.name.name == name {
                    ranges.push(span_to_range(t.name.span, source_map));
                }
            }
        }
    }
}

fn collect_body_references(
    body: &Body,
    name: &str,
    source_map: &SourceMap,
    ranges: &mut Vec<Range>,
) {
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::Property(prop) => {
                if let Value::Reference(ref_name) = &prop.value.value {
                    if ref_name == name {
                        ranges.push(span_to_range(prop.value.span, source_map));
                    }
                }
            }
            BodyEntryKind::NestedBlock(nb) => {
                if nb.name.name == name {
                    ranges.push(span_to_range(nb.name.span, source_map));
                }
                collect_body_references(&nb.body, name, source_map, ranges);
            }
            BodyEntryKind::ListItem(item) => {
                collect_list_item_references(item, name, source_map, ranges);
            }
            _ => {}
        }
    }
}

fn collect_list_item_references(
    item: &ListItem,
    name: &str,
    source_map: &SourceMap,
    ranges: &mut Vec<Range>,
) {
    match &item.kind {
        ListItemKind::Named { name: ident, body } => {
            if ident.name == name {
                ranges.push(span_to_range(ident.span, source_map));
            }
            collect_body_references(body, name, source_map, ranges);
        }
        ListItemKind::Reference(ident) => {
            if ident.name == name {
                ranges.push(span_to_range(ident.span, source_map));
            }
        }
        _ => {}
    }
}

// ── Hover helpers ─────────────────────────────────────────────

fn summarize_body(body: &Body) -> String {
    let mut lines = Vec::new();
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::Property(prop) => {
                lines.push(format!(
                    "  {} = {}",
                    prop.name.name,
                    format_value(&prop.value.value)
                ));
            }
            BodyEntryKind::NestedBlock(nb) => {
                lines.push(format!("  {}:", nb.name.name));
            }
            BodyEntryKind::FieldDefinition(fd) => {
                let type_name = match &fd.field_type {
                    FieldTypeExpr::Named(id) => id.name.clone(),
                    FieldTypeExpr::Array(id) => format!("[]{}", id.name),
                };
                let opt = if fd.optional { "?" } else { "" };
                lines.push(format!("  {} {}{}", fd.name.name, type_name, opt));
            }
            _ => {}
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("```nml\n{}\n```", lines.join("\n"))
}

fn format_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{}\"", s),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Reference(r) => r.clone(),
        Value::Secret(s) => s.clone(),
        Value::RoleRef(r) => r.clone(),
        Value::Duration(d) => format!("\"{}\"", d),
        Value::Path(p) => format!("\"{}\"", p),
        _ => "...".to_string(),
    }
}

// ── LanguageServer implementation ─────────────────────────────

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
                        "=".to_string(),
                    ]),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                document_formatting_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let registration = Registration {
            id: "nml-file-watcher".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: Some(
                serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                    watchers: vec![FileSystemWatcher {
                        glob_pattern: GlobPattern::String("**/*.nml".to_string()),
                        kind: None,
                    }],
                })
                .unwrap_or_default(),
            ),
        };
        let _ = self.client.register_capability(vec![registration]).await;
        self.client
            .log_message(MessageType::INFO, "NML language server initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.on_change(params.text_document.uri, params.text_document.text)
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
        let indexed = self
            .indexed_uris
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&uri);
        if !indexed {
            self.documents
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&uri);
        }

        if was_model {
            self.rebuild_schema_registry();
            self.revalidate_all_documents().await;
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        for change in params.changes {
            match change.typ {
                FileChangeType::CREATED | FileChangeType::CHANGED => {
                    if let Ok(path) = change.uri.to_file_path() {
                        if let Ok(content) = fs::read_to_string(&path) {
                            self.on_change(change.uri, content).await;
                        }
                    }
                }
                FileChangeType::DELETED => {
                    self.documents
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&change.uri);
                    self.indexed_uris
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&change.uri);
                    if change.uri.as_str().ends_with(".model.nml") {
                        self.rebuild_schema_registry();
                        self.revalidate_all_documents().await;
                    }
                }
                _ => {}
            }
        }
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let mut items = Vec::new();
        let pos = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;

        let is_value_position = {
            let docs = self
                .documents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            docs.get(&uri)
                .and_then(|source| {
                    let lines: Vec<&str> = source.lines().collect();
                    let line = lines.get(pos.line as usize)?;
                    let end = (pos.character as usize).min(line.len());
                    Some(line[..end].contains('='))
                })
                .unwrap_or(false)
        };

        if is_value_position {
            let names = self.collect_declaration_names();
            for (name, keyword) in names {
                items.push(CompletionItem {
                    label: name,
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: Some(keyword),
                    ..Default::default()
                });
            }
        }

        let keywords = [
            "model",
            "trait",
            "enum",
            "service",
            "resource",
            "endpoint",
            "roleTemplate",
            "role",
            "member",
            "restriction",
            "webServer",
            "peer",
            "accessControl",
            "action",
            "trigger",
            "provider",
            "workflow",
            "app",
            "const",
            "template",
        ];
        for kw in keywords {
            items.push(CompletionItem {
                label: kw.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }

        let types = [
            "string", "number", "money", "bool", "duration", "path", "secret",
        ];
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
            "unique",
            "secret",
            "token",
            "distinct",
            "shorthand",
            "integer",
            "min",
            "max",
            "minLength",
            "maxLength",
            "pattern",
            "currency",
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

        let source_clone = {
            let docs = self
                .documents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match docs.get(&uri) {
                Some(s) => s.clone(),
                None => return Ok(None),
            }
        };

        let lines: Vec<&str> = source_clone.lines().collect();
        let Some(line) = lines.get(pos.line as usize) else {
            return Ok(None);
        };

        let word = extract_word_at(line, pos.character as usize);

        let builtin_info = match word.as_str() {
            "string" => Some("**string** -- Quoted text value"),
            "number" => Some("**number** -- General-purpose numeric (integer or decimal)"),
            "money" => {
                Some("**money** -- Exact currency value with ISO 4217 code (e.g., `19.99 USD`)")
            }
            "bool" => Some("**bool** -- Boolean value (`true` or `false`)"),
            "duration" => Some("**duration** -- Time duration (e.g., `\"72h\"`, `\"30s\"`)"),
            "path" => Some("**path** -- URL path with variables and wildcards"),
            "secret" => Some("**secret** -- Value resolved from environment (`$ENV.X`)"),
            "model" => Some("**model** -- Define a custom object type"),
            "trait" => Some("**trait** -- Define a reusable group of fields"),
            "enum" => Some("**enum** -- Define a restricted set of allowed values"),
            _ => None,
        };

        if let Some(text) = builtin_info {
            return Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: text.to_string(),
                }),
                range: None,
            }));
        }

        if !word.is_empty() {
            let docs = self
                .documents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for (_, source) in docs.iter() {
                if let Ok(file) = nml_core::parse(source) {
                    for decl in &file.declarations {
                        let (kw, decl_name, body_summary) = match &decl.kind {
                            DeclarationKind::Block(block) if block.name.name == word => {
                                let summary = summarize_body(&block.body);
                                (
                                    block.keyword.name.clone(),
                                    block.name.name.clone(),
                                    summary,
                                )
                            }
                            DeclarationKind::Array(arr) if arr.name.name == word => (
                                format!("[]{}", arr.item_keyword.name),
                                arr.name.name.clone(),
                                String::new(),
                            ),
                            DeclarationKind::Const(c) if c.name.name == word => {
                                let val = format_value(&c.value.value);
                                ("const".into(), c.name.name.clone(), val)
                            }
                            DeclarationKind::Template(t) if t.name.name == word => {
                                let val = format_value(&t.value.value);
                                ("template".into(), t.name.name.clone(), val)
                            }
                            _ => continue,
                        };

                        let mut text = format!("**{kw}** `{decl_name}`");
                        if !body_summary.is_empty() {
                            text.push_str("\n\n");
                            text.push_str(&body_summary);
                        }
                        return Ok(Some(Hover {
                            contents: HoverContents::Markup(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: text,
                            }),
                            range: None,
                        }));
                    }
                }
            }
        }

        Ok(None)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;

        let word = {
            let docs = self
                .documents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
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
            Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri: target_uri,
                range,
            })))
        } else {
            Ok(None)
        }
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let pos = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;

        let word = {
            let docs = self
                .documents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
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

        let docs: HashMap<Url, String> = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut locations = Vec::new();

        for (doc_uri, source) in &docs {
            let source_map = SourceMap::new(source);
            for range in find_references_in_source(source, &word, &source_map) {
                locations.push(Location {
                    uri: doc_uri.clone(),
                    range,
                });
            }
        }

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
        }
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let source_clone = {
            let docs = self
                .documents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match docs.get(&uri) {
                Some(s) => s.clone(),
                None => return Ok(None),
            }
        };

        let file = match nml_core::parse(&source_clone) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };

        let source_map = SourceMap::new(&source_clone);
        let symbols = build_document_symbols(&file, &source_map);
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let pos = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;

        let (word, source_clone) = {
            let docs = self
                .documents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(source) = docs.get(&uri) else {
                return Ok(None);
            };
            let lines: Vec<&str> = source.lines().collect();
            let Some(line) = lines.get(pos.line as usize) else {
                return Ok(None);
            };
            (
                extract_word_at(line, pos.character as usize),
                source.clone(),
            )
        };

        if word.is_empty() {
            return Ok(None);
        }

        let source_map = SourceMap::new(&source_clone);
        let refs = find_references_in_source(&source_clone, &word, &source_map);

        if refs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(
                refs.into_iter()
                    .map(|range| DocumentHighlight {
                        range,
                        kind: Some(DocumentHighlightKind::TEXT),
                    })
                    .collect(),
            ))
        }
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        let source_clone = {
            let docs = self
                .documents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match docs.get(&uri) {
                Some(s) => s.clone(),
                None => return Ok(None),
            }
        };

        let file = match nml_core::parse(&source_clone) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };

        let formatted = nml_fmt::formatter::format(&file);
        if formatted == source_clone {
            return Ok(None);
        }

        let line_count = source_clone.lines().count() as u32;
        let last_line_len = source_clone.lines().last().map_or(0, |l| l.len()) as u32;
        let (end_line, end_char) = if source_clone.ends_with('\n') {
            (line_count, 0)
        } else {
            (line_count.saturating_sub(1), last_line_len)
        };

        Ok(Some(vec![TextEdit {
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(end_line, end_char),
            },
            new_text: formatted,
        }]))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let pos = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;
        let new_name = params.new_name;

        let word = {
            let docs = self
                .documents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
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

        let docs: HashMap<Url, String> = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();

        for (doc_uri, source) in &docs {
            let source_map = SourceMap::new(source);
            let refs = find_references_in_source(source, &word, &source_map);
            if !refs.is_empty() {
                changes.insert(
                    doc_uri.clone(),
                    refs.into_iter()
                        .map(|range| TextEdit {
                            range,
                            new_text: new_name.clone(),
                        })
                        .collect(),
                );
            }
        }

        if changes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(WorkspaceEdit {
                changes: Some(changes),
                ..Default::default()
            }))
        }
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let pos = params.position;
        let uri = params.text_document.uri;

        let docs = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(source) = docs.get(&uri) else {
            return Ok(None);
        };
        let lines: Vec<&str> = source.lines().collect();
        let Some(line) = lines.get(pos.line as usize) else {
            return Ok(None);
        };

        let word = extract_word_at(line, pos.character as usize);
        if word.is_empty() {
            return Ok(None);
        }

        let chars: Vec<char> = line.chars().collect();
        let col = (pos.character as usize).min(chars.len());
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

        Ok(Some(PrepareRenameResponse::Range(Range {
            start: Position::new(pos.line, start as u32),
            end: Position::new(pos.line, end as u32),
        })))
    }
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // ── extract_word_at ───────────────────────────────────────

    #[test]
    fn extract_word_in_middle() {
        assert_eq!(extract_word_at("hello world", 7), "world");
    }

    #[test]
    fn extract_word_at_line_start() {
        assert_eq!(extract_word_at("provider GroqFast:", 3), "provider");
    }

    #[test]
    fn extract_word_at_line_end() {
        assert_eq!(extract_word_at("foo = Bar", 8), "Bar");
    }

    #[test]
    fn extract_word_with_hyphens_underscores() {
        assert_eq!(extract_word_at("my-service_name", 5), "my-service_name");
    }

    #[test]
    fn extract_word_on_whitespace() {
        assert_eq!(extract_word_at("foo   bar", 4), "");
    }

    #[test]
    fn extract_word_empty_line() {
        assert_eq!(extract_word_at("", 0), "");
    }

    #[test]
    fn extract_word_on_equals() {
        assert_eq!(extract_word_at("key = val", 4), "");
    }

    #[test]
    fn extract_word_past_end() {
        assert_eq!(extract_word_at("foo", 100), "foo");
    }

    // ── find_name_by_text ─────────────────────────────────────

    #[test]
    fn find_by_text_keyword_name_colon() {
        let source = "provider GroqFast:\n    type = \"groq\"";
        let result = find_name_by_text(source, "GroqFast");
        assert!(result.is_some());
        let range = result.unwrap();
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 9);
    }

    #[test]
    fn find_by_text_dash_name_colon() {
        let source = "steps:\n    - myStep:\n        provider = Groq";
        let result = find_name_by_text(source, "myStep");
        assert!(result.is_some());
        let range = result.unwrap();
        assert_eq!(range.start.line, 1);
    }

    #[test]
    fn find_by_text_not_found() {
        let source = "provider GroqFast:\n    type = \"groq\"";
        assert!(find_name_by_text(source, "NonExistent").is_none());
    }

    #[test]
    fn find_by_text_ignores_values() {
        let source = "provider = GroqFast";
        assert!(find_name_by_text(source, "GroqFast").is_none());
    }

    // ── span_to_range ─────────────────────────────────────────

    #[test]
    fn span_to_range_single_line() {
        let source = "provider GroqFast:";
        let source_map = SourceMap::new(source);
        let span = nml_core::span::Span::new(9, 17);
        let range = span_to_range(span, &source_map);
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 9);
        assert_eq!(range.end.line, 0);
        assert_eq!(range.end.character, 17);
    }

    #[test]
    fn span_to_range_multi_line() {
        let source = "hello\nworld";
        let source_map = SourceMap::new(source);
        let span = nml_core::span::Span::new(6, 11);
        let range = span_to_range(span, &source_map);
        assert_eq!(range.start.line, 1);
        assert_eq!(range.start.character, 0);
        assert_eq!(range.end.line, 1);
        assert_eq!(range.end.character, 5);
    }

    // ── find_top_level_decl ───────────────────────────────────

    #[test]
    fn find_top_level_block() {
        let source = "provider GroqFast:\n    type = \"groq\"\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);
        assert!(find_top_level_decl(&file, "GroqFast", &source_map).is_some());
    }

    #[test]
    fn find_top_level_const() {
        let source = "const Limit = 100\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);
        assert!(find_top_level_decl(&file, "Limit", &source_map).is_some());
    }

    #[test]
    fn find_top_level_not_found() {
        let source = "provider GroqFast:\n    type = \"groq\"\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);
        assert!(find_top_level_decl(&file, "NonExistent", &source_map).is_none());
    }

    // ── find_field_definition ─────────────────────────────────

    #[test]
    fn find_field_in_model() {
        let source = "model user:\n    name string\n    email string\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);
        assert!(find_field_definition(&file, "email", &source_map).is_some());
    }

    #[test]
    fn find_field_in_trait() {
        let source = "trait timestamped:\n    createdAt string\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);
        assert!(find_field_definition(&file, "createdAt", &source_map).is_some());
    }

    #[test]
    fn find_field_ignores_non_model() {
        let source = "service Svc:\n    localMount = \"/\"\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);
        assert!(find_field_definition(&file, "localMount", &source_map).is_none());
    }

    #[test]
    fn find_field_not_found() {
        let source = "model user:\n    name string\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);
        assert!(find_field_definition(&file, "nonexistent", &source_map).is_none());
    }

    // ── find_name_in_file ─────────────────────────────────────

    #[test]
    fn find_name_top_level() {
        let source = "provider GroqFast:\n    type = \"groq\"\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);
        assert!(find_name_in_file(&file, "GroqFast", &source_map).is_some());
    }

    #[test]
    fn find_name_nested_block() {
        let source =
            "workflow W:\n    entrypoint = \"start\"\n    steps:\n        - s1:\n            provider = GroqFast\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);
        assert!(find_name_in_file(&file, "steps", &source_map).is_some());
    }

    #[test]
    fn find_name_list_item() {
        let source = "workflow W:\n    entrypoint = \"start\"\n    steps:\n        - myStep:\n            provider = GroqFast\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);
        assert!(find_name_in_file(&file, "myStep", &source_map).is_some());
    }

    #[test]
    fn find_name_not_found_in_file() {
        let source = "provider GroqFast:\n    type = \"groq\"\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);
        assert!(find_name_in_file(&file, "NonExistent", &source_map).is_none());
    }

    // ── find_definition_in_docs (priority + regression) ───────

    fn make_uri(name: &str) -> Url {
        Url::parse(&format!("file:///workspace/{name}")).unwrap()
    }

    #[test]
    fn definition_prefers_current_file() {
        let mut docs = HashMap::new();
        let current = make_uri("async-agent-test.workflow.nml");
        let other = make_uri("simple-chat.workflow.nml");

        docs.insert(
            current.clone(),
            "provider GroqFast:\n    type = \"groq\"\n    model = \"llama-3.3-70b-versatile\"\n"
                .to_string(),
        );
        docs.insert(
            other.clone(),
            "provider GroqFast:\n    type = \"groq\"\n    model = \"llama-3.1-8b-instant\"\n"
                .to_string(),
        );

        let result = find_definition_in_docs(&docs, "GroqFast", &current);
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        assert_eq!(
            uri, current,
            "should resolve to current file, not other file"
        );
    }

    #[test]
    fn definition_model_field_first() {
        let mut docs = HashMap::new();
        let model_uri = make_uri("schema.model.nml");
        let current = make_uri("app.nml");

        docs.insert(
            model_uri.clone(),
            "model user:\n    name string\n    email string\n".to_string(),
        );
        docs.insert(
            current.clone(),
            "service Svc:\n    name = \"test\"\n".to_string(),
        );

        let result = find_definition_in_docs(&docs, "name", &current);
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        assert_eq!(uri, model_uri, "model field should take priority");
    }

    #[test]
    fn definition_falls_back_to_other_file() {
        let mut docs = HashMap::new();
        let current = make_uri("app.nml");
        let other = make_uri("providers.nml");

        docs.insert(
            current.clone(),
            "workflow W:\n    provider = GroqFast\n".to_string(),
        );
        docs.insert(
            other.clone(),
            "provider GroqFast:\n    type = \"groq\"\n".to_string(),
        );

        let result = find_definition_in_docs(&docs, "GroqFast", &current);
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        assert_eq!(uri, other);
    }

    #[test]
    fn definition_nested_name_in_current() {
        let mut docs = HashMap::new();
        let current = make_uri("workflow.nml");
        docs.insert(
            current.clone(),
            "workflow W:\n    entrypoint = \"start\"\n    steps:\n        - myStep:\n            provider = GroqFast\n"
                .to_string(),
        );

        let result = find_definition_in_docs(&docs, "myStep", &current);
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        assert_eq!(uri, current);
    }

    #[test]
    fn definition_not_found() {
        let mut docs = HashMap::new();
        let current = make_uri("app.nml");
        docs.insert(
            current.clone(),
            "workflow W:\n    entrypoint = \"start\"\n".to_string(),
        );

        assert!(find_definition_in_docs(&docs, "NonExistent", &current).is_none());
    }
}
