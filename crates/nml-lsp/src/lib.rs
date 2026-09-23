// No `unsafe` in this crate (RFC 0019 item 0, E35): enforced at the root.
#![forbid(unsafe_code)]

pub(crate) mod ask;
pub mod diagnostics;
pub mod duration_lsp;
pub mod packages;
pub mod position;
#[cfg(test)]
mod scratch;
pub mod semantic_tokens;
pub mod server;
// The wasm editor's directory listings and their memo. Compiled under
// `test` on every target too, so the wiring cannot rot uncompiled.
#[cfg(any(target_os = "wasi", test))]
mod wasi_fs;

use std::process::ExitCode;
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use tower_lsp::Server;
use tower_lsp::jsonrpc::{Request, Response};
use tower_lsp::{Client, ClientSocket, ExitedError, LspService};

use server::NmlLanguageServer;

/// On `wasm32` the neutral server runs under VS Code's `wasm-wasi-core`, whose
/// stdio model is *synchronous*: the host blocks the (dedicated) worker on a
/// read until a message arrives, so the server must be a plain
/// read→process→write pump — each response is written *before* the next blocking
/// read. tower-lsp's native `Server::serve` reads input and writes output
/// concurrently, which that model deadlocks (a synchronous read starves the loop
/// that flushes responses); so on wasm `serve_stdio` drives the `LspService`
/// directly here instead. Server→client *requests* are the one thing a
/// synchronous pump cannot await — EVERY one of them, not just the dynamic
/// capability registration — so they all go through [`ask`], which shuts
/// them off on wasm; server→client *notifications* (`publishDiagnostics`,
/// `logMessage`) are queued by the client handle without waiting for anyone
/// and drained after each call.
/// `Content-Length` framing for the synchronous wasm pump — compiled on
/// the native test lane too, so the frame bound is pinned where a
/// test can run instead of living only in a module no test lane builds.
#[cfg(any(target_arch = "wasm32", test))]
mod framing {
    use std::io::{BufRead, Write};

    use tower_lsp::jsonrpc::Request;

    /// One well-formed frame off the wire.
    pub(super) enum Frame {
        /// Its body is a JSON-RPC message.
        Message(Box<Request>),
        /// Its body is NOT one. The framing is intact — the whole body was
        /// read — so the stream continues; only the message is lost, and
        /// the answer is a parse error. (Terminating here instead handed a
        /// buggy client one bad body and the server's life.) The NATIVE
        /// transport gives the same answer and then stops reading —
        /// `tokio_util`'s `FramedRead` is fused after a decoder error, so
        /// tower-lsp's loop ends and the process exits 0 — which is
        /// MEASURED and pinned (`tests/index_walk_stdio.rs`,
        /// `one_malformed_frame_ends_the_native_session_with_a_clean_exit_code`),
        /// not a property this pump inherits.
        Unparsable,
    }

    /// A bogus/corrupt `Content-Length` must never OOM the memory-limited
    /// module: cap the body allocation well above any real LSP message (nml
    /// config/schema files are KB–low-MB). Over the cap ⇒ treat the stream as
    /// corrupt and terminate, rather than allocate gigabytes.
    ///
    /// LIMIT: reach=peer guards=memory surface=editor shown="256 MiB" — bytes of one JSON-RPC frame the wasm transport will allocate
    pub(super) const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

    /// One HEADER line's bound. [`MAX_FRAME_BYTES`] caps the body and
    /// nothing else: the headers were read with an unbounded
    /// `read_line`, so a peer that sent `Content-Length:` and then
    /// gigabytes with no newline allocated without limit — the exact
    /// OOM of the memory-limited module that bound exists to prevent,
    /// reached BEFORE it is consulted. A real header (`Content-Length:
    /// 65536`) is tens of bytes. A line that does not end inside its
    /// bound leaves no later byte locatable, so the stream is over.
    ///
    /// LIMIT: reach=peer guards=memory surface=editor shown="8 KiB" — bytes of one JSON-RPC header line the wasm transport will read
    pub(super) const MAX_HEADER_BYTES: u64 = 8 * 1024;

