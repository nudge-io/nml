use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(nml_lsp::server::NmlLanguageServer::new)
        // RFC 0030 introspection: which schema package validates a document,
        // from where, at which hash — callable by any LSP client.
        .custom_method(
            "nml/schemaInfo",
            nml_lsp::server::NmlLanguageServer::schema_info,
        )
        .finish();
    Server::new(stdin, stdout, socket).serve(service).await;
}
