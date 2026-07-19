pub mod diagnostics;
pub mod packages;
pub mod position;
pub mod server;

use tower_lsp::{Client, ClientSocket, LspService, Server};

use server::NmlLanguageServer;

/// The stdio transport for the LSP wire — the one target-split seam (RFC 0035
/// WASM delivery). Native uses tokio's async stdio; `wasm32` (the neutral server
/// under VS Code's `wasm-wasi-core`) has no `io-std` feature, so it reads/writes
/// WASI fd 0/1 directly. Everything above this is target-agnostic.
#[cfg(not(target_arch = "wasm32"))]
mod transport {
    pub fn stdio() -> (tokio::io::Stdin, tokio::io::Stdout) {
        (tokio::io::stdin(), tokio::io::stdout())
    }
}

#[cfg(target_arch = "wasm32")]
mod transport {
    use std::io::{Read, Write};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    /// `AsyncRead`/`AsyncWrite` over WASI stdio. The reads are synchronous by
    /// design: under `wasm-wasi-core` the host blocks the (dedicated) worker
    /// until fd 0 has data, so `poll_read` never needs `Pending` — and the
    /// current-thread runtime has no other ready work while idle (on WASI
    /// `Store::user()` is `None`, so the freshness poll watches nothing). If
    /// concurrent-while-blocked ever matters here, this is where a
    /// `poll_oneoff`-driven non-blocking adapter would go.
    pub struct WasiStdin;
    impl AsyncRead for WasiStdin {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let unfilled = buf.initialize_unfilled();
            let n = std::io::stdin().lock().read(unfilled)?;
            buf.advance(n);
            Poll::Ready(Ok(()))
        }
    }

    pub struct WasiStdout;
    impl AsyncWrite for WasiStdout {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(std::io::stdout().lock().write(buf))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(std::io::stdout().lock().flush())
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    pub fn stdio() -> (WasiStdin, WasiStdout) {
        (WasiStdin, WasiStdout)
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
pub async fn serve_stdio(init: impl FnOnce(Client) -> NmlLanguageServer) {
    let (service, socket) = build_service(init);
    let (read, write) = transport::stdio();
    Server::new(read, write, socket).serve(service).await;
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