    /// Read one `Content-Length`-framed message. Blocking (the host
    /// services the wait). `None` means the STREAM is over: EOF, or a
    /// header this reader cannot frame past — after which no later byte
    /// can be located, so there is nothing to continue from.
    pub(super) fn read_frame(reader: &mut impl BufRead) -> Option<Frame> {
        use std::io::Read as _;
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            // Bounded: the header half of the frame, not only the body.
            if reader
                .by_ref()
                .take(MAX_HEADER_BYTES)
                .read_line(&mut line)
                .ok()?
                == 0
            {
                return None; // EOF
            }
            if !line.ends_with('\n') {
                // The line did not end inside its bound, or the stream
                // ended mid-header: no later byte can be located.
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break; // end of headers
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                content_length = v.trim().parse().ok()?;
            }
        }
        if content_length == 0 || content_length > MAX_FRAME_BYTES {
            return None;
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).ok()?;
        Some(match serde_json::from_slice(&body) {
            Ok(request) => Frame::Message(Box::new(request)),
            Err(_) => Frame::Unparsable,
        })
    }

    /// Write one framed message body (already serialized).
    pub(super) fn write_frame(writer: &mut impl Write, body: &[u8]) {
        let _ = write!(writer, "Content-Length: {}\r\n\r\n", body.len());
        let _ = writer.write_all(body);
        let _ = writer.flush();
    }

    #[cfg(test)]
    mod tests {
        use super::{Frame, MAX_FRAME_BYTES, MAX_HEADER_BYTES, read_frame, write_frame};

        /// The frame bound, pinned natively: a well-formed frame
        /// round-trips; a `Content-Length` past [`MAX_FRAME_BYTES`] — or
        /// zero — is a corrupt stream, refused before a byte of body is
        /// allocated (the reader would otherwise reserve the advertised
        /// gigabytes).
        #[test]
        fn a_frame_past_the_bound_is_corrupt_not_an_allocation() {
            let body = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0", "method": "initialized", "params": {}
            }))
            .expect("serializes");
            let mut buf = Vec::new();
            write_frame(&mut buf, &body);
            let mut reader = std::io::Cursor::new(buf);
            let Some(Frame::Message(req)) = read_frame(&mut reader) else {
                panic!("a well-formed frame parses");
            };
            assert_eq!(req.method(), "initialized");
            let over = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);
            assert!(read_frame(&mut std::io::Cursor::new(over.into_bytes())).is_none());
            let zero = b"Content-Length: 0\r\n\r\n".to_vec();
            assert!(read_frame(&mut std::io::Cursor::new(zero)).is_none());
        }

        /// The HEADER bound, pinned: a header line that does not end
        /// inside [`MAX_HEADER_BYTES`] ends the stream instead of
        /// growing a `String` without limit — pre-fix this read was a
        /// bare `read_line`, so `Content-Length:` followed by bytes
        /// with no newline allocated all of them, under the frame
        /// bound's nose. A header exactly at the bound (its newline the
        /// last byte) still frames.
        #[test]
        fn a_header_line_past_the_bound_ends_the_stream_not_the_heap() {
            let body = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0", "method": "initialized", "params": {}
            }))
            .expect("serializes");
            for over in [false, true] {
                let pad = MAX_HEADER_BYTES as usize + usize::from(over);
                // A header the reader ignores, padded to the bound: at
                // the bound its newline is the last byte read; one byte
                // past, the newline is not reached.
                let head = "X-Pad: ";
                let mut wire =
                    format!("{head}{}\r\n", "p".repeat(pad - head.len() - 2)).into_bytes();
                assert_eq!(wire.len(), pad);
                wire.extend_from_slice(
                    format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes(),
                );
                wire.extend_from_slice(&body);
                let framed = read_frame(&mut std::io::Cursor::new(wire));
                assert_eq!(framed.is_some(), !over, "pad {pad}");
            }
            // The shape that OOM'd: a header that never ends.
            let endless = vec![b'x'; MAX_HEADER_BYTES as usize * 2];
            assert!(read_frame(&mut std::io::Cursor::new(endless)).is_none());
        }

        /// A well-formed frame whose BODY is not a JSON-RPC message costs
        /// that message, not the stream: the frame is consumed whole and
        /// the NEXT one still reads.
        #[test]
        fn an_unparsable_body_leaves_the_stream_framed() {
            let mut buf = Vec::new();
            write_frame(&mut buf, b"{ not json");
            write_frame(
                &mut buf,
                &serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0", "method": "exit"
                }))
                .expect("serializes"),
            );
            let mut reader = std::io::Cursor::new(buf);
            assert!(matches!(read_frame(&mut reader), Some(Frame::Unparsable)));
            let Some(Frame::Message(req)) = read_frame(&mut reader) else {
                panic!("the frame after an unparsable body still reads");
            };
            assert_eq!(req.method(), "exit");
            assert!(read_frame(&mut reader).is_none(), "then EOF");
        }
    }
}

