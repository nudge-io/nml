pub mod diagnostics;
pub mod packages;
pub mod position;
pub mod server;

#[cfg(not(target_arch = "wasm32"))]
use tower_lsp::Server;
use tower_lsp::{Client, ClientSocket, LspService};

use server::NmlLanguageServer;

/// On `wasm32` the neutral server runs under VS Code's `wasm-wasi-core`, whose
/// stdio model is *synchronous*: the host blocks the (dedicated) worker on a
/// read until a message arrives, so the server must be a plain
/// read→process→write pump — each response is written *before* the next blocking
/// read. tower-lsp's native `Server::serve` reads input and writes output
/// concurrently, which that model deadlocks (a synchronous read starves the loop
/// that flushes responses); so on wasm `serve_stdio` drives the `LspService`
/// directly here instead. Server→client *requests* (dynamic capability
/// registration) are the one thing a synchronous pump cannot await, so they are
/// cfg-gated off on wasm; server→client *notifications* (`publishDiagnostics`,
/// `logMessage`) are drained after each call.
#[cfg(target_arch = "wasm32")]
mod wasm_pump {
    use std::io::{BufRead, Write};

    use futures::{FutureExt, StreamExt};
    use serde::Serialize;
    use tower::{Service, ServiceExt};
    use tower_lsp::jsonrpc::Request;
    use tower_lsp::{ClientSocket, LspService};

    use crate::server::NmlLanguageServer;

    /// Read one `Content-Length`-framed JSON-RPC message. Blocking (the host
    /// services the wait); `None` on EOF or a malformed frame.
    fn read_frame(reader: &mut impl BufRead) -> Option<Request> {
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                return None; // EOF
            }
            let line = line.trim_end();
            if line.is_empty() {
                break; // end of headers
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                content_length = v.trim().parse().ok()?;
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).ok()?;
        serde_json::from_slice(&body).ok()
    }

    fn write_frame(writer: &mut impl Write, msg: &impl Serialize) {
        if let Ok(body) = serde_json::to_vec(msg) {
            let _ = write!(writer, "Content-Length: {}\r\n\r\n", body.len());
            let _ = writer.write_all(&body);
            let _ = writer.flush();
        }
    }

    pub async fn run(mut service: LspService<NmlLanguageServer>, mut socket: ClientSocket) {
        let stdin = std::io::stdin();
        let mut reader = std::io::BufReader::new(stdin.lock());
        let mut writer = std::io::stdout().lock();
        while let Some(req) = read_frame(&mut reader) {
            // Drive the request/notification to completion, then write its
            // response (requests only) BEFORE reading stdin again.
            let Ok(svc) = service.ready().await else {
                break;
            };
            if let Ok(Some(response)) = svc.call(req).await {
                write_frame(&mut writer, &response);
            }
            // Flush server→client notifications queued during the call.
            while let Some(Some(out)) = socket.next().now_or_never() {
                write_frame(&mut writer, &out);
            }
        }
    }
}

/// Build the [`LspService`] with every nml custom method registered. The
/// single owner of custom-method wiring: the `nml-lsp` binary, the test
/// harness, and every schema provider (`nudge lsp`, RFC 0035) construct their
/// service here, so no call site can drift by forgetting a method. Adding a
/// method (e.g. `nml/status`) here reaches all of them at once.
pub fn build_service(
    init: impl FnOnce(Client) -> NmlLanguageServer,
) -> (LspService<NmlLanguageServer>, ClientSocket) {
    LspService::build(init)
        // RFC 0030 introspection: which schema package validates a document,
        // from where, at which hash — callable by any LSP client.
        .custom_method("nml/schemaInfo", NmlLanguageServer::schema_info)
        .finish()
}

/// Serve a language server over stdio until the client disconnects. `init`
/// chooses the flavor (`NmlLanguageServer::new` for the neutral server,
/// [`serve`]'s provider wiring for a tool). Async so an embedder with its own
/// runtime (a provider tool) can `.await` it directly.
#[cfg(not(target_arch = "wasm32"))]
pub async fn serve_stdio(init: impl FnOnce(Client) -> NmlLanguageServer) {
    let (service, socket) = build_service(init);
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}

/// wasm32: drive the service with a synchronous pump (see [`wasm_pump`]).
#[cfg(target_arch = "wasm32")]
pub async fn serve_stdio(init: impl FnOnce(Client) -> NmlLanguageServer) {
    let (service, socket) = build_service(init);
    wasm_pump::run(service, socket).await;
}

/// Serve as a schema provider (RFC 0035 in-binary channel): the neutral server
/// plus this tool's embedded `package` injected at in-binary precedence, over
/// stdio. This is the whole body of a provider tool's `<tool> lsp` subcommand —
/// `nml_lsp::serve(MY_PACKAGE.clone()).await`.
pub async fn serve(package: nml_validate::package::SchemaPackage) {
    serve_stdio(|client| {
        NmlLanguageServer::with_provider(client, package, nml_validate::store::Store::user())
    })
    .await;
}
