// No `unsafe` in this crate (RFC 0019 item 0, E35): enforced at the root.
#![forbid(unsafe_code)]

// Native uses the default multi-thread runtime; `wasm32` (the neutral server
// under VS Code's `wasm-wasi-core`) must use the current-thread runtime —
// `rt-multi-thread` needs OS threads, which WASI preview 1 does not provide.
#[cfg_attr(not(target_arch = "wasm32"), tokio::main)]
#[cfg_attr(target_arch = "wasm32", tokio::main(flavor = "current_thread"))]
async fn main() -> std::process::ExitCode {
    // The neutral nml language server (RFC 0035): serves the in-repo and
    // in-cache channels for any nml project. Providers embed their own
    // package via `nml_lsp::serve` from their `<tool> lsp` subcommand instead.
    // The code is the protocol's (LSP 3.17 §exit): 0 after `shutdown`, 1
    // without it.
    nml_lsp::serve_stdio().await.exit_code()
}
