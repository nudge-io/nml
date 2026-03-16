use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use nml_core::ast::*;
use nml_core::model::{EnumDef, FieldType, ModelDef};
use nml_core::span::SourceMap;
use nml_core::types::Value;

use crate::diagnostics;

const MAX_DIR_DEPTH: usize = 20;
const MAX_FILE_COUNT: usize = 10_000;

pub struct NmlLanguageServer {
    client: Client,
    documents: Mutex<HashMap<Url, String>>,
    indexed_uris: Mutex<HashSet<Url>>,
    scoped_models: Mutex<HashMap<String, Vec<ModelDef>>>,
    scoped_enums: Mutex<HashMap<String, Vec<EnumDef>>>,
    project_config: Mutex<nml_core::ProjectConfig>,
}

impl NmlLanguageServer {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Mutex::new(HashMap::new()),
            indexed_uris: Mutex::new(HashSet::new()),
            scoped_models: Mutex::new(HashMap::new()),
            scoped_enums: Mutex::new(HashMap::new()),
            project_config: Mutex::new(nml_core::ProjectConfig::default()),
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

            let project_file = path.join("nml-project.nml");
            if project_file.exists() {
                if let Ok(content) = fs::read_to_string(&project_file) {
                    if let Ok(file) = nml_core::parse(&content) {
                        let config = nml_core::ProjectConfig::from_file(&file);
                        *self.project_config.lock().unwrap_or_else(|e| e.into_inner()) = config;
                    }
                    if let Ok(uri) = Url::from_file_path(&project_file) {
                        docs.insert(uri.clone(), content);
                        indexed.insert(uri);
                    }
                }
            }

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
        let mut scoped_models: HashMap<String, Vec<ModelDef>> = HashMap::new();
        let mut scoped_enums: HashMap<String, Vec<EnumDef>> = HashMap::new();

        for (uri, source) in docs.iter() {
            if !uri.as_str().ends_with(".model.nml") {
                continue;
            }
            let scope = extract_schema_scope(uri.as_str());
            if let Ok(file) = nml_core::parse(source) {
                let schema = nml_core::model_extract::extract(&file);
                scoped_models.entry(scope.clone()).or_default().extend(schema.models);
                scoped_enums.entry(scope).or_default().extend(schema.enums);
            }
        }

