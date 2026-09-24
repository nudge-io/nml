use std::io::{BufRead, Write};

use futures::future::{self, Either};
use futures::{FutureExt, StreamExt};
use tower::{Service, ServiceExt};
use tower_lsp::ClientSocket;
use tower_lsp::jsonrpc::{Error, Id, Response};

use super::framing::{Frame, read_frame, write_frame};
use crate::session::NmlService;

/// Drive `service` from `reader` to `writer` until the session ends.
///
/// Strictly serial in its READS by necessity: under `wasm-wasi-core`
/// the host blocks the worker inside the read, so a message is fully
/// answered — its response and every notification it queued written —
/// before the next read starts. Nothing may be awaited across that
/// boundary, which is why a server→client REQUEST, whose answer can
/// only arrive by reading, never leaves this process
/// (see [`crate::ask`]).
///
/// Writes are NOT serial with the call, and must not be: tower-lsp's
/// client handle is a one-slot channel, so a handler's second
/// notification parks its own send until someone takes the first.
/// Draining only after the call returned therefore deadlocked the
/// handler — `initialized` sends two the moment the editor has a
/// workspace folder, which is every real session — and with it the
/// pump, the reader and the process.
pub(super) async fn run(
    service: &mut NmlService,
    socket: &mut ClientSocket,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> crate::SessionEnd {
    while let Some(frame) = read_frame(reader) {
        let request = match frame {
            Frame::Message(request) => request,
            // One unparsable body is answered, not fatal — the answer
            // the native transport gives (parse error, null id).
            Frame::Unparsable => {
                let parse_error = Response::from_error(Id::Null, Error::parse_error());
                if let Ok(body) = serde_json::to_vec(&parse_error) {
                    write_frame(writer, &body);
                }
                continue;
            }
        };
        let Ok(svc) = service.ready().await else {
            break;
        };
        // The call and the client's outbound queue, driven together.
        let mut call = svc.call(*request);
        let answer = loop {
            match future::select(call, socket.next()).await {
                Either::Left((answer, _)) => break answer,
                // Written while the handler still runs — that is the
                // whole point (see above).
                Either::Right((Some(queued), rest)) => {
                    if let Ok(body) = serde_json::to_vec(&queued) {
                        write_frame(writer, &body);
                    }
                    call = rest;
                }
                // The handle is closed (the `exit` handler closes it):
                // nothing more can be queued, so just finish the call.
                Either::Right((None, rest)) => break rest.await,
            }
        };
        if let Ok(Some(response)) = answer {
            if let Ok(body) = serde_json::to_vec(&response) {
                write_frame(writer, &body);
            }
        }
        // Anything queued between the last poll and the answer.
        while let Some(Some(out)) = socket.next().now_or_never() {
            if let Ok(body) = serde_json::to_vec(&out) {
                write_frame(writer, &body);
            }
        }
        // LSP 3.17 §exit: the server exits on this notification, with
        // the ending the service decided ([`crate::session::ExitSignal`]). The
        // pump must therefore not go back to the read — a host that
        // keeps stdin open would park the process there forever, and
        // it would outlive the editor session that asked it to stop.
        if let Some(ending) = service.exit_signal().ending() {
            return ending;
        }
    }
    // The host closed stdin without `exit`: not a protocol ending.
    crate::SessionEnd::Disconnected
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, Read};
    use std::time::Duration;

    use super::super::framing::write_frame;
    use crate::server::NmlLanguageServer;

    /// A reader that PANICS the instant it is read past `trap` — the
    /// byte after the last frame the session is allowed to consume.
    /// Under `wasm-wasi-core` that read does not fail, it BLOCKS: the
    /// worker parks in the host forever and the server outlives the
    /// editor session. A cursor would quietly report EOF and hide
    /// exactly the bug, so the trap makes the read itself the
    /// assertion.
    struct ReadsUpTo {
        data: std::io::Cursor<Vec<u8>>,
        trap: u64,
    }

    impl ReadsUpTo {
        fn check(&self) {
            assert!(
                self.data.position() < self.trap,
                "the pump read past the frame it was told was the last one \
                 (byte {}): under `wasm-wasi-core` that read blocks forever",
                self.trap
            );
        }
    }

    impl Read for ReadsUpTo {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.check();
            self.data.read(buf)
        }
    }

    impl BufRead for ReadsUpTo {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.check();
            self.data.fill_buf()
        }

        fn consume(&mut self, amount: usize) {
            self.data.consume(amount);
        }
    }

    /// The frames of a session, and the byte offset just past the
    /// `until`-th of them.
    fn session(messages: &[serde_json::Value], until: usize) -> ReadsUpTo {
        let mut data = Vec::new();
        let mut trap = 0;
        for (i, message) in messages.iter().enumerate() {
            write_frame(&mut data, &serde_json::to_vec(message).expect("serializes"));
            if i + 1 == until {
                trap = data.len() as u64;
            }
        }
        ReadsUpTo {
            data: std::io::Cursor::new(data),
            trap,
        }
    }

    /// Every framed body written to `out`, in order — read back
    /// through the `Content-Length` headers the pump wrote, so a test
    /// asserts on what the wire actually carried.
    fn bodies(out: &[u8]) -> Vec<serde_json::Value> {
        let mut reader = std::io::Cursor::new(out.to_vec());
        let mut written = Vec::new();
        while let Some(body) = next_body(&mut reader) {
            written.push(body);
        }
        written
    }

    /// One `Content-Length`-framed JSON body off `reader`.
    fn next_body(reader: &mut impl std::io::BufRead) -> Option<serde_json::Value> {
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                length = v.trim().parse().ok()?;
            }
        }
        let mut body = vec![0u8; length];
        std::io::Read::read_exact(reader, &mut body).ok()?;
        serde_json::from_slice(&body).ok()
    }

    /// An EMPTY workspace root, removed when the guard drops. Empty
    /// because `initialized` indexes the root it is given, and the
    /// process temp dir is neither empty nor small.
    struct Root(std::path::PathBuf);

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn empty_root(tag: &str) -> Root {
        let dir = std::env::temp_dir().join(format!("nml-lsp-pump-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Root(dunce::canonicalize(&dir).expect("canonicalize"))
    }

    /// Run the pump to completion, turning a DEADLOCK into a named
    /// failure instead of a hung suite. The bound is never reached by
    /// a working pump: every session here ends in `exit`.
    async fn pumped(
        service: &mut crate::session::NmlService,
        socket: &mut tower_lsp::ClientSocket,
        reader: &mut impl BufRead,
        writer: &mut Vec<u8>,
    ) -> crate::SessionEnd {
        tokio::time::timeout(
            Duration::from_secs(20),
            super::run(service, socket, reader, writer),
        )
        .await
        .expect("the pump never finished the session — it is deadlocked")
    }

    fn handshake(root: &std::path::Path) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "capabilities": {},
                "rootUri": tower_lsp::lsp_types::Url::from_file_path(root)
                    .expect("absolute").to_string(),
            },
        })
    }

    /// `exit` ends the pump — it does not read again. LSP 3.17 says
    /// the server exits on that notification; a pump that loops back
    /// to the read waits on a host that may never close stdin, and the
    /// process outlives the session (the trap reader is that read).
    /// The frames after `exit` are the proof: none of them is served.
    #[tokio::test]
    async fn exit_ends_the_pump_before_it_reads_again() {
        let root = empty_root("exit");
        let mut reader = session(
            &[
                handshake(&root.0),
                serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
                serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown"}),
                serde_json::json!({"jsonrpc": "2.0", "method": "exit"}),
                // Never to be served: the session is over.
                serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "shutdown"}),
            ],
            4,
        );
        let (mut service, mut socket) =
            crate::session::build_service(|client| NmlLanguageServer::with_store(client, None));
        let mut out = Vec::new();
        let code = pumped(&mut service, &mut socket, &mut reader, &mut out).await;
        let ids: Vec<_> = bodies(&out)
            .iter()
            .filter_map(|b| b.get("id").and_then(serde_json::Value::as_i64))
            .collect();
        assert_eq!(
            ids,
            [1, 2],
            "one answer each, and nothing after exit: {ids:?}"
        );
        assert_eq!(code, crate::SessionEnd::Exited, "`shutdown` came first");
        assert_eq!(code.exit_code(), std::process::ExitCode::SUCCESS);
    }

    /// LSP 3.17 §exit: `exit` with no `shutdown` before it ends the
    /// session with error code 1 — the code is the protocol's, decided
    /// by the service and returned by the pump for `main` to carry.
    #[tokio::test]
    async fn exit_without_shutdown_ends_the_session_with_failure() {
        let root = empty_root("exit-no-shutdown");
        let mut reader = session(
            &[
                handshake(&root.0),
                serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
                serde_json::json!({"jsonrpc": "2.0", "method": "exit"}),
            ],
            3,
        );
        let (mut service, mut socket) =
            crate::session::build_service(|client| NmlLanguageServer::with_store(client, None));
        let mut out = Vec::new();
        let code = pumped(&mut service, &mut socket, &mut reader, &mut out).await;
        assert_eq!(code, crate::SessionEnd::ExitedWithoutShutdown);
        assert_eq!(code.exit_code(), std::process::ExitCode::FAILURE);
    }

    /// The host closing stdin without `exit` is not a protocol ending:
    /// the server's own outcome is clean, and an editor that ends a
    /// server it could never talk to must not read that as its fault.
    #[tokio::test]
    async fn stdin_eof_without_exit_ends_the_session_with_success() {
        let root = empty_root("eof");
        // A plain cursor, not the trap reader: reading to EOF is the
        // case under test here, and the trap exists to catch exactly
        // that read in the OTHER tests.
        let mut reader = session(
            &[
                handshake(&root.0),
                serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
            ],
            2,
        )
        .data;
        let (mut service, mut socket) =
            crate::session::build_service(|client| NmlLanguageServer::with_store(client, None));
        let mut out = Vec::new();
        let code = pumped(&mut service, &mut socket, &mut reader, &mut out).await;
        assert_eq!(code, crate::SessionEnd::Disconnected);
        assert_eq!(code.exit_code(), std::process::ExitCode::SUCCESS);
    }

    /// One unparsable body is answered with a parse error and the
    /// session continues — a buggy client must not be able to end the
    /// server with a single bad frame. The native transport answers
    /// the same way and does NOT read on (measured; see
    /// `framing::Frame::Unparsable`), so this is the pump's own
    /// property, not a shared one.
    #[tokio::test]
    async fn an_unparsable_body_is_answered_and_the_session_continues() {
        let root = empty_root("unparsable");
        let mut data = Vec::new();
        write_frame(&mut data, b"{\"jsonrpc\": broken");
        write_frame(
            &mut data,
            &serde_json::to_vec(&handshake(&root.0)).expect("serializes"),
        );
        write_frame(
            &mut data,
            &serde_json::to_vec(&serde_json::json!({"jsonrpc": "2.0", "method": "exit"}))
                .expect("serializes"),
        );
        let trap = data.len() as u64;
        let mut reader = ReadsUpTo {
            data: std::io::Cursor::new(data),
            trap,
        };
        let (mut service, mut socket) =
            crate::session::build_service(|client| NmlLanguageServer::with_store(client, None));
        let mut out = Vec::new();
        pumped(&mut service, &mut socket, &mut reader, &mut out).await;
        let written = bodies(&out);
        assert_eq!(
            written[0]["error"]["code"], -32700,
            "a parse error, null id: {written:?}"
        );
        assert!(written[0]["id"].is_null(), "{written:?}");
        assert!(
            written[1]["result"]["capabilities"].is_object(),
            "the handshake after it was still served: {written:?}"
        );
    }

    /// A handler's own notifications are written WHILE it runs, and
    /// that is not an optimization. tower-lsp's client handle is a
    /// ONE-slot channel: the second notification a handler sends parks
    /// its own flush until someone takes the first, so a pump that
    /// drained only after the call returned deadlocked the handler on
    /// its second message — and `initialized` sends two the moment the
    /// editor has a workspace folder, which is every real session. The
    /// wasm neutral server could not finish its own handshake.
    #[tokio::test]
    async fn a_handler_is_not_deadlocked_by_its_own_notifications() {
        let root = empty_root("notify");
        let mut reader = session(
            &[
                handshake(&root.0),
                serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
                serde_json::json!({"jsonrpc": "2.0", "method": "exit"}),
            ],
            3,
        );
        let (mut service, mut socket) =
            crate::session::build_service(|client| NmlLanguageServer::with_store(client, None));
        let mut out = Vec::new();
        pumped(&mut service, &mut socket, &mut reader, &mut out).await;
        let logs: Vec<String> = bodies(&out)
            .iter()
            .filter(|body| body["method"] == "window/logMessage")
            .filter_map(|body| body["params"]["message"].as_str().map(str::to_string))
            .collect();
        assert!(
            logs.len() >= 2,
            "`initialized` sends one line per denial, one for the roots it \
             indexed and one for itself — all of them must reach the wire: {logs:?}"
        );
    }

    /// WHY [`crate::ask`] exists, executed: a server→client REQUEST
    /// awaited inside a handler can never be answered under this
    /// pump. The request itself IS written — the concurrent drain
    /// puts it on the wire while the handler waits (before that
    /// drain it was not even written) — but its answer can only
    /// arrive by a READ, and the pump reads only between calls: the
    /// handler is suspended waiting for a frame that is never read,
    /// so the call never completes, the pump never reads again, and
    /// the process is unkillable by `exit`. Here the file-watch
    /// registration is the request (this lane is native, so `ask`
    /// lets it through); on wasm32 `ask` shuts every one of them off,
    /// which is what keeps the neutral server alive. Both halves are
    /// pinned: the request on the wire, and the pump that never
    /// finishes — a pump that learned to read answers mid-call would
    /// have to change this test on purpose.
    #[tokio::test]
    async fn a_server_to_client_request_stops_this_pump() {
        let root = empty_root("stops");
        let mut handshake = handshake(&root.0);
        handshake["params"]["capabilities"] = serde_json::json!({
            "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": true } }
        });
        let mut reader = session(
            &[
                handshake,
                serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
                serde_json::json!({"jsonrpc": "2.0", "method": "exit"}),
            ],
            3,
        );
        let (mut service, mut socket) =
            crate::session::build_service(|client| NmlLanguageServer::with_store(client, None));
        let mut out = Vec::new();
        let pump = super::run(&mut service, &mut socket, &mut reader, &mut out);
        // The wait cannot flake: nothing in the pump can make
        // progress, so no budget lets it finish. It is a bound on the
        // test, not a race.
        assert!(
            tokio::time::timeout(Duration::from_secs(2), pump)
                .await
                .is_err(),
            "the pump finished a call that awaits an answer it can never read"
        );
        let methods: Vec<String> = bodies(&out)
            .iter()
            .filter_map(|b| b["method"].as_str().map(str::to_string))
            .collect();
        assert!(
            methods.iter().any(|m| m == "client/registerCapability"),
            "the request was written while the handler waited: {methods:?}"
        );
    }
}