/// The synchronous read→process→write pump the wasm32 neutral server runs
/// on — compiled on the native test lane too, so the transport the
/// in-editor server lives or dies by is exercised where a test can run
/// (it used to be reachable only from a `wasm32` build, and nothing in
/// the suite touched it).
#[cfg(any(target_arch = "wasm32", test))]
mod pump {
    use std::io::{BufRead, Write};

    use futures::future::{self, Either};
    use futures::{FutureExt, StreamExt};
    use tower::{Service, ServiceExt};
    use tower_lsp::ClientSocket;
    use tower_lsp::jsonrpc::{Error, Id, Response};

    use super::framing::{Frame, read_frame, write_frame};
    use crate::NmlService;

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
            // the ending the service decided ([`crate::ExitSignal`]). The
            // pump must therefore not go back to the read — a host that
            // keeps stdin open would park the process there forever, and
            // it would outlive the editor session that asked it to stop.
            if let Some(ending) = service.ending() {
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
            let dir =
                std::env::temp_dir().join(format!("nml-lsp-pump-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Root(dunce::canonicalize(&dir).expect("canonicalize"))
        }

        /// Run the pump to completion, turning a DEADLOCK into a named
        /// failure instead of a hung suite. The bound is never reached by
        /// a working pump: every session here ends in `exit`.
        async fn pumped(
            service: &mut crate::NmlService,
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
                crate::build_service(|client| NmlLanguageServer::with_store(client, None));
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
                crate::build_service(|client| NmlLanguageServer::with_store(client, None));
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
                crate::build_service(|client| NmlLanguageServer::with_store(client, None));
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
                crate::build_service(|client| NmlLanguageServer::with_store(client, None));
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
                crate::build_service(|client| NmlLanguageServer::with_store(client, None));
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
                crate::build_service(|client| NmlLanguageServer::with_store(client, None));
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
}

/// Build the [`LspService`] with every nml custom method registered. The
/// single owner of custom-method wiring: the `nml-lsp` binary, the test
/// harness, and every schema provider (`nudge lsp`, RFC 0035) construct their
/// service here, so no call site can drift by forgetting a method. Adding a
/// method (e.g. `nml/status`) here reaches all of them at once.
pub fn build_service(init: impl FnOnce(Client) -> NmlLanguageServer) -> (NmlService, ClientSocket) {
    let (inner, socket) = LspService::build(init)
        // RFC 0030 introspection: which schema package validates a document,
        // from where, at which hash — callable by any LSP client.
        .custom_method("nml/schemaInfo", NmlLanguageServer::schema_info)
        // RFC 0010 tier 2: full error-index entries from the running binary
        // (`nml/explain`), and the code list behind the explain-a-code
        // palette (`nml/explainIndex`).
        .custom_method("nml/explain", NmlLanguageServer::explain)
        .custom_method("nml/explainIndex", NmlLanguageServer::explain_index)
        .finish();
    (
        NmlService {
            inner,
            exit: Arc::default(),
        },
        socket,
    )
}

/// The service every front end drives: tower-lsp's [`LspService`] behind
/// one look at the raw `initialize` params, for the one client capability
/// the typed params cannot carry. LSP 3.17 spells the pull-diagnostics
/// workspace capability `workspace.diagnostics.refreshSupport`
/// (vscode-languageclient sends exactly that); lsp-types 0.94.1 names its
/// field `diagnostic` and drops the specification's key on
/// deserialization, so a server reading only the typed capability never
/// sees a conforming client's declaration. The look reads the
/// specification's spelling from the request as sent and records it on
/// the server before the typed handler runs; the handler still reads
/// lsp-types' spelling, so a client generated from either counts.
pub struct NmlService {
    inner: LspService<NmlLanguageServer>,
    exit: Arc<ExitSignal>,
}

impl NmlService {
    /// The server behind the service (tests reach its state through here).
    pub fn inner(&self) -> &NmlLanguageServer {
        self.inner.inner()
    }

    /// The session's ending, shared with the transport that drives this
    /// service: it resolves once the client has sent `exit`.
    pub fn exit_signal(&self) -> Arc<ExitSignal> {
        Arc::clone(&self.exit)
    }

    /// `Some` once the client has sent `exit`. A synchronous read for the
    /// transport that polls between frames (the wasm pump).
    pub fn ending(&self) -> Option<SessionEnd> {
        self.exit.ending()
    }
}

/// How a session ended — the FACT, for the embedder to map onto whatever
/// exit discipline its process has. LSP 3.17 §exit fixes the process exit
/// code for the two protocol endings, and [`SessionEnd::exit_code`] is that
/// mapping for a `main` that has no discipline of its own.
///
/// "The server should exit with success code 0 if the shutdown request has
/// been received before; otherwise with error code 1." A client that simply
/// goes away — stdin EOF with no `exit` — is not a protocol ending at all,
/// and the server's own outcome is clean: that maps to success. It is also
/// how an editor ends a server it could never talk to, and a code of 1
/// there would read as the server's fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnd {
    /// `exit` after `shutdown`: the protocol's orderly ending.
    Exited,
    /// `exit` with no `shutdown` before it: the client's error.
    ExitedWithoutShutdown,
    /// The client closed the connection without `exit`.
    Disconnected,
}

impl SessionEnd {
    /// The process exit code LSP 3.17 §exit prescribes for this ending.
    pub const fn exit_code(self) -> ExitCode {
        match self {
            Self::Exited | Self::Disconnected => ExitCode::SUCCESS,
            Self::ExitedWithoutShutdown => ExitCode::FAILURE,
        }
    }
}

impl From<SessionEnd> for ExitCode {
    fn from(end: SessionEnd) -> Self {
        end.exit_code()
    }
}

/// LSP 3.17 §exit, decided in ONE place for both transports.
///
/// The service records `shutdown`, and at `exit` publishes the
/// [`SessionEnd`]; each transport stops reading and returns it, and the
/// binary's `main` returns its exit code to the operating system. Two things
/// were wrong before this existed (measured on the native binary): the
/// transport read on after `exit` until the client closed stdin, so a client
/// that kept the pipe open kept the process; and the process ended with 0
/// whether or not `shutdown` had come.
#[derive(Debug, Default)]
pub struct ExitSignal {
    shut_down: std::sync::atomic::AtomicBool,
    ending: std::sync::OnceLock<SessionEnd>,
    exited: tokio::sync::Notify,
}

impl ExitSignal {
    /// Read in FRAME ORDER, from the one place every message passes through,
    /// and deliberately upstream of tower-lsp's lifecycle layer.
    ///
    /// The alternative — counting a `shutdown` only once its handler has run,
    /// so that one tower-lsp REFUSED (out of order, or with params it cannot
    /// deserialize) does not count — cannot be made race-free: `Server::serve`
    /// drives up to four handlers concurrently with the reader, so a client
    /// that pipelines `shutdown` and `exit` without waiting for the response
    /// could have its `exit` observed while the `shutdown` future is still
    /// pending, and the ending would depend on scheduling. Frame order is a
    /// total order the client controls; "the shutdown request has been
    /// received before" (LSP 3.17 §exit) is a statement about exactly that.
    ///
    /// The cost, MEASURED and accepted: a client whose `shutdown` was refused
    /// — the only way to provoke one is to send a malformed or out-of-order
    /// request — still gets exit code 0. Its session was already broken, and
    /// an exit code that depended on which future the executor polled first
    /// would be worse than one that is merely generous.
    fn observe(&self, method: &str) {
        use std::sync::atomic::Ordering::Relaxed;
        match method {
            "shutdown" => self.shut_down.store(true, Relaxed),
            "exit" => {
                let ending = if self.shut_down.load(Relaxed) {
                    SessionEnd::Exited
                } else {
                    SessionEnd::ExitedWithoutShutdown
                };
                // First `exit` wins; a second one is the client's error.
                let _ = self.ending.set(ending);
                self.exited.notify_one();
            }
            _ => {}
        }
    }

    /// How the session ended, once `exit` has been received.
    pub fn ending(&self) -> Option<SessionEnd> {
        self.ending.get().copied()
    }

    /// Resolves once `exit` has been received.
    pub async fn ended(&self) -> SessionEnd {
        loop {
            if let Some(ending) = self.ending() {
                return ending;
            }
            // A permit stored by `notify_one` before this wait began is
            // consumed here, so the signal cannot be missed between the
            // check above and the wait.
            self.exited.notified().await;
        }
    }
}

impl tower::Service<Request> for NmlService {
    type Response = Option<Response>;
    type Error = ExitedError;
    type Future = <LspService<NmlLanguageServer> as tower::Service<Request>>::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        self.exit.observe(req.method());
        if req.method() == "initialize" {
            // PARKED, not applied: tower-lsp refuses a duplicate
            // `initialize` (`invalid_request`) without running the typed
            // handler, and this look is upstream of that lifecycle — so
            // the handler, which runs for the accepted frame alone, is
            // what turns the capability on. Written even when the frame
            // declares nothing, so a refused frame cannot leave the
            // previous one's answer parked for the next handshake.
            let declared = req.params().is_some_and(client_declares_diagnostic_refresh);
            self.inner.inner().park_raw_refresh_declaration(declared);
        }
        self.inner.call(req)
    }
}

/// LSP 3.17 §WorkspaceClientCapabilities, read from the `initialize` params
/// as the client sent them: `workspace.diagnostics.refreshSupport` is true.
pub fn client_declares_diagnostic_refresh(params: &serde_json::Value) -> bool {
    params
        .pointer("/capabilities/workspace/diagnostics/refreshSupport")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

/// Serve a language server over stdio until the session ends, and say how
/// it ended ([`SessionEnd`]): map that onto the process's exit code —
/// [`SessionEnd::exit_code`] is the protocol's mapping — and return it from
/// `main`. `init` chooses the flavor (`NmlLanguageServer::new` for the
/// neutral server, [`serve`]'s provider wiring for a tool). Async so an
/// embedder with its own runtime (a provider tool) can `.await` it directly.
#[cfg(not(target_arch = "wasm32"))]
pub async fn serve_stdio(init: impl FnOnce(Client) -> NmlLanguageServer) -> SessionEnd {
    let (service, socket) = build_service(init);
    let exit = service.exit_signal();
    let served = Server::new(detached_stdin(), tokio::io::stdout(), socket).serve(service);
    tokio::select! {
        // The transport ended: stdin reached EOF. If `exit` had ALREADY been
        // decided, that is the session's ending — not this one. A client that
        // sends `exit` and closes the pipe in the same breath leaves both
        // branches ready in ONE poll, and `select!` picks between ready
        // branches at random: reading the transport's ending there reported
        // `Disconnected` (code 0) for a session the protocol says is code 1.
        // Measured on the binary before this line: 15 of 60 runs exited 0.
        () = served => exit.ending().unwrap_or(SessionEnd::Disconnected),
        // `exit` arrived and the client is still holding stdin open.
        // tower-lsp's transport would read on until EOF — a client that keeps
        // stdin open after `exit` kept the process (measured) — so the
        // session ends here.
        ending = exit.ended() => ending,
    }
}

/// Stdin read on a thread of this crate's own, which the runtime does not
/// wait for.
///
/// `tokio::io::stdin()` reads on the runtime's blocking pool, and dropping
/// the runtime — which is what returning from a `#[tokio::main]` does —
/// waits for that pool with no timeout (tokio's `BlockingPool::shutdown`).
/// A thread parked in `read(2)` on a pipe the client still holds never
/// returns, so the process lived on after `exit` even though the session
/// had ended (measured on the binary, with stdin held open). A detached
/// thread is not the runtime's to wait for: when the session ends, `main`
/// returns and the process exits with the code; when the client closes
/// stdin, the read returns 0, the thread ends, and the reader reports EOF.
///
/// The cost of not waiting: that thread holds `std::io::Stdin`'s own lock
/// until the read returns, so a session that ended on `exit` leaves it held
/// for as long as the client keeps the pipe open. ONE session per process,
/// and the process ends when it returns — which is what a `<tool> lsp`
/// subcommand does. A second [`serve_stdio`] beside it, or a read of stdin
/// after one returns, would wait on that lock rather than on any input.
#[cfg(not(target_arch = "wasm32"))]
fn detached_stdin() -> DetachedStdin {
    use std::io::Read as _;
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(8);
    let spawned = std::thread::Builder::new()
        .name("nml-lsp stdin".to_string())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(Ok(buf[..n].to_vec())).is_err() {
                            break; // the reader is gone: the session ended first
                        }
                    }
                    Err(err) => {
                        let _ = tx.blocking_send(Err(err));
                        break;
                    }
                }
            }
        });
    if let Err(err) = spawned {
        // No thread, no input: the reader reports the failure as a read
        // error and the transport ends the session.
        let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(1);
        let _ = tx.try_send(Err(err));
        return DetachedStdin {
            rx,
            pending: Vec::new(),
            at: 0,
        };
    }
    DetachedStdin {
        rx,
        pending: Vec::new(),
        at: 0,
    }
}

