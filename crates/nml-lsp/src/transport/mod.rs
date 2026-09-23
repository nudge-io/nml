//! On `wasm32` the neutral server runs under VS Code's `wasm-wasi-core`, whose
//! stdio model is *synchronous*: the host blocks the (dedicated) worker on a
//! read until a message arrives, so the server must be a plain
//! read→process→write pump — each response is written *before* the next blocking
//! read. tower-lsp's native `Server::serve` reads input and writes output
//! concurrently, which that model deadlocks (a synchronous read starves the loop
//! that flushes responses); so on wasm `serve_stdio` drives the `LspService`
//! directly here instead. Server→client *requests* are the one thing a
//! synchronous pump cannot await — EVERY one of them, not just the dynamic
//! capability registration — so they all go through [`crate::ask`], which shuts
//! them off on wasm; server→client *notifications* (`publishDiagnostics`,
//! `logMessage`) are queued by the client handle without waiting for anyone
//! and drained after each call.
//!
//! Two entry points, one result: [`serve_stdio`] is the neutral server,
//! [`serve`] the neutral server plus a provider tool's embedded package;
//! both drive the session the crate builds ([`crate::session::build_service`]) and
//! return how it ended. Each target's driver is `serve_with` — native
//! (`native.rs`, tower-lsp's concurrent server) or wasm32 (`wasm.rs`, the
//! synchronous pump).

use crate::SessionEnd;
use crate::server::NmlLanguageServer;

/// `Content-Length` framing for the synchronous wasm pump — compiled on
/// the native test lane too, so the frame bound is pinned where a
/// test can run instead of living only in a module no test lane builds.
#[cfg(any(target_arch = "wasm32", test))]
mod framing;
#[cfg(not(target_arch = "wasm32"))]
mod native;
/// The synchronous read→process→write pump the wasm32 neutral server runs
/// on — compiled on the native test lane too, so the transport the
/// in-editor server lives or dies by is exercised where a test can run
/// (it used to be reachable only from a `wasm32` build, and nothing in
/// the suite touched it).
#[cfg(any(target_arch = "wasm32", test))]
mod pump;
#[cfg(target_arch = "wasm32")]
mod wasm;

#[cfg(not(target_arch = "wasm32"))]
use native::serve_with;
#[cfg(target_arch = "wasm32")]
use wasm::serve_with;

/// Serve the neutral nml language server over stdio until the session
/// ends, and say how it ended ([`SessionEnd`]): map that onto the
/// process's exit code — [`SessionEnd::exit_code`] is the protocol's
/// mapping — and return it from `main`. The neutral server serves the
/// in-repo and in-cache schema channels for any nml project; a provider
/// tool embeds its own package with [`serve`] instead. Async so an
/// embedder with its own runtime can `.await` it directly.
pub async fn serve_stdio() -> SessionEnd {
    serve_with(NmlLanguageServer::new).await
}

/// Serve as a schema provider (RFC 0035 in-binary channel): the neutral server
/// plus this tool's embedded `package` injected at in-binary precedence, over
/// stdio. This is the whole body of a provider tool's `<tool> lsp` subcommand —
/// `nml_lsp::serve(MY_PACKAGE.clone()).await`.
pub async fn serve(package: nml_validate::package::SchemaPackage) -> SessionEnd {
    serve_with(|client| {
        NmlLanguageServer::with_provider(client, package, nml_validate::store::Store::user())
    })
    .await
}
