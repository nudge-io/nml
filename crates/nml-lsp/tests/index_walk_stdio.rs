//! r75 — the workspace index walk driven over stdio, WATCHDOG-bounded.
//! A pin for a hang has to be able to fail: an in-process harness on a
//! single-threaded runtime cannot, because a blocking read inside
//! `initialized` blocks the runtime, timeout futures included. So the
//! `nml-lsp` binary is driven here exactly as an editor drives it, under
//! a kill-timer that turns a hung server into a red test instead of a
//! hung suite.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The watchdog's budget: the sweep over a three-entry workspace takes
/// milliseconds; a hung server is killed at the bound and the test fails
/// on the missing response, never on the CI job's own timeout.
const WATCHDOG: Duration = Duration::from_secs(20);

fn write_frame(stdin: &mut ChildStdin, msg: &Value) {
    let body = serde_json::to_vec(msg).expect("serializes");
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("header");
    stdin.write_all(&body).expect("body");
    stdin.flush().expect("flush");
}

/// One `Content-Length`-framed message, or `None` at EOF (a killed
/// server).
fn read_frame(reader: &mut BufReader<ChildStdout>) -> Option<Value> {
    let mut content_length = 0usize;
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
            content_length = v.trim().parse().ok()?;
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// Read until the response to request `id` arrives, answering every
/// server→client REQUEST (dynamic capability registration) with a null
/// result on the way, as a conforming client would.
fn response_to(
    reader: &mut BufReader<ChildStdout>,
    stdin: &mut ChildStdin,
    id: i64,
) -> Option<Value> {
    loop {
        let msg = read_frame(reader)?;
        let is_request = msg.get("method").is_some() && msg.get("id").is_some();
        if is_request {
            write_frame(
                stdin,
                &json!({"jsonrpc": "2.0", "id": msg["id"].clone(), "result": null}),
            );
            continue;
        }
        if msg.get("id") == Some(&json!(id)) {
            return Some(msg);
        }
    }
}

/// `kill -9` the child when the watchdog fires; the returned sender
/// disarms it.
fn arm_watchdog(child: &Child) -> std::sync::mpsc::Sender<()> {
    let pid = child.id();
    let (disarm, fired) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if fired.recv_timeout(WATCHDOG).is_err() {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
    });
    disarm
}

