//! The session: the service every transport drives, and its ending.

use std::process::ExitCode;
use std::sync::Arc;

use tower_lsp::jsonrpc::{Request, Response};
use tower_lsp::{Client, ClientSocket, ExitedError, LspService};

use crate::server::NmlLanguageServer;

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
    #[cfg(test)]
    pub fn inner(&self) -> &NmlLanguageServer {
        self.inner.inner()
    }

    /// The session's ending, shared with the transport that drives this
    /// service: it resolves once the client has sent `exit`.
    pub fn exit_signal(&self) -> Arc<ExitSignal> {
        Arc::clone(&self.exit)
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

    /// Resolves once `exit` has been received — the native transport's wait.
    /// (The wasm pump cannot await between frames; it polls [`Self::ending`].)
    #[cfg(not(target_arch = "wasm32"))]
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
fn client_declares_diagnostic_refresh(params: &serde_json::Value) -> bool {
    params
        .pointer("/capabilities/workspace/diagnostics/refreshSupport")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
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
    }
}
