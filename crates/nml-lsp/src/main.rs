// Native uses the default multi-thread runtime; `wasm32` (the neutral server
// under VS Code's `wasm-wasi-core`) must use the current-thread runtime —
// `rt-multi-thread` needs OS threads, which WASI preview 1 does not provide.
#[cfg_attr(not(target_arch = "wasm32"), tokio::main)]
#[cfg_attr(target_arch = "wasm32", tokio::main(flavor = "current_thread"))]
async fn main() {
    // The neutral nml language server (RFC 0035): serves the in-repo and
    // in-cache channels for any nml project. Providers embed their own
    // package via `nml_lsp::serve` from their `<tool> lsp` subcommand instead.
    nml_lsp::serve_stdio(nml_lsp::server::NmlLanguageServer::new).await;
}