/// r74-cert-cli F3: a FIFO named `*.nml` in the workspace hung the index
/// sweep forever (`fs::read_to_string` on a pipe with no writer) — the
/// candidate inside `initialize`, the merged tree inside `initialized`
/// — and the single-task server answered nothing again; git cannot
/// commit a FIFO, so this needs write access to the checkout, which is
/// why it is low. The walk keeps regular files only now. The pin: with
/// the FIFO in place, `initialize` → `initialized` → a completion and a
/// `shutdown` all answer inside the watchdog, and the model beside the
/// FIFO is still indexed.
#[test]
fn a_fifo_named_nml_in_the_workspace_never_hangs_the_index_sweep() {
    let dir = std::env::temp_dir().join(format!("nml-lsp-fifo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let ws = dunce::canonicalize(&dir).expect("canonical scratch dir");
    let app = ws.join("app.nml");
    std::fs::write(&app, "\n").expect("write app");
    std::fs::write(ws.join("ok.model.nml"), "model okmodel:\n    a number\n").expect("write model");
    let status = Command::new("mkfifo")
        .arg(ws.join("fifo.model.nml"))
        .status()
        .expect("mkfifo runs");
    assert!(status.success(), "mkfifo");

    // An EMPTY store: the machine's own store would auto-associate a
    // package and shadow the workspace index (the certifier hit exactly
    // this), and the pin reads the index through completion.
    let store = dir.join("store");
    std::fs::create_dir_all(&store).expect("store dir");
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nml-lsp"))
        .env("NML_SCHEMA_STORE_DIR", &store)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nml-lsp");
    let disarm = arm_watchdog(&child);
    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    let uri = |p: &std::path::Path| format!("file://{}", p.display());

    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"capabilities": {}, "rootUri": uri(&ws)}}),
    );
    let init = response_to(&mut reader, &mut stdin, 1);
    assert!(
        init.as_ref().is_some_and(|r| r.get("result").is_some()),
        "`initialize` must answer: {init:?}"
    );
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
    );
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "textDocument/didOpen",
                "params": {"textDocument": {"uri": uri(&app), "languageId": "nml",
                                            "version": 1, "text": "\n"}}}),
    );
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "textDocument/completion",
                "params": {"textDocument": {"uri": uri(&app)},
                           "position": {"line": 0, "character": 0}}}),
    );
    let completion = response_to(&mut reader, &mut stdin, 2);
    assert!(
        completion.is_some(),
        "the sweep hung on the FIFO: no completion answer within {WATCHDOG:?}"
    );
    let labels: Vec<String> = completion
        .and_then(|r| r.get("result").cloned())
        .and_then(|r| r.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|i| i["label"].as_str().map(str::to_string))
        .collect();
    assert!(
        labels.iter().any(|l| l == "okmodel"),
        "the model beside the FIFO is still indexed: {labels:?}"
    );
    // No `params` key: tower-lsp deserializes a notification/request's
    // params strictly, and `"params": null` on a no-params method is
    // answered -32602 `Unexpected params` — a REFUSAL, which
    // `.is_some()` could not tell from an answer. Asserting the `result`
    // is what makes this a pin on the server shutting down.
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "shutdown"}),
    );
    let shutdown = response_to(&mut reader, &mut stdin, 3).expect("`shutdown` must answer");
    assert!(
        shutdown.get("result").is_some(),
        "`shutdown` was refused, not served: {shutdown:?}"
    );
    assert!(
        started.elapsed() < WATCHDOG,
        "the whole exchange must finish inside the watchdog"
    );
    let _ = disarm.send(());
    // The exchange is the pin; the process's own exit is not (the
    // server lingers after `exit` until its stdin closes, and the
    // certifier's driver killed it after 5 s too): close the wire, give
    // it a moment, then kill whatever is left.
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "exit", "params": null}),
    );
    drop(stdin);
    drop(reader);
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A scratch directory removed when the guard drops.
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A freshly spawned `nml-lsp`, pointed at an EMPTY schema store.
///
/// Hermetic on purpose: `Store::user()` falls back to the machine's own data
/// directory, so a server spawned with no override reads whatever packages
/// the person running the suite happens to have installed — and an exit-code
/// pin that can be changed by something outside the repository is not a pin.
fn spawn_server(tag: &str) -> (Child, Scratch) {
    let dir = std::env::temp_dir().join(format!("nml-lsp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch store dir");
    let child = Command::new(env!("CARGO_BIN_EXE_nml-lsp"))
        .env("NML_SCHEMA_STORE_DIR", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nml-lsp");
    (child, Scratch(dir))
}

/// Wait up to `budget` for the child to end; `None` if it is still running.
fn exit_within(child: &mut Child, budget: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return Some(status);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// LSP 3.17 §exit is not gated on the handshake: "A notification to ask the
/// server to exit its process", and the code is 1 because no `shutdown` came
/// before it. tower-lsp refuses ORDINARY requests before `initialize`
/// (`Server not initialized`), so a reading in which `exit` were refused the
/// same way would leave an editor that gave up mid-handshake with a process
/// it could only kill — and the [`nml_lsp::ExitSignal`] that decides the
/// ending sits UPSTREAM of that lifecycle layer, which is the thing this pin
/// is about. Measured before the ending existed: alive after `exit`, exit
/// code 0 when it eventually went.
#[test]
fn exit_before_initialize_ends_the_process_with_code_1() {
    let (mut child, _store) = spawn_server("exit-before-initialize");
    let disarm = arm_watchdog(&child);
    let mut stdin = child.stdin.take().expect("stdin");
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "exit", "params": null}),
    );
    // stdin stays OPEN: the notification has to be what ends it.
    let status = exit_within(&mut child, Duration::from_secs(5));
    let _ = disarm.send(());
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let status = status.expect("`exit` must end the process even before `initialize`");
    assert_eq!(status.code(), Some(1), "{status:?}");
}