/// The read side of [`detached_stdin`]: chunks arrive over a channel, and
/// a closed channel is EOF.
#[cfg(not(target_arch = "wasm32"))]
struct DetachedStdin {
    rx: tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    pending: Vec<u8>,
    at: usize,
}

#[cfg(not(target_arch = "wasm32"))]
impl tokio::io::AsyncRead for DetachedStdin {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::task::Poll;
        let this = self.get_mut();
        if this.at >= this.pending.len() {
            match this.rx.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Err(err)),
                Poll::Ready(Some(Ok(chunk))) => {
                    this.pending = chunk;
                    this.at = 0;
                }
            }
        }
        let n = buf.remaining().min(this.pending.len() - this.at);
        buf.put_slice(&this.pending[this.at..this.at + n]);
        this.at += n;
        Poll::Ready(Ok(()))
    }
}

/// wasm32: drive the service with a synchronous pump (see [`pump`]) over
/// the host's stdio, and say how the session ended.
#[cfg(target_arch = "wasm32")]
pub async fn serve_stdio(init: impl FnOnce(Client) -> NmlLanguageServer) -> SessionEnd {
    let (mut service, mut socket) = build_service(init);
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let mut writer = std::io::stdout().lock();
    pump::run(&mut service, &mut socket, &mut reader, &mut writer).await
}