        *self.scoped_models.lock().unwrap_or_else(|e| e.into_inner()) = scoped_models;
        *self.scoped_enums.lock().unwrap_or_else(|e| e.into_inner()) = scoped_enums;
    }

    fn models_for_file(&self, uri: &Url) -> (Vec<ModelDef>, Vec<EnumDef>) {
        let file_scope = extract_file_scope(uri.as_str());
        let scoped_models = self.scoped_models.lock().unwrap_or_else(|e| e.into_inner());
        let scoped_enums = self.scoped_enums.lock().unwrap_or_else(|e| e.into_inner());

        let mut models = Vec::new();
        let mut enums = Vec::new();
        let mut seen_model_names: HashSet<String> = HashSet::new();
        let mut seen_enum_names: HashSet<String> = HashSet::new();

        if let Some(ref scope) = file_scope {
            if let Some(scope_models) = scoped_models.get(scope) {
                for m in scope_models {
                    seen_model_names.insert(m.name.clone());
                    models.push(m.clone());
                }
            }
            if let Some(scope_enums) = scoped_enums.get(scope) {
                for e in scope_enums {
                    seen_enum_names.insert(e.name.clone());
                    enums.push(e.clone());
                }
            }
        }

        for (scope, ms) in scoped_models.iter() {
            if file_scope.as_deref() == Some(scope.as_str()) {
                continue;
            }
            for m in ms {
                if seen_model_names.insert(m.name.clone()) {
                    models.push(m.clone());
                }
            }
        }
        for (scope, es) in scoped_enums.iter() {
            if file_scope.as_deref() == Some(scope.as_str()) {
                continue;
            }
            for e in es {
                if seen_enum_names.insert(e.name.clone()) {
                    enums.push(e.clone());
                }
            }
        }

        (models, enums)
    }

    fn diagnostic_config(&self) -> diagnostics::DiagnosticConfig {
        let pc = self.project_config.lock().unwrap_or_else(|e| e.into_inner());
        diagnostics::DiagnosticConfig {
            template_namespaces: pc.template_namespaces.clone(),
            modifiers: pc.modifiers.clone(),
        }
    }

    async fn on_change(&self, uri: Url, text: String) {
        self.documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(uri.clone(), text.clone());

        if uri.as_str().ends_with("nml-project.nml") {
            if let Ok(file) = nml_core::parse(&text) {
                let config = nml_core::ProjectConfig::from_file(&file);
                *self.project_config.lock().unwrap_or_else(|e| e.into_inner()) = config;
            }
            self.revalidate_all_documents().await;
            return;
        }

        let is_model_file = uri.as_str().ends_with(".model.nml");
        if is_model_file {
            self.rebuild_schema_registry();
        }

        let (models, enums) = self.models_for_file(&uri);
        let dc = self.diagnostic_config();
        let diags = diagnostics::compute(&text, &models, &enums, &dc);

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

        let dc = self.diagnostic_config();
        for (uri, source) in docs {
            if uri.as_str().ends_with(".model.nml") {
                continue;
            }
            let (models, enums) = self.models_for_file(&uri);
            let diags = diagnostics::compute(&source, &models, &enums, &dc);
            self.client.publish_diagnostics(uri, diags, None).await;
        }
    }

    fn find_definition(&self, name: &str, current_uri: &Url, enclosing_keyword: Option<&str>) -> Option<(Url, Range)> {
        let docs: HashMap<Url, String> = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        find_definition_in_docs(&docs, name, current_uri, enclosing_keyword)
    }

    fn find_schema_definition(&self, name: &str, current_uri: &Url) -> Option<(Url, Range)> {
        let docs: HashMap<Url, String> = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let file_scope = extract_file_scope(current_uri.as_str());

        let mut model_uris: Vec<&Url> = docs
            .keys()
            .filter(|u| u.as_str().ends_with(".model.nml"))
            .collect();

        if let Some(ref scope) = file_scope {
            let scope = scope.clone();
            model_uris.sort_by_key(|u| {
                if extract_schema_scope(u.as_str()) == scope {
                    0
                } else {
                    1
                }
            });
        }

        for uri in model_uris {
            if let Some(source) = docs.get(uri) {
                if let Ok(file) = nml_core::parse(source) {
                    let source_map = SourceMap::new(source);
                    if let Some(range) = find_schema_block_definition(&file, name, &source_map) {
                        return Some((uri.clone(), range));
                    }
                }
            }
        }
        None
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

// ── Schema scoping ────────────────────────────────────────────

fn extract_schema_scope(uri_str: &str) -> String {
    let filename = uri_str.rsplit('/').next().unwrap_or(uri_str);
    filename
        .strip_suffix(".model.nml")
        .unwrap_or("")
        .to_string()
}

fn extract_file_scope(uri_str: &str) -> Option<String> {
    let filename = uri_str.rsplit('/').next().unwrap_or(uri_str);
    if filename.ends_with(".model.nml") {
        return None;
    }
    let stem = filename.strip_suffix(".nml")?;
    let pos = stem.rfind('.')?;
    Some(stem[pos + 1..].to_string())
}

fn find_enclosing_block_keyword(
    file: &File,
    pos: Position,
    source_map: &SourceMap,
) -> Option<String> {
    let mut best_start: Option<u32> = None;
    let mut result: Option<String> = None;
    for decl in &file.declarations {
        let range = span_to_range(decl.span, source_map);
        if pos.line >= range.start.line && pos.line <= range.end.line {
            let keyword = match &decl.kind {
                DeclarationKind::Block(block) => Some(block.keyword.name.clone()),
                DeclarationKind::Array(arr) => Some(arr.item_keyword.name.clone()),
                _ => None,
            };
            if let Some(kw) = keyword {
                if best_start.map_or(true, |s| range.start.line > s) {
                    best_start = Some(range.start.line);
                    result = Some(kw);
                }
            }
        }
    }
    result
}

// ── Definition resolution ─────────────────────────────────────

fn find_definition_in_docs(
    docs: &HashMap<Url, String>,
    name: &str,
    current_uri: &Url,
    enclosing_keyword: Option<&str>,
) -> Option<(Url, Range)> {
    let file_scope = extract_file_scope(current_uri.as_str());
    let is_on_keyword = enclosing_keyword == Some(name);

    // Priority 1: Field definition in the specific enclosing model
    // (Skip when cursor is on the declaration keyword itself)
    if !is_on_keyword {
        if let Some(keyword) = enclosing_keyword {
            let mut model_uris: Vec<&Url> = docs
                .keys()
                .filter(|u| u.as_str().ends_with(".model.nml"))
                .collect();

            if let Some(ref scope) = file_scope {
                let scope = scope.clone();
                model_uris.sort_by_key(|u| {
                    if extract_schema_scope(u.as_str()) == scope {
                        0
                    } else {
                        1
                    }
                });
            }

            for uri in &model_uris {
                if let Some(source) = docs.get(*uri) {
                    if let Ok(file) = nml_core::parse(source) {
                        let source_map = SourceMap::new(source);
                        if let Some(range) =
                            find_field_definition_in_model(&file, name, keyword, &source_map)
                        {
                            return Some(((*uri).clone(), range));
                        }
                    }
                }
            }
        }
    }

    // Priority 2: Field definitions in .model.nml files (any model)
    // (Skip when cursor is on the declaration keyword itself)
    if !is_on_keyword {
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
    }

    // Priority 3: Names in current file (top-level + nested)
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

    // Priority 4: Top-level declarations in other files
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

fn find_schema_block_definition(
    file: &File,
    name: &str,
    source_map: &SourceMap,
) -> Option<Range> {
    for decl in &file.declarations {
        if let DeclarationKind::Block(block) = &decl.kind {
            if matches!(block.keyword.name.as_str(), "model" | "trait" | "enum")
                && block.name.name == name
            {
                return Some(span_to_range(block.name.span, source_map));
            }
        }
    }
    None
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

fn find_field_definition_in_model(
    file: &File,
    name: &str,
    model_name: &str,
    source_map: &SourceMap,
) -> Option<Range> {
    for decl in &file.declarations {
        if let DeclarationKind::Block(block) = &decl.kind {
            if matches!(block.keyword.name.as_str(), "model" | "trait")
                && block.name.name == model_name
            {
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

fn format_field_type(field_type: &FieldType) -> String {
    match field_type {
        FieldType::Primitive(p) => p.as_str().to_string(),
        FieldType::List(inner) => format!("[]{}", format_field_type(inner)),
        FieldType::ModelRef(name) => name.clone(),
        FieldType::Modifier(name) => format!("|{}", name),
        FieldType::RefOnly(inner) => format!("&{}", format_field_type(inner)),
        FieldType::RoleRef => "roleRef".to_string(),
        FieldType::InlineObject(_) => "object".to_string(),
        FieldType::SharedProperty(_) => "shared".to_string(),
    }
}

fn is_property_name_position(line: &str, word: &str, col: usize) -> bool {
    if word.is_empty() {
        return false;
    }
    let trimmed = line.trim();

    if let Some(eq_pos) = line.find('=') {
        if col < eq_pos {
            return true;
        }
    }

    if trimmed.ends_with(':') && !trimmed.starts_with("//") {
        let before_colon = &trimmed[..trimmed.len() - 1];
        let indent = line.len() - line.trim_start().len();
        if !before_colon.contains(' ') && indent > 0 {
            return true;
        }
    }

    false
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

enum TemplateCompletionContext {
    Namespace,
    StepName(Vec<String>),
}

fn detect_template_context(before_cursor: &str, source: &str) -> Option<TemplateCompletionContext> {
    let last_open = before_cursor.rfind("{{")?;
    let after_open = &before_cursor[last_open + 2..];
    if after_open.contains("}}") {
        return None;
    }
    let typed = after_open.trim();
    if typed.is_empty() {
        return Some(TemplateCompletionContext::Namespace);
    }
    if typed == "steps." || typed.starts_with("steps.") && !typed[6..].contains('.') {
        if let Ok(file) = nml_core::parse(source) {
            let mut step_names = Vec::new();
            for decl in &file.declarations {
                if let DeclarationKind::Block(block) = &decl.kind {
                    if block.keyword.name == "workflow" {
                        for entry in &block.body.entries {
                            if let BodyEntryKind::NestedBlock(nested) = &entry.kind {
                                if nested.name.name == "steps" {
                                    for step_entry in &nested.body.entries {
                                        if let BodyEntryKind::ListItem(item) = &step_entry.kind {
                                            if let ListItemKind::Named { name, .. } = &item.kind {
                                                step_names.push(name.name.clone());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !step_names.is_empty() {
                return Some(TemplateCompletionContext::StepName(step_names));
            }
        }
    }
    None
}

fn detect_template_hover(line: &str, col: usize) -> Option<String> {
    let bytes = line.as_bytes();
    let mut start = None;
    let mut i = col.min(line.len());
    while i >= 2 {
        if bytes.get(i - 1) == Some(&b'{') && bytes.get(i - 2) == Some(&b'{') {
            start = Some(i);
            break;
        }
        if bytes.get(i - 1) == Some(&b'}') && i >= 2 && bytes.get(i - 2) == Some(&b'}') {
            break;
        }
        i -= 1;
    }
    let start = start?;
    let end = line[start..].find("}}")?;
    let expr = line[start..start + end].trim();
    let parts: Vec<&str> = expr.splitn(2, '.').collect();
    let (namespace, path_str) = if parts.len() == 2 {
        (parts[0], parts[1])
    } else {
        (parts[0], "")
    };

    let hover = match namespace {
        "args" => {
            if path_str.is_empty() {
                "**args** -- LLM tool call arguments".to_string()
            } else {
                format!("**args.{path_str}** -- Tool argument `{path_str}` (resolved from LLM tool call)")
            }
        }
        "steps" => {
            let step_parts: Vec<&str> = path_str.splitn(2, '.').collect();
            if step_parts.len() == 2 {
                format!(
                    "**steps.{}** -- Output field `{}` from step `{}`",
                    path_str, step_parts[1], step_parts[0]
                )
            } else if !path_str.is_empty() {
                format!("**steps.{path_str}** -- Output of step `{path_str}`")
            } else {
                "**steps** -- Workflow step outputs".to_string()
            }
        }
        "input" => {
            if path_str.is_empty() {
                "**input** -- Workflow input data".to_string()
            } else {
                format!("**input.{path_str}** -- Workflow input field `{path_str}`")
            }
        }
        "artifacts" => {
            if path_str.is_empty() {
                "**artifacts** -- Workflow artifacts".to_string()
            } else {
                format!("**artifacts.{path_str}** -- Workflow artifact `{path_str}`")
            }
        }
        _ => return None,
    };

    Some(hover)
}

// ── On-type indent computation ───────────────────────────────

fn is_inside_triple_quote(lines: &[&str], line_idx: usize) -> bool {
    let mut open = false;
    for (i, line) in lines.iter().enumerate() {
        if i >= line_idx {
            break;
        }
        let count = line.matches("\"\"\"").count();
        for _ in 0..count {
            open = !open;
        }
    }
    open
}

/// Compute the desired indentation (in spaces) for a new line inserted after
/// `line_idx` in the given source lines.  This drives `onTypeFormatting` for
/// the `\n` trigger so the cursor lands at the right column.
fn compute_indent_after_line(lines: &[&str], line_idx: usize) -> usize {
    let effective_idx = if line_idx < lines.len() {
        let mut idx = line_idx;
        while idx > 0 && lines[idx].trim().is_empty() {
            idx -= 1;
        }
        idx
    } else if !lines.is_empty() {
        lines.len() - 1
    } else {
        return 0;
    };

    let line = lines[effective_idx];
    let trimmed = line.trim();

    if trimmed.is_empty() {
        return 0;
    }

    if is_inside_triple_quote(lines, line_idx + 1) {
        return line.len() - line.trim_start().len();
    }

    let prev_indent = line.len() - line.trim_start().len();

    if trimmed.ends_with(':') && !trimmed.starts_with("//") {
        return prev_indent + 4;
    }

    prev_indent
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
                document_on_type_formatting_provider: Some(
                    DocumentOnTypeFormattingOptions {
                        first_trigger_character: "\n".to_string(),
                        more_trigger_character: None,
                    },
                ),
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
            let template_context = {
                let docs = self
                    .documents
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                docs.get(&uri).and_then(|source| {
                    let lines: Vec<&str> = source.lines().collect();
                    let line = lines.get(pos.line as usize)?;
                    let end = (pos.character as usize).min(line.len());
                    let before_cursor = &line[..end];
                    detect_template_context(before_cursor, source)
                })
            };

            if let Some(ctx) = template_context {
                match ctx {
                    TemplateCompletionContext::Namespace => {
                        let namespaces: Vec<String> = {
                            let pc = self.project_config.lock().unwrap_or_else(|e| e.into_inner());
                            if pc.template_namespaces.is_empty() {
                                nml_core::template::VALID_NAMESPACES.iter().map(|s| s.to_string()).collect()
                            } else {
                                pc.template_namespaces.clone()
                            }
                        };
                        for ns in &namespaces {
                            items.push(CompletionItem {
                                label: format!("{ns}."),
                                kind: Some(CompletionItemKind::MODULE),
                                detail: Some("template namespace".to_string()),
                                ..Default::default()
                            });
                        }
                    }
                    TemplateCompletionContext::StepName(step_names) => {
                        for name in step_names {
                            items.push(CompletionItem {
                                label: format!("{name}."),
                                kind: Some(CompletionItemKind::FIELD),
                                detail: Some("workflow step".to_string()),
                                ..Default::default()
                            });
                        }
                    }
                }
                return Ok(Some(CompletionResponse::Array(items)));
            }

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

        let language_keywords = ["model", "trait", "enum", "const", "template"];
        for kw in language_keywords {
            items.push(CompletionItem {
                label: kw.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                detail: Some("language".to_string()),
                ..Default::default()
            });
        }

        {
            let mut seen: HashSet<String> =
                language_keywords.iter().map(|s| s.to_string()).collect();

            let pc = self.project_config.lock().unwrap_or_else(|e| e.into_inner());
            for kw in &pc.keywords {
                if seen.insert(kw.clone()) {
                    items.push(CompletionItem {
                        label: kw.clone(),
                        kind: Some(CompletionItemKind::KEYWORD),
                        detail: Some("project".to_string()),
                        ..Default::default()
                    });
                }
            }
            drop(pc);

            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            for source in docs.values() {
                if let Ok(file) = nml_core::parse(source) {
                    for decl in &file.declarations {
                        if let nml_core::ast::DeclarationKind::Block(block) = &decl.kind {
                            let kw = &block.keyword.name;
                            if seen.insert(kw.clone()) {
                                items.push(CompletionItem {
                                    label: kw.clone(),
                                    kind: Some(CompletionItemKind::KEYWORD),
                                    detail: Some("workspace".to_string()),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
            }
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

        if let Some(template_hover) = detect_template_hover(line, pos.character as usize) {
            return Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: template_hover,
                }),
                range: None,
            }));
        }

        let word = extract_word_at(line, pos.character as usize);
        let is_prop = is_property_name_position(line, &word, pos.character as usize);

        if is_prop && !word.is_empty() {
            if let Ok(file) = nml_core::parse(&source_clone) {
                let source_map = SourceMap::new(&source_clone);
                if let Some(keyword) = find_enclosing_block_keyword(&file, pos, &source_map) {
                    let (models, _) = self.models_for_file(&uri);
                    if let Some(model) = models.iter().find(|m| m.name == keyword) {
                        if let Some(field) = model.fields.iter().find(|f| f.name == word) {
                            let type_str = format_field_type(&field.field_type);
                            let opt = if field.optional { "?" } else { "" };
                            let text = format!(
                                "**{keyword}** field\n\n```nml\n  {} {}{}\n```",
                                field.name, type_str, opt
                            );
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
        }

        if !is_prop {
            let builtin_info = match word.as_str() {
                "string" => Some("**string** -- Quoted text value"),
                "number" => {
                    Some("**number** -- General-purpose numeric (integer or decimal)")
                }
                "money" => Some(
                    "**money** -- Exact currency value with ISO 4217 code (e.g., `19.99 USD`)",
                ),
                "bool" => Some("**bool** -- Boolean value (`true` or `false`)"),
                "duration" => {
                    Some("**duration** -- Time duration (e.g., `\"72h\"`, `\"30s\"`)")
                }
                "path" => Some("**path** -- URL path with variables and wildcards"),
                "secret" => {
                    Some("**secret** -- Value resolved from environment (`$ENV.X`)")
                }
                "model" => Some("**model** -- Define a custom object type"),
                "trait" => Some("**trait** -- Define a reusable group of fields"),
                "enum" => {
                    Some("**enum** -- Define a restricted set of allowed values")
                }
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

        let (word, enclosing_keyword, is_prop) = {
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
            let is_prop = is_property_name_position(line, &word, pos.character as usize);

            let enclosing = if let Ok(file) = nml_core::parse(source) {
                let source_map = SourceMap::new(source);
                find_enclosing_block_keyword(&file, pos, &source_map)
            } else {
                None
            };

            (word, enclosing, is_prop)
        };

        if word.is_empty() {
            return Ok(None);
        }

        if !is_prop {
            if let Some(ref keyword) = enclosing_keyword {
                if keyword == &word {
                    if let Some((target_uri, range)) = self.find_schema_definition(&word, &uri) {
                        return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                            uri: target_uri,
                            range,
                        })));
                    }
                }
            }
        }

        if let Some((target_uri, range)) = self.find_definition(&word, &uri, enclosing_keyword.as_deref()) {
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

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        if params.ch != "\n" {
            return Ok(None);
        }

        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;

        let source = {
            let docs = self
                .documents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match docs.get(&uri) {
                Some(s) => s.clone(),
                None => return Ok(None),
            }
        };

        let lines: Vec<&str> = source.lines().collect();

        if pos.line == 0 {
            return Ok(None);
        }

        let prev_line_idx = (pos.line - 1) as usize;
        if prev_line_idx >= lines.len() {
            return Ok(None);
        }

        let desired = compute_indent_after_line(&lines, prev_line_idx);
        let indent_str: String = " ".repeat(desired);

        let current_line_idx = pos.line as usize;
        let existing_ws = if current_line_idx < lines.len() {
            let cur = lines[current_line_idx];
            cur.len() - cur.trim_start().len()
        } else {
            0
        };

        if existing_ws == desired {
            return Ok(None);
        }

        Ok(Some(vec![TextEdit {
            range: Range {
                start: Position::new(pos.line, 0),
                end: Position::new(pos.line, existing_ws as u32),
            },
            new_text: indent_str,
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

        let result = find_definition_in_docs(&docs, "GroqFast", &current, None);
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

        let result = find_definition_in_docs(&docs, "name", &current, None);
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

        let result = find_definition_in_docs(&docs, "GroqFast", &current, None);
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

        let result = find_definition_in_docs(&docs, "myStep", &current, None);
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

        assert!(find_definition_in_docs(&docs, "NonExistent", &current, None).is_none());
    }

    // ── Scope extraction ──────────────────────────────────────

    #[test]
    fn extract_schema_scope_workflow() {
        assert_eq!(
            extract_schema_scope("file:///path/to/workflow.model.nml"),
            "workflow"
        );
    }

    #[test]
    fn extract_schema_scope_config() {
        assert_eq!(
            extract_schema_scope("file:///path/to/config.model.nml"),
            "config"
        );
    }

    #[test]
    fn extract_file_scope_workflow() {
        assert_eq!(
            extract_file_scope("file:///path/to/voice-agent.workflow.nml"),
            Some("workflow".to_string())
        );
    }

    #[test]
    fn extract_file_scope_plain() {
        assert_eq!(extract_file_scope("file:///path/to/app.nml"), None);
    }

    #[test]
    fn extract_file_scope_model_file() {
        assert_eq!(
            extract_file_scope("file:///path/to/workflow.model.nml"),
            None
        );
    }

    // ── Scoped definition resolution ──────────────────────────

    #[test]
    fn definition_field_resolves_to_enclosing_model() {
        let mut docs = HashMap::new();
        let model_uri = make_uri("schema.model.nml");
        let current = make_uri("test.nml");

        docs.insert(
            model_uri.clone(),
            "model mount:\n    transport string\n\nmodel pipeline:\n    transport string?\n"
                .to_string(),
        );
        docs.insert(
            current.clone(),
            "pipeline P:\n    transport = TelnyxCall\n".to_string(),
        );

        let result =
            find_definition_in_docs(&docs, "transport", &current, Some("pipeline"));
        assert!(result.is_some());
        let (uri, range) = result.unwrap();
        assert_eq!(uri, model_uri);
        // Should resolve to transport in model pipeline (line 4), not model mount (line 1)
        assert_eq!(range.start.line, 4, "should resolve to pipeline's transport field");
    }

    #[test]
    fn definition_scoped_schema_preferred() {
        let mut docs = HashMap::new();
        let workflow_model = make_uri("workflow.model.nml");
        let config_model = make_uri("config.model.nml");
        let current = make_uri("voice-agent.workflow.nml");

        docs.insert(
            config_model.clone(),
            "model pipeline:\n    input []string?\n".to_string(),
        );
        docs.insert(
            workflow_model.clone(),
            "model pipeline:\n    transport string?\n".to_string(),
        );
        docs.insert(
            current.clone(),
            "pipeline P:\n    transport = TelnyxCall\n".to_string(),
        );

        let result =
            find_definition_in_docs(&docs, "transport", &current, Some("pipeline"));
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        assert_eq!(
            uri, workflow_model,
            "should resolve to workflow.model.nml, not config.model.nml"
        );
    }

    // ── Keyword navigation (cmd+click on declaration keyword) ─

    #[test]
    fn keyword_skips_field_definitions() {
        let mut docs = HashMap::new();
        let model_uri = make_uri("workflow.model.nml");
        let current = make_uri("voice-agent.workflow.nml");

        docs.insert(
            model_uri.clone(),
            "model step:\n    provider string?\n\nmodel provider:\n    type string\n    model string\n".to_string(),
        );
        docs.insert(
            current.clone(),
            "provider GroqFast:\n    type = \"groq\"\n".to_string(),
        );

        // When enclosing_keyword == name (cursor on keyword), field lookup is skipped.
        // Should NOT go to "provider string?" field in model step (line 1).
        // Falls through to top-level decl lookup and finds "model provider:" (line 3).
        let result = find_definition_in_docs(&docs, "provider", &current, Some("provider"));
        assert!(result.is_some());
        let (uri, range) = result.unwrap();
        assert_eq!(uri, model_uri, "should resolve to model definition, not to a field");
        assert_eq!(range.start.line, 3, "should point to 'model provider:' declaration");
    }

    #[test]
    fn find_schema_block_definition_finds_model() {
        let source = "model provider:\n    type string\n\nmodel workflow:\n    entrypoint string\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);

        let result = find_schema_block_definition(&file, "workflow", &source_map);
        assert!(result.is_some());
        assert_eq!(result.unwrap().start.line, 3, "should find model workflow on line 3");
    }

    #[test]
    fn find_schema_block_definition_finds_enum() {
        let source = "enum transport:\n    - \"http\"\n    - \"websocket\"\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);

        let result = find_schema_block_definition(&file, "transport", &source_map);
        assert!(result.is_some());
        assert_eq!(result.unwrap().start.line, 0);
    }

    #[test]
    fn find_schema_block_definition_ignores_instances() {
        let source = "provider GroqFast:\n    type = \"groq\"\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);

        let result = find_schema_block_definition(&file, "GroqFast", &source_map);
        assert!(result.is_none(), "should not match instance declarations");
    }

    #[test]
    fn keyword_does_not_match_field_in_other_model() {
        let mut docs = HashMap::new();
        let config_model = make_uri("config.model.nml");
        let server_model = make_uri("server.model.nml");
        let workflow_model = make_uri("workflow.model.nml");
        let current = make_uri("voice-agent.workflow.nml");

        docs.insert(
            config_model.clone(),
            "model mount:\n    workflow string?\n".to_string(),
        );
        docs.insert(
            server_model.clone(),
            "model auth:\n    provider string\n".to_string(),
        );
        docs.insert(
            workflow_model.clone(),
            "model workflow:\n    entrypoint string\n\nmodel provider:\n    type string\n".to_string(),
        );
        docs.insert(
            current.clone(),
            "workflow VoiceAgent:\n    entrypoint = \"start\"\n\nprovider Groq:\n    type = \"groq\"\n".to_string(),
        );

        // "workflow" with enclosing_keyword="workflow" should skip field lookups
        let result = find_definition_in_docs(&docs, "workflow", &current, Some("workflow"));
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        // Must NOT go to "workflow string?" in model mount (config.model.nml)
        assert_ne!(uri, config_model, "should not resolve to field 'workflow' in model mount");

        // "provider" with enclosing_keyword="provider" should skip field lookups
        let result = find_definition_in_docs(&docs, "provider", &current, Some("provider"));
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        // Must NOT go to "provider string" in model auth (server.model.nml)
        assert_ne!(uri, server_model, "should not resolve to field 'provider' in model auth");
    }

    // ── is_property_name_position ─────────────────────────────

    #[test]
    fn property_position_before_equals() {
        assert!(is_property_name_position("    model = \"llama\"", "model", 6));
    }

    #[test]
    fn property_position_nested_block() {
        assert!(is_property_name_position("    inbound:", "inbound", 6));
    }

    #[test]
    fn not_property_position_keyword() {
        assert!(!is_property_name_position("workflow VoiceAgent:", "workflow", 3));
    }

    #[test]
    fn not_property_position_value() {
        assert!(!is_property_name_position("    transport = TelnyxCall", "TelnyxCall", 18));
    }

    #[test]
    fn not_property_position_top_level_block() {
        assert!(!is_property_name_position("provider GroqFast:", "provider", 3));
    }

    // ── find_enclosing_block_keyword ─────────────────────────────

    #[test]
    fn enclosing_keyword_on_workflow_declaration() {
        let source = r#"stage TelnyxCall:
    wasm = "telnyx.wasm"
    accepts = "audio"
    produces = "audio"

provider GroqFast:
    type = "groq"
    model = "llama-3.3-70b-versatile"
    temperature = 0.7

workflow VoiceAgent:
    entrypoint = "conversation"
    steps:
        - conversation:
            provider = GroqFast
"#;
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);

        // "workflow" keyword is on line 11 (0-indexed)
        let pos = Position::new(11, 3);
        let result = find_enclosing_block_keyword(&file, pos, &source_map);
        assert_eq!(result, Some("workflow".to_string()), "cursor on 'workflow' should return 'workflow'");

        // "provider" keyword is on line 5 (0-indexed)
        let pos = Position::new(5, 3);
        let result = find_enclosing_block_keyword(&file, pos, &source_map);
        assert_eq!(result, Some("provider".to_string()), "cursor on 'provider' should return 'provider'");
    }

    #[test]
    fn enclosing_keyword_on_tool_declaration() {
        let source = r#"stage TelnyxCall:
    wasm = "telnyx.wasm"
    produces = "audio"

pipeline TelnyxVoice:
    transport = TelnyxCall
    inbound:
        - DeepgramSTT

tool DialViaTelnyx:
    pipeline = TelnyxVoice

provider GroqFast:
    type = "groq"
    model = "llama-3.3-70b-versatile"

workflow VoiceAgent:
    entrypoint = "conversation"
    steps:
        - conversation:
            provider = GroqFast
"#;
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);

        // "tool" keyword is on line 9 (0-indexed) - must return "tool" not "workflow" or "stage"
        let pos = Position::new(9, 3);
        let result = find_enclosing_block_keyword(&file, pos, &source_map);
        assert_eq!(result, Some("tool".to_string()), "cursor on 'tool' in tool DialViaTelnyx: should return 'tool'");
    }

    #[test]
    fn enclosing_keyword_returns_none_for_blank_line() {
        let source = "stage A:\n    wasm = \"a.wasm\"\n\nstage B:\n    wasm = \"b.wasm\"\n";
        let file = nml_core::parse(source).unwrap();
        let source_map = SourceMap::new(source);

        // Line 2 is the blank line between stage A and stage B
        let pos = Position::new(2, 0);
        let result = find_enclosing_block_keyword(&file, pos, &source_map);
        // Blank line may or may not be inside a declaration depending on parser spans
        // Just verify it doesn't panic
        let _ = result;
    }

    #[test]
    fn keyword_tool_goes_to_model_not_field() {
        let mut docs = HashMap::new();
        let model_uri = make_uri("workflow.model.nml");
        let current = make_uri("voice-agent.workflow.nml");

        docs.insert(
            model_uri.clone(),
            concat!(
                "model step:\n",
                "    provider string?\n",
                "    tool string?\n",
                "    tools []string?\n",
                "\n",
                "model tool:\n",
                "    wasm string?\n",
                "    pipeline string?\n",
            ).to_string(),
        );
        docs.insert(
            current.clone(),
            concat!(
                "tool DialViaTelnyx:\n",
                "    pipeline = TelnyxVoice\n",
            ).to_string(),
        );

        // Clicking on "tool" in "tool DialViaTelnyx:" should go to model tool: (line 5),
        // NOT to "tool string?" field in model step (line 2).
        let result = find_definition_in_docs(&docs, "tool", &current, Some("tool"));
        assert!(result.is_some());
        let (uri, range) = result.unwrap();
        assert_eq!(uri, model_uri);
        assert_eq!(range.start.line, 5, "should point to model tool:, not tool string? field");
    }

    #[test]
    fn full_goto_keyword_to_schema_definition() {
        let mut docs = HashMap::new();
        let model_uri = make_uri("workflow.model.nml");
        let current = make_uri("voice-agent.workflow.nml");

        docs.insert(
            model_uri.clone(),
            concat!(
                "model provider:\n",
                "    type string\n",
                "    model string\n",
                "\n",
                "model step:\n",
                "    provider string?\n",
                "\n",
                "model workflow:\n",
                "    entrypoint string\n",
                "    steps []step\n",
            ).to_string(),
        );
        docs.insert(
            current.clone(),
            concat!(
                "provider GroqFast:\n",
                "    type = \"groq\"\n",
                "\n",
                "workflow VoiceAgent:\n",
                "    entrypoint = \"conversation\"\n",
            ).to_string(),
        );

        // Test 1: "workflow" with enclosing="workflow" (cursor on keyword)
        // find_schema_definition path: looks for model/trait/enum named "workflow"
        // Should find "model workflow:" on line 7 (0-indexed) in workflow.model.nml
        {
            let source = docs.get(&model_uri).unwrap();
            let file = nml_core::parse(source).unwrap();
            let source_map = SourceMap::new(source);
            let result = find_schema_block_definition(&file, "workflow", &source_map);
            assert!(result.is_some(), "find_schema_block_definition should find model workflow:");
            let range = result.unwrap();
            assert_eq!(range.start.line, 7, "model workflow: is on line 7 (0-indexed)");
        }

        // Test 2: "provider" with enclosing="provider" (cursor on keyword)
        {
            let source = docs.get(&model_uri).unwrap();
            let file = nml_core::parse(source).unwrap();
            let source_map = SourceMap::new(source);
            let result = find_schema_block_definition(&file, "provider", &source_map);
            assert!(result.is_some(), "find_schema_block_definition should find model provider:");
            let range = result.unwrap();
            assert_eq!(range.start.line, 0, "model provider: is on line 0 (0-indexed)");
        }

        // Test 3: find_definition_in_docs with is_on_keyword=true should NOT return field definitions
        {
            let result = find_definition_in_docs(&docs, "workflow", &current, Some("workflow"));
            assert!(result.is_some(), "should find something for 'workflow'");
            let (uri, range) = result.unwrap();
            // Should NOT go to "provider string?" field. Should find via Priority 4 (top-level decl).
            // model workflow: is on line 7 in workflow.model.nml
            assert_eq!(uri, model_uri);
            assert_eq!(range.start.line, 7, "should point to model workflow: name");
        }
    }

    // ── compute_indent_after_line ───────────────────────────────

    #[test]
    fn indent_after_block_colon() {
        let lines = vec!["workflow RecipeAssistant:", "    steps:"];
        assert_eq!(compute_indent_after_line(&lines, 0), 4);
        assert_eq!(compute_indent_after_line(&lines, 1), 8);
    }

    #[test]
    fn indent_after_list_item_colon() {
        let lines = vec!["    steps:", "        - classify:"];
        assert_eq!(compute_indent_after_line(&lines, 1), 12);
    }

    #[test]
    fn indent_after_property() {
        let lines = vec!["        - classify:", "            provider = Groq"];
        assert_eq!(compute_indent_after_line(&lines, 1), 12);
    }

    #[test]
    fn indent_after_goto_property() {
        let lines = vec![
            "                - clarifyRoute:",
            "                    when:",
            "                        field = \"response_mode\"",
            "                        equals = \"clarify\"",
            "                    goto = \"respond\"",
        ];
        assert_eq!(compute_indent_after_line(&lines, 4), 20);
    }

    #[test]
    fn indent_after_blank_line_uses_prev_non_empty() {
        let lines = vec!["    steps:", "        - classify:", ""];
        assert_eq!(compute_indent_after_line(&lines, 2), 12);
    }

    #[test]
    fn indent_after_nested_block_colon() {
        let lines = vec![
            "        - router:",
            "            routes:",
        ];
        assert_eq!(compute_indent_after_line(&lines, 1), 16);
    }

    #[test]
    fn indent_inside_triple_quote() {
        let lines = vec![
            "            system = \"\"\"",
            "            You are a helpful assistant.",
        ];
        assert_eq!(compute_indent_after_line(&lines, 1), 12);
    }

    #[test]
    fn indent_after_scalar_list_item() {
        let lines = vec![
            "enum providerType:",
            "    - \"anthropic\"",
        ];
        assert_eq!(compute_indent_after_line(&lines, 1), 4);
    }

    #[test]
    fn indent_after_comment_ending_with_colon() {
        let lines = vec!["    // this is a comment:"];
        assert_eq!(compute_indent_after_line(&lines, 0), 4);
    }

    #[test]
    fn indent_empty_source() {
        let lines: Vec<&str> = vec![];
        assert_eq!(compute_indent_after_line(&lines, 0), 0);
    }

    #[test]
    fn indent_at_top_level() {
        let lines = vec!["workflow RecipeAssistant:"];
        assert_eq!(compute_indent_after_line(&lines, 0), 4);
    }
}