/// MEASURED, and NOT what this repository said: ONE malformed frame ends
/// the native session.
///
/// tower-lsp answers a body it cannot parse with a parse error
/// (-32700, null id) and then stops reading — `tokio_util`'s
/// `FramedRead` is FUSED after a decoder error (`framed_impl.rs`: the
/// poll after `has_errored` returns `Ready(None)`), so `Server::serve`'s
/// `while let Some(msg) = framed_stdin.next().await` leaves the loop, the
/// transport ends, and `serve_stdio` reports `SessionEnd::Disconnected` —
/// process exit code 0, the code the protocol gives a client that simply
/// went away. The editor therefore sees a CLEAN exit for a session a
/// single bad byte ended.
///
/// Three shapes reach it, all of them a client's or a proxy's bug rather
/// than an attack: a body that is not JSON-RPC, a frame carrying more
/// than two headers (`httparse` is given two slots), and a
/// `Content-Type` whose charset is not UTF-8. The wasm pump answers all
/// three and READS ON (`nml_lsp`'s `framing::Frame::Unparsable`), so the
/// two transports disagree about the same wire — which is why this is
/// pinned rather than assumed: the pump's own test used to claim "the
/// native transport answers the same way and reads on", and nothing had
/// ever measured it.
///
/// This test records the behaviour as it IS. Changing it is an owner
/// decision (a `SessionEnd` variant for a transport fault, so the editor
/// is not told the exit was clean), not a silent fix.
#[test]
fn one_malformed_frame_ends_the_native_session_with_a_clean_exit_code() {
    for (label, wire) in [
        ("an unparsable body", b"Content-Length: 10\r\n\r\n{ not json".to_vec()),
        (
            "more than two headers",
            b"X-A: 1\r\nX-B: 2\r\nX-C: 3\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        ),
        (
            "a non-UTF-8 content type",
            b"Content-Type: application/vscode-jsonrpc; charset=utf-16\r\nContent-Length: 2\r\n\r\n{}"
                .to_vec(),
        ),
    ] {
        let (mut child, _store) = spawn_server("malformed-frame");
        let disarm = arm_watchdog(&child);
        let mut stdin = child.stdin.take().expect("stdin");
        let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
        stdin.write_all(&wire).expect("wire");
        stdin.flush().expect("flush");

        let answered = read_frame(&mut reader).expect("the parse error is answered");
        assert_eq!(answered["error"]["code"], -32700, "{label}: {answered:?}");
        assert!(answered["id"].is_null(), "{label}: {answered:?}");

        // stdin stays OPEN, and no `exit` was ever sent: anything that
        // ends the process now is the transport's own doing.
        let status = exit_within(&mut child, Duration::from_secs(5));
        let _ = disarm.send(());
        let code = status.and_then(|s| s.code());
        assert_eq!(
            code,
            Some(0),
            "{label}: the native transport ends the session on one malformed frame,              and reports it as a clean disconnect — if this is now `None` the              session survived (the defect is fixed; drop this pin), and if it is              some other code the ending changed"
        );
        drop(stdin);
        drop(reader);
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// A frame far larger than one read of stdin is REASSEMBLED.
///
/// The native transport reads stdin on a detached thread in 64 KiB chunks
/// ([`nml_lsp`'s `detached_stdin`]) and hands them to tower-lsp's codec
/// through an `AsyncRead` that splices each chunk into whatever space the
/// framed reader has. Nothing exercised that splice: every frame the suite
/// sent fitted in one chunk, so a transport that dropped the tail of a read
/// — or that ended the stream at the first partial one — passed everything.
/// A 1 MiB `initialize` crosses at least sixteen reads and several partial
/// copies, and its ANSWER is the proof: a frame the server could not
/// reassemble is one it never answers — and a decode error ENDS the native
/// session outright (pinned just above), so a later frame answering is not
/// evidence about this one either.
///
/// The size is also a statement about the NATIVE transport's lack of a frame
/// bound: the wasm pump caps a body at `MAX_FRAME_BYTES` because it allocates
/// the advertised length up front, while tower-lsp's codec grows its buffer
/// only as bytes actually arrive. 1 MiB is far inside anything real (a large
/// `didOpen`) and is here to prove reassembly, not to probe the bound.
#[test]
fn a_frame_far_larger_than_one_read_is_reassembled() {
    let (mut child, _store) = spawn_server("big-frame");
    let disarm = arm_watchdog(&child);
    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));

    // `initializationOptions` is free-form JSON in LSP 3.17, so this is a
    // well-formed request that happens to be a megabyte.
    let pad = "p".repeat(1024 * 1024);
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"capabilities": {}, "rootUri": null,
                           "initializationOptions": {"explainCommand": "nml.explain",
                                                     "pad": pad}}}),
    );
    let answered = response_to(&mut reader, &mut stdin, 1);
    assert!(
        answered.as_ref().is_some_and(|r| r.get("result").is_some()),
        "a 1 MiB `initialize` was not reassembled: {answered:?}"
    );

    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
    );
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown"}),
    );
    let shutdown = response_to(&mut reader, &mut stdin, 2).expect("`shutdown` must answer");
    assert!(
        shutdown.get("result").is_some(),
        "the session continued after the large frame: {shutdown:?}"
    );
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "exit", "params": null}),
    );
    let status = exit_within(&mut child, Duration::from_secs(5));
    let _ = disarm.send(());
    drop(stdin);
    drop(reader);
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(0),
        "the session ended orderly after a megabyte frame"
    );
}

