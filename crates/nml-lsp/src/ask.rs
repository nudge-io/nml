//! The ONE door every server→client REQUEST goes through — a TYPE, not a
//! spelling. [`ClientDoor`] owns the tower-lsp [`Client`] privately and
//! exposes the notifications this crate sends plus [`ClientDoor::ask`],
//! the only way a request leaves the server. Nothing outside this module
//! can reach the raw client, so nothing outside it can name a request
//! method: an alias (`let c = …`, a destructured field, a `Client` under
//! another name) has nothing to alias. The source ratchet below is the
//! second fence, over the two windows the type cannot close — the raw
//! `Client` a constructor holds for the instant before it is wrapped,
//! and this module's own methods, which hold it by design.
//!
//! A *notification* to the client costs nothing: tower-lsp hands each
//! sender its own guaranteed queue slot, so `window/logMessage` returns
//! whether or not anyone is draining. A *request* is different — it
//! `await`s the client's answer, and an answer can only arrive by READING
//! the transport. Whether that read can happen while a handler is
//! suspended is a property of the transport, not of the handler, so it is
//! decided here once instead of at each call site.

use std::fmt::Display;
use std::future::Future;

use tower_lsp::Client;
use tower_lsp::jsonrpc::{Error, ErrorCode, Result};
use tower_lsp::lsp_types::MessageType;

/// Whether this build's transport can deliver the client's answer to a
/// server→client request while the handler that sent it is suspended.
///
/// Native (`tower_lsp::Server::serve`) reads input and writes output
/// concurrently: it can. The wasm32 neutral server (RFC 0035) runs under
/// VS Code's `wasm-wasi-core`, whose stdio model is synchronous, so
/// [`crate::pump`] is a strict read→call→write loop — the request is not
/// even WRITTEN until the handler returns, and the handler is waiting for
/// its answer. It cannot, ever: the `await` never resolves, the pump never
/// reads again, and the process outlives both `exit` and stdin's EOF —
/// one leaked, unkillable server per editor session.
pub(crate) const CLIENT_ANSWERS_REQUESTS: bool = !cfg!(target_arch = "wasm32");

/// What a caller is told where [`CLIENT_ANSWERS_REQUESTS`] is false. It is
/// the literal truth of the situation and the same shape a real refusal
/// takes, so every caller's existing failure path is the right one: a
/// registration the client would not take, a refresh it will not serve.
const UNANSWERABLE: &str = "this transport cannot receive an answer to a server→client request";

/// The server's handle on its client: every message to the editor leaves
/// through here. The wrapped [`Client`] is private — a request method can
/// be called only inside [`Self::ask`], which is the whole guarantee.
#[derive(Clone)]
pub(crate) struct ClientDoor {
    client: Client,
}

impl ClientDoor {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// `window/logMessage` — a notification: queued in the sender's own
    /// slot, never waited on, so it is safe on every transport.
    pub(crate) async fn log_message<M: Display>(&self, typ: MessageType, message: M) {
        self.client.log_message(typ, message).await;
    }

    /// Send one server→client request and wait for the answer — the only
    /// way this crate sends one. `request` builds the un-awaited call from
    /// the client it is handed (`door.ask(|c| c.register_capability(..))`):
    /// where the transport cannot carry the answer the closure is never
    /// called, so nothing is sent at all and the caller gets the refusal it
    /// would get from a client that declined.
    pub(crate) async fn ask<'a, T, Fut>(
        &'a self,
        request: impl FnOnce(&'a Client) -> Fut,
    ) -> Result<T>
    where
        Fut: Future<Output = Result<T>> + 'a,
    {
        ask_over(CLIENT_ANSWERS_REQUESTS, &self.client, request).await
    }
}

/// [`ClientDoor::ask`] over an explicit answer to "can this transport
/// deliver the reply?" — the door with its gate as a parameter, so the
/// SHUT side is executable on the native lane, whose own gate is always
/// open (a door that ignored the gate passed every native test). Shut,
/// the closure is never called: nothing is sent for an answer that
/// cannot arrive.
async fn ask_over<'a, T, Fut>(
    answers: bool,
    client: &'a Client,
    request: impl FnOnce(&'a Client) -> Fut,
) -> Result<T>
where
    Fut: Future<Output = Result<T>> + 'a,
{
    if !answers {
        return Err(Error {
            code: ErrorCode::InternalError,
            message: UNANSWERABLE.into(),
            data: None,
        });
    }
    request(client).await
}