/// Serve as a schema provider (RFC 0035 in-binary channel): the neutral server
/// plus this tool's embedded `package` injected at in-binary precedence, over
/// stdio. This is the whole body of a provider tool's `<tool> lsp` subcommand —
/// `nml_lsp::serve(MY_PACKAGE.clone()).await`.
pub async fn serve(package: nml_validate::package::SchemaPackage) -> SessionEnd {
    serve_stdio(|client| {
        NmlLanguageServer::with_provider(client, package, nml_validate::store::Store::user())
    })
    .await
}

#[cfg(test)]
mod exit_signal_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::{ExitSignal, SessionEnd};

    /// The bound that turns a signal which never arrives into a NAMED
    /// failure instead of a suite that hangs. Never reached by a working
    /// signal: every wait below is already satisfiable when it starts, or
    /// is satisfied by the very next statement.
    const NEVER: Duration = Duration::from_secs(10);

    /// LSP 3.17 §exit: the code is decided by what came BEFORE, and a
    /// client that sends `exit` twice — or sends `shutdown` after it, which
    /// the protocol forbids — cannot rewrite the answer it already got. The
    /// ending is published once, and the transport may read it at any later
    /// point without the value moving under it.
    #[test]
    fn the_first_exit_decides_the_ending_and_nothing_later_rewrites_it() {
        let signal = ExitSignal::default();
        assert_eq!(signal.ending(), None, "nothing has happened yet");
        signal.observe("exit");
        assert_eq!(signal.ending(), Some(SessionEnd::ExitedWithoutShutdown));
        // Both of the things a confused client does next.
        signal.observe("shutdown");
        signal.observe("exit");
        assert_eq!(
            signal.ending(),
            Some(SessionEnd::ExitedWithoutShutdown),
            "a late `shutdown` must not turn the client's error into an orderly exit"
        );
    }

    /// The orderly ending, and the only order that produces it.
    #[test]
    fn shutdown_then_exit_is_the_orderly_ending() {
        let signal = ExitSignal::default();
        signal.observe("shutdown");
        assert_eq!(
            signal.ending(),
            None,
            "`shutdown` alone does not end anything"
        );
        signal.observe("exit");
        assert_eq!(signal.ending(), Some(SessionEnd::Exited));
    }

    /// Methods that are not the two lifecycle ones leave the ending alone —
    /// the service sees EVERY request go past, so a match that was too
    /// generous would end sessions on ordinary traffic.
    #[test]
    fn ordinary_traffic_decides_nothing() {
        let signal = ExitSignal::default();
        for method in [
            "initialize",
            "initialized",
            "textDocument/didOpen",
            "exited",
            "shutdownNow",
            "$/cancelRequest",
        ] {
            signal.observe(method);
            assert_eq!(signal.ending(), None, "{method} ended the session");
        }
        signal.observe("exit");
        assert_eq!(
            signal.ending(),
            Some(SessionEnd::ExitedWithoutShutdown),
            "`shutdownNow` must not have counted as `shutdown`"
        );
    }

    /// A waiter that arrives AFTER the signal must not park. The native
    /// transport's `tokio::select!` polls `ended()` for the first time only
    /// once the reader yields, which is routinely after the `exit` frame has
    /// been through the service: a `notified()` awaited before the value is
    /// read would wait for a notification that has already happened.
    #[tokio::test]
    async fn a_waiter_that_arrives_after_the_signal_does_not_park() {
        let signal = ExitSignal::default();
        signal.observe("shutdown");
        signal.observe("exit");
        let ending = tokio::time::timeout(NEVER, signal.ended())
            .await
            .expect("`ended()` parked on a signal that had already fired");
        assert_eq!(ending, SessionEnd::Exited);
    }

    /// EVERY reader learns the ending, not just the first.
    ///
    /// `notify_one` stores exactly ONE permit, so a `ended()` that waited
    /// before it looked would consume that permit on the first call and park
    /// the second one forever — and the two orders would only APPEAR to work
    /// because production has a single waiter (`serve_stdio`'s `select!`).
    /// The published value is the fact; the notification only ends the wait.
    #[tokio::test]
    async fn every_reader_learns_the_ending_not_only_the_first() {
        let signal = ExitSignal::default();
        signal.observe("shutdown");
        signal.observe("exit");
        for round in 0..3 {
            let ending = tokio::time::timeout(NEVER, signal.ended())
                .await
                .unwrap_or_else(|_| panic!("`ended()` parked on round {round}"));
            assert_eq!(ending, SessionEnd::Exited);
        }
    }

    /// …and one that arrives BEFORE it is woken by it. The permit
    /// `notify_one` stores is what makes both orders work with one
    /// mechanism.
    #[tokio::test]
    async fn a_waiter_that_arrives_before_the_signal_is_woken_by_it() {
        let signal = Arc::new(ExitSignal::default());
        let waiting = Arc::clone(&signal);
        let waiter = tokio::spawn(async move { waiting.ended().await });
        // Let the task reach the wait before the signal fires.
        tokio::task::yield_now().await;
        assert_eq!(signal.ending(), None, "the task must not have ended it");
        signal.observe("exit");
        let ending = tokio::time::timeout(NEVER, waiter)
            .await
            .expect("`ended()` was never woken")
            .expect("the waiting task panicked");
        assert_eq!(ending, SessionEnd::ExitedWithoutShutdown);
    }

    /// The protocol's code for each ending, in one place: a `main` that
    /// returns `SessionEnd::exit_code()` is conformant by construction.
    #[test]
    fn each_ending_carries_the_code_the_specification_names() {
        assert_eq!(
            SessionEnd::Exited.exit_code(),
            std::process::ExitCode::SUCCESS
        );
        assert_eq!(
            SessionEnd::ExitedWithoutShutdown.exit_code(),
            std::process::ExitCode::FAILURE
        );
        assert_eq!(
            SessionEnd::Disconnected.exit_code(),
            std::process::ExitCode::SUCCESS
        );
        // `From` is the same mapping, not a second one.
        for end in [
            SessionEnd::Exited,
            SessionEnd::ExitedWithoutShutdown,
            SessionEnd::Disconnected,
        ] {
            assert_eq!(std::process::ExitCode::from(end), end.exit_code());
        }
    }
}