/// LSP 3.17 §shutdown: "Clients must not send any notifications other than
/// `exit` or requests to a server to which they have sent a `shutdown`
/// request", and a server that receives one anyway answers `InvalidRequest`.
/// A server that served it would be running handlers after it had promised
/// to stop. The rule is tower-lsp's lifecycle layer, inherited rather than
/// written here — which is exactly why it is pinned: nothing else in this
/// repository would notice if that layer were replaced or bypassed. The
/// second `shutdown` is refused the same way, and `exit` afterwards is still
/// the orderly ending (code 0), because the FIRST `shutdown` was answered.
#[test]
fn a_request_after_shutdown_is_refused_and_exit_is_still_orderly() {
    let (mut child, _store) = spawn_server("after-shutdown");
    let disarm = arm_watchdog(&child);
    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"capabilities": {}, "rootUri": null}}),
    );
    assert!(
        response_to(&mut reader, &mut stdin, 1).is_some(),
        "`initialize` must answer"
    );
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
    );
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown"}),
    );
    let shutdown = response_to(&mut reader, &mut stdin, 2).expect("`shutdown` must answer");
    assert!(
        shutdown.get("result").is_some(),
        "the first `shutdown` is answered, not refused: {shutdown:?}"
    );

    // An ordinary request, after the promise to stop.
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "textDocument/completion",
                "params": {"textDocument": {"uri": "file:///nowhere.nml"},
                           "position": {"line": 0, "character": 0}}}),
    );
    let refused = response_to(&mut reader, &mut stdin, 3).expect("a refusal is still an answer");
    assert_eq!(
        refused["error"]["code"], -32600,
        "a request after `shutdown` must be InvalidRequest: {refused:?}"
    );

    // And a SECOND `shutdown` is a request like any other.
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 4, "method": "shutdown"}),
    );
    let twice = response_to(&mut reader, &mut stdin, 4).expect("the second `shutdown` answers");
    assert_eq!(
        twice["error"]["code"], -32600,
        "a second `shutdown` must be InvalidRequest: {twice:?}"
    );

    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "exit", "params": null}),
    );
    // stdin stays OPEN.
    let status = exit_within(&mut child, Duration::from_secs(5));
    let _ = disarm.send(());
    drop(stdin);
    drop(reader);
    let _ = child.kill();
    let _ = child.wait();
    let status = status.expect("the process must end on `exit` itself");
    assert_eq!(
        status.code(),
        Some(0),
        "a refused SECOND shutdown does not unmake the first: {status:?}"
    );
}

