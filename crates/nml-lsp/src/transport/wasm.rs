use tower_lsp::Client;

use super::pump;
use crate::SessionEnd;
use crate::server::NmlLanguageServer;
use crate::session::build_service;

/// wasm32: drive the service with a synchronous pump (see [`pump`]) over
/// the host's stdio, and say how the session ended.
pub(super) async fn serve_with(init: impl FnOnce(Client) -> NmlLanguageServer) -> SessionEnd {
    let (mut service, mut socket) = build_service(init);
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let mut writer = std::io::stdout().lock();
    pump::run(&mut service, &mut socket, &mut reader, &mut writer).await
}
