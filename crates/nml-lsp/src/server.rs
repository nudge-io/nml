use std::collections::HashMap;
use std::sync::Mutex;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::diagnostics;

pub struct NmlLanguageServer {
    client: Client,
    documents: Mutex<HashMap<Url, String>>,
}

impl NmlLanguageServer {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Mutex::new(HashMap::new()),
        }
    }

    async fn on_change(&self, uri: Url, text: String) {
        let diags = diagnostics::compute(&text);
        self.documents.lock().unwrap().insert(uri.clone(), text);
        self.client
            .publish_diagnostics(uri, diags, None)
            .await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for NmlLanguageServer {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
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
        self.documents
            .lock()
            .unwrap()
            .remove(&params.text_document.uri);
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