/// One session up to `exit`, with stdin left OPEN: the process must end on
/// the notification itself, not on the EOF that a client may never send.
/// Returns the exit status, or `None` if it was still running after 5 s.
fn exit_status_with_stdin_open(shutdown_first: bool) -> Option<std::process::ExitStatus> {
    // A tag PER CASE: cargo runs the two callers in parallel threads of one
    // process, so a tag that carried only the pid would have them sharing —
    // and deleting — one store directory.
    let (mut child, _store) = spawn_server(if shutdown_first {
        "exit-status-after-shutdown"
    } else {
        "exit-status-no-shutdown"
    });
    let disarm = arm_watchdog(&child);
    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"capabilities": {}, "rootUri": null}}),
    );
    assert!(
        response_to(&mut reader, &mut stdin, 1).is_some(),
        "`initialize` must answer"
    );
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
    );
    if shutdown_first {
        write_frame(
            &mut stdin,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown"}),
        );
        // The RESULT, not merely a response. MEASURED: `"params": null` on
        // this method is answered -32602 `Unexpected params`, and the
        // process still exited 0 — because the ending is decided in frame
        // order, upstream of tower-lsp's refusal. A pin that accepted any
        // response therefore proved "the word `shutdown` went past", not
        // "the server shut down", and would have stayed green if the
        // handshake had stopped working altogether.
        let shutdown = response_to(&mut reader, &mut stdin, 2).expect("`shutdown` must answer");
        assert!(
            shutdown.get("result").is_some(),
            "`shutdown` was refused, not served: {shutdown:?}"
        );
    }
    write_frame(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "exit", "params": null}),
    );
    // stdin stays OPEN — that is the point.
    let status = exit_within(&mut child, Duration::from_secs(5));
    let _ = disarm.send(());
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    status
}

/// LSP 3.17 §exit: "The server should exit with success code 0 if the
/// shutdown request has been received before". Measured before the fix:
/// the process stayed alive after `exit` for as long as stdin was open.
#[test]
fn exit_after_shutdown_ends_the_process_with_code_0_while_stdin_stays_open() {
    let status = exit_status_with_stdin_open(true)
        .expect("the process must end on `exit` itself, not on stdin EOF");
    assert_eq!(status.code(), Some(0), "{status:?}");
}

/// LSP 3.17 §exit: "… otherwise with error code 1". Measured before the
/// fix: 0, whether or not `shutdown` had come.
#[test]
fn exit_without_shutdown_ends_the_process_with_code_1() {
    let status = exit_status_with_stdin_open(false)
        .expect("the process must end on `exit` itself, not on stdin EOF");
    assert_eq!(status.code(), Some(1), "{status:?}");
}

/// The same session, with stdin CLOSED in the same breath as `exit` — the
/// shape a client that does not wait produces, and the one the two pins
/// above deliberately avoid by holding the pipe open.
///
/// It is a RACE, so it is run until it would have been seen: the transport
/// ends on the EOF and the service decides on the `exit`, and when both are
/// ready in one poll of the same `select!` the poll order is random. Before
/// the EOF branch deferred to the protocol's recorded ending this exited 0
/// in roughly a quarter of runs — a code the specification fixes at 1.
#[test]
fn exit_then_immediate_eof_still_exits_1_every_time() {
    const RUNS: usize = 40;
    let mut codes = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nml-lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn nml-lsp");
        let disarm = arm_watchdog(&child);
        let mut stdin = child.stdin.take().expect("stdin");
        write_frame(
            &mut stdin,
            &json!({"jsonrpc": "2.0", "method": "exit", "params": null}),
        );
        // No wait, no read: the frame and the EOF reach the server together.
        drop(stdin);
        let status = child.wait().expect("wait");
        let _ = disarm.send(());
        codes.push(status.code());
    }
    let wrong = codes.iter().filter(|c| **c != Some(1)).count();
    assert_eq!(
        wrong, 0,
        "`exit` without `shutdown` must exit 1 however the client closes stdin; \
         {wrong}/{RUNS} runs did not: {codes:?}"
    );
}