#[cfg(test)]
mod tests {
    use nml_validate::test_support::scan::{blank_comments_and_strings, sources};
    use tower_lsp::Client;
    use tower_lsp::jsonrpc::{ErrorCode, Result};

    /// Every REQUEST method of tower-lsp's `Client` (0.20.0, pinned by
    /// `Cargo.lock`): each awaits an answer, so each may be called only
    /// inside [`super::ClientDoor::ask`]. Everything else on `Client` is a
    /// notification (`log_message`, `show_message`, `publish_diagnostics`,
    /// `send_notification`, `telemetry_event`, `log_trace`), which no
    /// transport has to answer. A tower-lsp bump revisits this list.
    const REQUEST_METHODS: &[&str] = &[
        "register_capability",
        "unregister_capability",
        "show_message_request",
        "show_document",
        "code_lens_refresh",
        "semantic_tokens_refresh",
        "inline_value_refresh",
        "inlay_hint_refresh",
        "workspace_diagnostic_refresh",
        "configuration",
        "workspace_folders",
        "apply_edit",
        "send_request",
    ];

    /// Every `.rs` under this crate's `src` — this module included when
    /// `with_door` is set — read through the one shared source walker,
    /// never a second `read_dir` (`wasi_fs.rs`'s own ratchet), and rooted
    /// at THIS crate's manifest dir so a cloned target scans the tree it
    /// was built from. The METHOD ratchet scans the door too: its one
    /// legal spelling here is `request(client).await`, which names no
    /// method, so a door method that sent a request of its own outside
    /// `ask_over` is the only thing it can find here — and with the door
    /// excepted, exactly that compiled clean and passed every test. The
    /// NAMING ratchet must skip the door: the raw `Client` lives here by
    /// design.
    fn crate_sources(with_door: bool) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        sources(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            false,
            &mut out,
        );
        if !with_door {
            out.retain(|p| p.file_name().is_some_and(|n| n != "ask.rs"));
        }
        out.sort();
        out
    }

    /// The ratchet's scrubber: a source with comments and strings blanked,
    /// whitespace collapsed, and the door's own legal spelling —
    /// `.ask(|c| c.<method>(` for any binding name — blanked. Whatever
    /// request method survives is one sent outside the door, on WHATEVER
    /// receiver: `self.client.x(`, `client.x(` from a destructured field,
    /// `c.x(` on a `Client` under another name, `f().x(` on a temporary.
    fn outside_the_door(text: &str) -> String {
        let collapsed: String = blank_comments_and_strings(text)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("");
        let mut scrubbed = String::with_capacity(collapsed.len());
        let mut rest = collapsed.as_str();
        while let Some(at) = rest.find(".ask(|") {
            let (head, tail) = rest.split_at(at);
            scrubbed.push_str(head);
            let after = &tail[".ask(|".len()..];
            let legal = after.find('|').and_then(|end| {
                let name = &after[..end];
                let body = &after[end + 1..];
                let is_ident =
                    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                (is_ident && body.starts_with(name) && body[name.len()..].starts_with('.'))
                    .then(|| ".ask(|".len() + end + 1 + name.len() + 1)
            });
            match legal {
                Some(len) => {
                    scrubbed.push_str("\u{0}DOOR\u{0}");
                    rest = &tail[len..];
                }
                None => {
                    scrubbed.push_str(".ask(|");
                    rest = after;
                }
            }
        }
        scrubbed.push_str(rest);
        scrubbed
    }

    /// The request methods `scrubbed` still calls, as `.<m>(` or
    /// `.<m>::<` (a turbofish), or in UFCS form `::<m>(` / `::<m>::<`
    /// (`Alias::apply_edit(&client, ..)` — the receiver is an argument,
    /// so no `.` precedes the method).
    fn requests_in(scrubbed: &str) -> Vec<&'static str> {
        REQUEST_METHODS
            .iter()
            .copied()
            .filter(|m| {
                [".", "::"].iter().any(|sep| {
                    scrubbed.contains(&format!("{sep}{m}("))
                        || scrubbed.contains(&format!("{sep}{m}::<"))
                })
            })
            .collect()
    }

    /// Source-level ratchet: every server→client REQUEST in this crate is
    /// sent inside [`super::ClientDoor::ask`].
    ///
    /// The type already makes the ordinary path impossible (the raw
    /// `Client` is private to this module); this fence covers the window
    /// the type cannot — the `Client` a constructor holds before wrapping
    /// it — and it is keyed on the METHOD, not on how the receiver is
    /// spelled. A ratchet keyed on the field name (`.client`) stayed
    /// green while a destructured `let Self { client, .. }` and a `Client`
    /// bound to another name each sent a request outside the door: a new
    /// one added that way compiles clean and passes every native test —
    /// the native transport answers it — while hanging the wasm neutral
    /// server forever the first time a client declines to answer or
    /// cannot.
    #[test]
    fn every_server_to_client_request_goes_through_the_door() {
        let mut offenders = Vec::new();
        for path in crate_sources(true) {
            let text = std::fs::read_to_string(&path).expect("source readable");
            let sent = requests_in(&outside_the_door(&text));
            if sent.is_empty() {
                continue;
            }
            for (i, line) in text.lines().enumerate() {
                let code = blank_comments_and_strings(line);
                if sent
                    .iter()
                    .any(|m| code.contains(&format!(".{m}")) || code.contains(&format!("::{m}")))
                {
                    offenders.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "a server→client request sent outside `ClientDoor::ask` — the wasm pump can \
             never receive an answer to it and wedges the server forever:\n{}",
            offenders.join("\n")
        );
    }

    /// The scrubber itself: each bypass shape it must catch — on every
    /// receiver spelling — and the legal shapes it must not flag.
    #[test]
    fn the_ratchet_scrubber_catches_every_bypass_shape() {
        for src in [
            "self.client.register_capability(vec![r]).await",
            "self.client\n    .workspace_diagnostic_refresh()\n    .await",
            "let NmlLanguageServer { client, .. } = self; client.workspace_diagnostic_refresh()",
            "fn f(c: Client) { let _ = c.register_capability(Vec::new()); }",
            "let c = self.client.clone(); c.apply_edit(edit).await",
            "make_client().show_document(params).await",
            "self.client.send_request::<R>(params).await",
            "door.ask(|c| c.configuration(items)).await; other.workspace_folders().await",
            "door.ask(|c| d.apply_edit(e)).await",
            // A `Client` reached by reference, without `self`.
            "fn watch(client: &Client) { client.register_capability(v).await }",
            // UFCS: the receiver is an argument, the method has no `.`
            // before it, and the type is reached through a renaming `use`.
            "use tower_lsp::Client as RawClient; drop(RawClient::apply_edit(&client, e))",
            "RawClient::send_request::<R>(&client, params).await",
        ] {
            assert!(
                !requests_in(&outside_the_door(src)).is_empty(),
                "must catch: {src}"
            );
        }
        for src in [
            "self.client.ask(|c| c.register_capability(vec![r])).await",
            "self.client\n    .ask(|c| c.workspace_diagnostic_refresh())\n    .await",
            "door.ask(|client| client.send_request::<R>(params)).await",
            "self.client.log_message(MessageType::INFO, m).await",
            "// self.client.apply_edit(edit) in a comment",
            "let s = \"self.client.apply_edit(x)\";",
            // A `Client` handed around as a constructor argument, and a
            // field whose name merely starts with `client`.
            "|client| NmlLanguageServer::with_store(client, None)",
            "pub fn new(client: Client) -> Self { Self::build(client, cfg) }",
            "params.client_info.as_ref()",
        ] {
            assert!(
                requests_in(&outside_the_door(src)).is_empty(),
                "must pass: {src}"
            );
        }
    }

    /// The raw `Client` is reachable from this module alone: outside it,
    /// the type is named only where tower-lsp hands one to a constructor
    /// (a `Client` parameter, a `FnOnce(Client)` init), never held in a
    /// field, a local or a return type, and never RENAMED by a `use` —
    /// so the window the ratchet above covers is exactly those
    /// constructors.
    #[test]
    fn the_raw_client_is_named_outside_this_module_only_as_a_constructor_argument() {
        let mut offenders = Vec::new();
        for path in crate_sources(false) {
            let text = std::fs::read_to_string(&path).expect("source readable");
            for (i, line) in text.lines().enumerate() {
                let code = blank_comments_and_strings(line);
                let names_client = code
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .any(|token| token == "Client");
                if !names_client {
                    continue;
                }
                let collapsed: String = code.split_whitespace().collect();
                // A `use` may import the type, never rename it: `use
                // tower_lsp::Client as RawClient;` puts the raw client
                // under a name this fence does not look for.
                let allowed = (collapsed.starts_with("use") && !collapsed.contains("Clientas"))
                    || collapsed.contains("client:Client")
                    || collapsed.contains("FnOnce(Client)");
                if !allowed {
                    offenders.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "`tower_lsp::Client` named outside `ask.rs` other than as a constructor \
             argument — the door's guarantee rests on the raw client staying here:\n{}",
            offenders.join("\n")
        );
    }

    /// The shut door: the request is refused with the transport's own
    /// sentence and the closure is NEVER called — nothing is sent for an
    /// answer that cannot arrive. (The gate itself is target-conditional
    /// and pinned below; this is the door's READING of it, which no native
    /// test could observe before: a door that ignored its gate passed
    /// every native test.) The open side is the harness's own
    /// `client/registerCapability` exchange.
    #[tokio::test]
    async fn the_shut_door_refuses_without_calling_the_request() {
        let mut held: Option<Client> = None;
        let (_service, _socket) = tower_lsp::LspService::new(|client| {
            held = Some(client.clone());
            crate::server::NmlLanguageServer::with_store(client, None)
        });
        let client = held.expect("the init closure ran");
        let refused: Result<()> = super::ask_over(false, &client, |_| async {
            panic!("a shut door must not build the request")
        })
        .await;
        let err = refused.expect_err("the shut door refuses");
        assert_eq!(err.code, ErrorCode::InternalError);
        assert_eq!(err.message, super::UNANSWERABLE);
    }

    /// The gate the door reads is a real gate: true where the transport
    /// answers (every native build, this test included), false on the
    /// wasm32 neutral server.
    ///
    /// The second half is a SOURCE assertion, and has to be: the native
    /// test lane cannot observe a `cfg!` that is only false on wasm32, so
    /// hard-coding the gate open would pass every test this crate can run
    /// while the neutral server hangs on its first refresh. The spelling
    /// is therefore what is pinned; `cargo check -p nml-lsp --target
    /// wasm32-wasip1` is the other half of the evidence.
    #[test]
    fn the_gate_is_open_natively_and_shut_on_wasm() {
        assert_eq!(
            super::CLIENT_ANSWERS_REQUESTS,
            !cfg!(target_arch = "wasm32")
        );
        const { assert!(super::CLIENT_ANSWERS_REQUESTS, "this test lane is native") };
        let own = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ask.rs"),
        )
        .expect("this module is readable");
        let declaration = own
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("pub(crate) const CLIENT_ANSWERS_REQUESTS"))
            .expect("the gate is declared here");
        assert_eq!(
            declaration,
            "pub(crate) const CLIENT_ANSWERS_REQUESTS: bool = !cfg!(target_arch = \"wasm32\");",
            "the gate must stay target-conditional, not a constant"
        );
        // The door hands THAT gate to `ask_over` — the other half of the
        // same blindness: a door passing `true` is `ask_over(false, ..)`
        // never reached in production, and no native test can tell.
        let door_line = "ask_over(CLIENT_ANSWERS_REQUESTS, &self.client, request).await";
        assert_eq!(
            own.lines()
                .map(str::trim)
                .filter(|l| *l == door_line)
                .count(),
            1,
            "`ClientDoor::ask` must hand the gate constant to `ask_over`"
        );
    }
}
