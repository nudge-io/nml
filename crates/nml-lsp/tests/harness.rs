//! In-process duplex test harness for the NML language server (RFC 0030 P1).
//!
//! No stdio, no editor: `LspService` implements `tower::Service`, so the
//! tests drive it with raw JSON-RPC `Request` values
//! (`service.ready().await.call(req)`) and read server→client traffic
//! (window/logMessage, client/registerCapability) off the `ClientSocket`,
//! which is a `Stream` of `Request` frames — exactly tower-lsp's own testing
//! style, but against the real `NmlLanguageServer` with a real (tempdir)
//! schema-package store injected through `NmlLanguageServer::with_store`.
//! Diagnostics are PULLED (`textDocument/diagnostic`), not read off the
//! socket — see [`Harness::diagnostics`].

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tower::{Service, ServiceExt};
use tower_lsp::ClientSocket;
use tower_lsp::jsonrpc::{Request, Response};
use tower_lsp::lsp_types::Url;

use nml_lsp::test_support::{MAX_INDEX_BYTES, NmlLanguageServer, SERVER_NAME};
use nml_validate::package::SchemaPackage;
use nml_validate::store::Store;
use nml_validate::test_support::{
    DEMO_MANIFEST, DEMO_MANIFEST_WITH_DIRECTIVES, demo_package, publish_demo,
};

/// Generous slack for a server→client notification. Store-health
/// `window/logMessage`s are emitted during the diagnostic-pull handler
/// (`drain_store_events`), so after a pull they are already queued; this
/// bound only guards against a hang, never a busy-wait.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// The in-process server plus both directions of its wire.
struct Harness {
    service: nml_lsp::test_support::NmlService,
    socket: ClientSocket,
    /// Server→client notifications drained off the socket but not yet
    /// consumed by an assertion, in arrival order. Server→client *requests*
    /// never land here — they are auto-acknowledged in [`Self::route`],
    /// their methods recorded in [`Self::requests`].
    inbox: VecDeque<Request>,
    /// Every server→client REQUEST's method, in arrival order.
    requests: Vec<String>,
    /// Answer `client/registerCapability` with an ERROR, as a client that
    /// cannot serve a dynamic registration does. Everything else is still
    /// acknowledged.
    refuse_registration: bool,
    next_id: i64,
}

impl Harness {
    /// Build the service through the same `nml_lsp::test_support::build_service`
    /// owner the binary uses — so `nml/schemaInfo` (and every future custom
    /// method) is exercised through the real JSON-RPC route — but with the
    /// resolver's store injected.
    fn new(store: Store) -> Self {
        let (service, socket) = nml_lsp::test_support::build_service(|client| {
            NmlLanguageServer::with_store(client, Some(store))
        });
        Self {
            service,
            socket,
            inbox: VecDeque::new(),
            requests: Vec::new(),
            refuse_registration: false,
            next_id: 0,
        }
    }

    /// Build a *provider* service (RFC 0035 in-binary channel) — the `nudge
    /// lsp` wiring: the tool's package injected in-process, plus a store (here
    /// a tempdir, so coverage must come from the injected package, not the
    /// cache). Exercises `NmlLanguageServer::with_provider` through the same
    /// service builder the tool binary uses.
    fn new_provider(package: nml_validate::package::SchemaPackage, store: Store) -> Self {
        let (service, socket) = nml_lsp::test_support::build_service(move |client| {
            NmlLanguageServer::with_provider(client, package, Some(store))
        });
        Self {
            service,
            socket,
            inbox: VecDeque::new(),
            requests: Vec::new(),
            refuse_registration: false,
            next_id: 0,
        }
    }

    /// Send one JSON-RPC message and drive the socket concurrently until the
    /// call resolves.
    ///
    /// The concurrent drain is load-bearing, not an optimization: handlers
    /// can send server→client *requests* and await the reply mid-handler
    /// (`initialized` awaits `client/registerCapability`), so awaiting the
    /// call without simultaneously answering the socket would deadlock.
    async fn call_raw(&mut self, req: Request) -> Option<Response> {
        let call = self
            .service
            .ready()
            .await
            .expect("language server exited")
            .call(req);
        tokio::pin!(call);
        loop {
            tokio::select! {
                result = &mut call => return result.expect("language server exited"),
                frame = self.socket.next() => {
                    self.route(frame.expect("client socket closed")).await;
                }
            }
        }
    }

    /// File one server→client frame: requests are acknowledged with a
    /// success reply through the socket's `Sink` half (the tests have no
    /// client-side capability machinery worth simulating — the handlers only
    /// need *a* reply to make progress); notifications are queued for
    /// assertions.
    async fn route(&mut self, frame: Request) {
        match frame.id().cloned() {
            Some(id) => {
                self.requests.push(frame.method().to_string());
                let reply =
                    if self.refuse_registration && frame.method() == "client/registerCapability" {
                        Response::from_error(id, tower_lsp::jsonrpc::Error::internal_error())
                    } else {
                        Response::from_ok(id, Value::Null)
                    };
                self.socket.send(reply).await.expect("client socket closed")
            }
            None => self.inbox.push_back(frame),
        }
    }

    /// How many `workspace/diagnostic/refresh` requests the server has sent.
    fn refreshes(&self) -> usize {
        self.requests
            .iter()
            .filter(|m| m.as_str() == "workspace/diagnostic/refresh")
            .count()
    }

    /// `initialize` as a client that declares it can take a dynamic
    /// file-watch registration (`workspace.didChangeWatchedFiles
    /// .dynamicRegistration`, LSP 3.17) — VS Code does. Such a client's
    /// events are the freshness contract, so the server stats nothing.
    async fn initialize_watching(&mut self, root: &Path) {
        self.request(
            "initialize",
            json!({
                "capabilities": {
                    "workspace": {
                        "didChangeWatchedFiles": { "dynamicRegistration": true }
                    }
                },
                "rootUri": file_uri(root),
            }),
        )
        .await;
        self.notify("initialized", json!({})).await;
    }

    /// `initialize` as VS Code with pull diagnostics: `documentChanges`,
    /// the `create` operation, and `workspace.diagnostics.refreshSupport`
    /// spelled as LSP 3.17 and vscode-languageclient spell it (the key
    /// lsp-types 0.94.1 cannot deserialize), then `initialized`.
    async fn initialize_as_vscode_with_refresh(&mut self, root: &Path) {
        self.initialize_with_refresh_key(root, "diagnostics").await;
    }

    /// `initialize` declaring the refresh capability under lsp-types'
    /// own key, `workspace.diagnostic.refreshSupport` — what a client
    /// generated from that crate sends.
    async fn initialize_as_lsp_types_client_with_refresh(&mut self, root: &Path) {
        self.initialize_with_refresh_key(root, "diagnostic").await;
    }

    async fn initialize_with_refresh_key(&mut self, root: &Path, key: &str) {
        self.request(
            "initialize",
            json!({
                "capabilities": {
                    "workspace": {
                        "workspaceEdit": {
                            "documentChanges": true,
                            "resourceOperations": ["create", "rename", "delete"],
                            "failureHandling": "textOnlyTransactional",
                        },
                        key: { "refreshSupport": true },
                    },
                },
                "rootUri": file_uri(root),
            }),
        )
        .await;
        self.notify("initialized", json!({})).await;
    }

    /// JSON-RPC request: returns the `result` payload, panics on an `error`
    /// reply (no test here expects one).
    async fn request(&mut self, method: &'static str, params: Value) -> Value {
        self.next_id += 1;
        let req = Request::build(method)
            .params(params)
            .id(self.next_id)
            .finish();
        let response = self
            .call_raw(req)
            .await
            .expect("a request always yields a response");
        let (_, result) = response.into_parts();
        result.unwrap_or_else(|e| panic!("{method} returned a JSON-RPC error: {e}"))
    }

    /// JSON-RPC notification: no response by definition.
    async fn notify(&mut self, method: &'static str, params: Value) {
        let req = Request::build(method).params(params).finish();
        let response = self.call_raw(req).await;
        assert!(response.is_none(), "notification produced a response");
    }

    /// `initialize` (rootUri = `root`) followed by `initialized`.
    async fn initialize(&mut self, root: &Path) {
        self.initialize_with_options(root, Value::Null).await;
    }

    /// [`Self::initialize`] with client `initializationOptions` — how a test
    /// declares client-side registrations (e.g. RFC 0010 tier 2's
    /// `explainCommand`) exactly as a real client would.
    async fn initialize_with_options(&mut self, root: &Path, options: Value) {
        let mut params = json!({ "capabilities": {}, "rootUri": file_uri(root) });
        if !options.is_null() {
            params["initializationOptions"] = options;
        }
        self.request("initialize", params).await;
        self.notify("initialized", json!({})).await;
    }

    /// [`Self::initialize`] declaring what the VS Code client library
    /// declares (`vscode-languageclient` 10, `client.js`):
    /// `workspaceEdit.documentChanges` and the three resource operations —
    /// the capabilities a versioned edit and a created file are negotiated
    /// under. [`Self::initialize`] declares nothing: the plain-`changes`
    /// client every edit must also fit.
    async fn initialize_as_vscode(&mut self, root: &Path) {
        self.request(
            "initialize",
            json!({
                "capabilities": { "workspace": { "workspaceEdit": {
                    "documentChanges": true,
                    "resourceOperations": ["create", "rename", "delete"],
                    "failureHandling": "textOnlyTransactional",
                } } },
                "rootUri": file_uri(root),
            }),
        )
        .await;
        self.notify("initialized", json!({})).await;
    }

    /// `initialize` with NO workspace folder (`rootUri: null`, no
    /// `workspaceFolders`) — VS Code's single-file mode — then
    /// `initialized`.
    async fn initialize_folderless(&mut self) {
        self.request("initialize", json!({ "capabilities": {}, "rootUri": null }))
            .await;
        self.notify("initialized", json!({})).await;
    }

    /// `workspace/didChangeWorkspaceFolders` with folders added and removed.
    async fn change_folders(&mut self, added: &[&Path], removed: &[&Path]) {
        let folder = |p: &&Path| json!({ "uri": file_uri(p), "name": p.file_name().unwrap().to_string_lossy() });
        self.notify(
            "workspace/didChangeWorkspaceFolders",
            json!({ "event": {
                "added": added.iter().map(folder).collect::<Vec<_>>(),
                "removed": removed.iter().map(folder).collect::<Vec<_>>(),
            }}),
        )
        .await;
    }

    /// The `initialize` request ALONE — the handshake leg, with the
    /// `initialized` notification withheld. Only the workspace-index
    /// scheduling pin uses this: every other test wants a fully initialized
    /// server and calls [`Self::initialize`].
    async fn handshake_only(&mut self, root: &Path) {
        self.request(
            "initialize",
            json!({ "capabilities": {}, "rootUri": file_uri(root) }),
        )
        .await;
    }

    /// `textDocument/didOpen` followed by a diagnostics PULL — RFC 0035: the
    /// server no longer pushes `publishDiagnostics`; the client requests a
    /// document's diagnostics. Returns the report normalized to
    /// `{"uri", "diagnostics": [...]}`, so assertions read a full report's
    /// `items` exactly as they read the old publish params' `diagnostics`.
    ///
    /// This is strictly MORE deterministic than the old push assert: a request
    /// yields its response synchronously, with no notification to race.
    async fn open(&mut self, path: &Path, text: &str) -> Value {
        let uri = file_uri(path);
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "nml",
                    "version": 1,
                    "text": text,
                }
            }),
        )
        .await;
        self.diagnostics(&uri).await
    }

    /// Pull a document's diagnostics (`textDocument/diagnostic`), normalized to
    /// `{"uri", "diagnostics": [...]}`. Tests never send a `previousResultId`,
    /// so the server always returns a full report (never `Unchanged`). This is
    /// also how a test asserts cross-file / out-of-band healing under the pull
    /// model: re-pull an already-open document after the upstream change.
    async fn diagnostics(&mut self, uri: &str) -> Value {
        let report = self
            .request(
                "textDocument/diagnostic",
                json!({ "textDocument": { "uri": uri } }),
            )
            .await;
        assert_eq!(
            report["kind"], "full",
            "test pulls always expect a full report: {report}"
        );
        json!({
            "uri": uri,
            "diagnostics": report.get("items").cloned().unwrap_or_else(|| json!([])),
        })
    }

    /// Next server→client notification with the given method (already-queued
    /// frames first, then the live socket), timeout-bounded. Returns its
    /// params.
    async fn next_from_client(&mut self, method: &str, timeout: Duration) -> Value {
        let wait = async {
            loop {
                if let Some(position) = self.inbox.iter().position(|frame| frame.method() == method)
                {
                    let frame = self.inbox.remove(position).expect("position just found");
                    return frame.params().cloned().unwrap_or(Value::Null);
                }
                let frame = self.socket.next().await.expect("client socket closed");
                self.route(frame).await;
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for a {method} notification"))
    }
}

fn file_uri(path: &Path) -> String {
    Url::from_file_path(path)
        .expect("absolute path")
        .to_string()
}

/// Matches the server's derived-root log spelling: canonical, forward
/// slashes, no Windows `\\?\` prefix.
fn message_path(path: &Path) -> String {
    let path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    strip_extended_prefix(&path)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// Windows extended-path spellings (`\\?\`, `\\?\UNC\`) are for syscalls;
/// a log line spells the path without them. Everywhere else the path is
/// already the spelling — one function, no per-platform rebinding.
fn strip_extended_prefix(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.as_os_str().to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// A test's scratch directory, removed when the guard drops — on a red
/// assertion too (thousands of `nml-lsp-harness-*` directories stood in
/// `$TMPDIR` after rounds 80–84). Derefs to its path.
struct Scratch(PathBuf);

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Fresh scratch dir per test, canonicalized because the server
/// canonicalizes workspace roots and document paths (macOS `/var` →
/// `/private/var`); the URIs the test sends must agree byte-for-byte with
/// the URIs the server publishes back. Guard-owned: removed on drop.
fn temp_dir(tag: &str) -> Scratch {
    // pid + process-wide counter: pid alone collides when a re-used pid (or
    // a same-process re-entry) hits the same tag.
    static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "nml-lsp-harness-{tag}-{}-{nonce}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    Scratch(dunce::canonicalize(&dir).expect("canonicalize scratch dir"))
}

/// A workspace whose `nml-project.nml` pins the demo package. The store
/// lives in a sibling dir, NOT under the workspace root: workspace indexing
/// sweeps `**/*.nml`, and the store's own manifest/model files must not leak
/// in as workspace documents.
fn demo_workspace(base: &Path) -> PathBuf {
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    fs::write(
        ws.join("nml-project.nml"),
        "project P:\n    schemaPackages:\n        - demo\n",
    )
    .expect("write project file");
    ws
}

/// `initialize` answers before the workspace is read: indexing runs in the
/// `initialized` handler, after the handshake. This is what makes a fixed
/// start budget in the editor safe — the budget bounds the handshake, and
/// the handshake's cost does not grow with the workspace. Pinned by ORDER,
/// not by timing: the server cannot say "indexed" before it has indexed.
#[tokio::test]
async fn indexing_happens_after_initialize_not_inside_it() {
    let base = temp_dir("index-after-initialize");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = demo_workspace(&base);
    let mut harness = Harness::new(Store::at(&store_base));

    harness
        .request(
            "initialize",
            json!({ "capabilities": {}, "rootUri": file_uri(&ws) }),
        )
        .await;
    let said_during_initialize: Vec<String> = harness
        .inbox
        .iter()
        .filter(|f| f.method() == "window/logMessage")
        .filter_map(|f| {
            f.params()
                .and_then(|p| p["message"].as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        said_during_initialize
            .iter()
            .all(|m| !m.contains("indexed")),
        "the workspace was indexed INSIDE initialize: {said_during_initialize:?}"
    );

    harness.notify("initialized", json!({})).await;
    let mut seen = Vec::new();
    for _ in 0..8 {
        let params = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        let message = params["message"].as_str().unwrap_or_default().to_string();
        if message.contains("indexed") {
            return;
        }
        seen.push(message);
    }
    panic!("no indexing log after initialized; saw {seen:?}");
}

/// LSP 3.17 §initialize — the handshake identifies the server. A client
/// that spawned a project-declared `<tool> lsp` (RFC 0035's in-binary
/// channel) has, at that moment, run a binary chosen by a name in a
/// repository file; `serverInfo.name` is the first thing it can check that
/// only a real NML language server can answer. The embed model makes the
/// name the PROTOCOL implementation's — `nudge lsp` is `nml_lsp::serve`, so
/// it answers `nml-lsp` too, and the assertion below proves exactly that by
/// running BOTH flavors through the same handshake.
///
/// Pinned here because the extension's provider handshake
/// (`editors/vscode/src/providerTrust.ts`, `judgeServerIdentity`) accepts
/// only this name: a different one stops the provider and withdraws the
/// operator's approval, and a missing one stops it for the session. A
/// server that stopped sending `serverInfo` would turn every provider launch
/// into that failure.
#[tokio::test]
async fn the_handshake_identifies_the_server_for_both_flavors() {
    let base = temp_dir("server-info");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create ws");

    for (flavor, mut harness) in [
        ("neutral", Harness::new(Store::at(&store_base))),
        (
            "provider",
            Harness::new_provider(demo_package(), Store::at(&store_base)),
        ),
    ] {
        let result = harness
            .request("initialize", json!({ "capabilities": {} }))
            .await;
        assert_eq!(
            result["serverInfo"]["name"],
            json!(SERVER_NAME),
            "{flavor}: serverInfo.name must name the NML server: {result}"
        );
        let version = result["serverInfo"]["version"]
            .as_str()
            .unwrap_or_else(|| panic!("{flavor}: serverInfo.version is a string: {result}"));
        assert!(
            version
                .split('.')
                .next()
                .is_some_and(|major| major.chars().all(|c| c.is_ascii_digit())),
            "{flavor}: serverInfo.version is a semver-shaped crate version, got {version}"
        );
        harness.notify("initialized", json!({})).await;
    }
}

/// TEST A — notifier end-to-end. A corrupt store entry (`current` pointer
/// naming a slot that does not exist, with a wrong hash) must surface as a
/// `window/logMessage` warning: pin resolution fails →
/// `PackageResolver::load_store_package` emits a `StoreEvent` → the notifier
/// task spawned at `initialize` logs it. The wait is timeout-bounded because
/// that last hop crosses a task boundary — unlike diagnostics, log ordering
/// against the didOpen call is *not* guaranteed.
#[tokio::test]
async fn corrupt_store_entry_surfaces_as_log_message_warning() {
    let base = temp_dir("corrupt-store");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    publish_demo(&Store::at(&store_base));
    // Corrupt the pointer through the layout the store contract pins:
    // well-formed (two lines, blake3-prefixed hash) so it passes pointer
    // parsing, but naming a slot that was never written — the load fails,
    // not the parse.
    fs::write(
        store_base.join("schema-packages/demo/current"),
        "0.1.0+bad00000\nblake3:wrong\n",
    )
    .expect("corrupt the current pointer");

    let ws = demo_workspace(&base);
    fs::write(ws.join("x.nml"), "").expect("write x.nml");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&ws.join("x.nml"), "").await;

    // Other logMessages exist (e.g. "NML language server initialized"), so
    // scan until the store-failure one arrives; the per-wait timeout bounds
    // the scan because the server emits finitely many frames here.
    loop {
        let params = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        let message = params["message"]
            .as_str()
            .expect("logMessage has a message");
        if message.contains("failed to load") {
            assert!(
                message.contains("the package binds nothing until then"),
                "degraded wording missing from: {message}"
            );
            // MessageType::WARNING = 2 — the event is a degradation, not info.
            assert_eq!(params["type"], json!(2), "expected a warning: {params}");
            return;
        }
    }
}

/// TEST B — `nml/schemaInfo` smoke over a healthy store: a file matched by
/// the demo package's binding globs (`demo.nml`, which is also its root
/// marker) reports bound=true from "store current" via the project pin,
/// note-free. Also proves didOpen published diagnostics for the file (the
/// file is empty, so the *content* of the diagnostics is not asserted —
/// the publish itself is, via `open`'s built-in determinism assert).
#[tokio::test]
async fn schema_info_reports_pinned_store_binding() {
    let base = temp_dir("schema-info");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    publish_demo(&Store::at(&store_base));

    let ws = demo_workspace(&base);
    let file = ws.join("demo.nml");
    fs::write(&file, "").expect("write demo.nml");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let diagnostics = harness.open(&file, "").await;
    assert!(
        diagnostics["diagnostics"].is_array(),
        "publishDiagnostics params carry a diagnostics array: {diagnostics}"
    );

    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&file) }))
        .await;
    assert_eq!(info["bound"], json!(true), "unbound: {info}");
    assert_eq!(info["package"], json!("demo"), "wrong package: {info}");
    assert_eq!(
        info["source"],
        json!("store current"),
        "wrong source: {info}"
    );
    // The project file pins demo, so binding must report the pin step —
    // not auto-association (which would also match here via rootMarkers).
    assert_eq!(info["step"], json!("pinned"), "wrong step: {info}");
    assert_eq!(info["notes"], json!([]), "expected a note-free binding");
}

/// r73 — the `nml/schemaInfo` BOUND payload's wire types, pinned field by
/// field. Every field here is read by a shipped VS Code extension whose
/// version floats free of the server's: the extension is published at 0.4.0
/// against crates at 0.1.0, and two of the three rungs of the RFC 0035
/// discovery ladder hand the extension a SEPARATELY released server (the
/// `<tool> lsp` provider binary; a `nml.server.path` / `~/.cargo/bin` native
/// build — `editors/vscode/INSTALL.md` builds the two in independent steps).
/// Only the bundled-WASM rung ships them together.
///
/// The extension's parser (`editors/vscode/src/contracts/schemaInfo.ts`)
/// requires each of these by TYPE and returns `undefined` for the WHOLE
/// payload if any one is wrong — which renders as a plain green
/// `$(check) nml` with no package, no hash, and **no warning background**,
/// i.e. a degraded binding reported as healthy. So this surface grows only by
/// ADDING keys: an added key is ignored by every older client; a retyped or
/// renamed one silently blinds it. `docs/stability.md` classes the LSP wire
/// shape as versioned — breaking is a minor-release event, never a quiet one.
#[tokio::test]
async fn schema_info_bound_wire_types_grow_only_by_addition() {
    let base = temp_dir("schema-info-wire");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    publish_demo(&Store::at(&store_base));

    let ws = demo_workspace(&base);
    let file = ws.join("demo.nml");
    fs::write(&file, "").expect("write demo.nml");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&file, "").await;
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&file) }))
        .await;

    assert_eq!(info["bound"], json!(true), "not bound: {info}");
    for key in [
        "package",
        "version",
        "contentHash",
        // `binding` is the binding's NAME, a string. A kernel binding report
        // is a NEW key beside it, never a retype of this one.
        "binding",
        "source",
        "step",
        "root",
    ] {
        assert!(
            info[key].is_string(),
            "schemaInfo.{key} must stay a string: {info}"
        );
    }
    assert!(
        info["shadowsStore"].is_boolean(),
        "schemaInfo.shadowsStore must stay a boolean: {info}"
    );
    // The universe word — the kernel's own, the `--json` `binding`
    // row's — is a CLOSED vocabulary the extension's parser matches
    // exactly (an unknown word is dropped, never rendered). Two words,
    // both pinned here, so a third arrives as a reviewed change on both
    // sides and never as a field the editor silently ignores.
    assert!(
        info["universe"] == json!("open") || info["universe"] == json!("closed"),
        "schemaInfo.universe must stay one of the parsed words: {info}"
    );
    let actions = info["actions"]
        .as_array()
        .unwrap_or_else(|| panic!("schemaInfo.actions must stay an array: {info}"));
    assert!(
        actions.iter().all(serde_json::Value::is_string),
        "schemaInfo.actions must stay an array of strings: {info}"
    );
    let notes = info["notes"]
        .as_array()
        .unwrap_or_else(|| panic!("schemaInfo.notes must stay an array: {info}"));
    for note in notes {
        assert!(
            note["message"].is_string(),
            "schemaInfo.notes[].message must stay a string: {info}"
        );
        assert!(
            note["severity"] == json!("error")
                || note["severity"] == json!("warning")
                || note["severity"] == json!("info"),
            "schemaInfo.notes[].severity must stay one of the parsed words: {info}"
        );
    }
}

/// TEST B2 — the in-binary channel end-to-end (RFC 0035): a *provider* server
/// (embedded package injected in-binary, EMPTY store) validates an opened file
/// through the real didOpen → validate → publish route, and the diagnostic's
/// identity suffix names the `in-binary` source. This is the `nudge lsp`
/// scenario minus the tool binary — the committed regression test behind the
/// hand-driven stdio smoke.
#[tokio::test]
async fn injected_provider_validates_open_file_with_empty_store() {
    let base = temp_dir("provider-in-binary");
    let store_base = base.join("store"); // created, never published to
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    // `demo.nml` is a demo-package binding glob AND its root marker, so the
    // file binds under its own directory with no nml-project.nml.
    let demo_nml = ws.join("demo.nml");
    let text = "core Main:\n    name = \"x\"\n    bogus = 1\n";
    fs::write(&demo_nml, text).expect("write demo.nml");

    let mut harness = Harness::new_provider(demo_package(), Store::at(&store_base));
    harness.initialize(&ws).await;
    let params = harness.open(&demo_nml, text).await;
    let diags = params["diagnostics"].as_array().expect("diagnostics array");
    assert!(
        diags.iter().any(|d| {
            let m = d["message"].as_str().unwrap_or("");
            m.contains("bogus") && m.contains("in-binary")
        }),
        "expected an in-binary-sourced strict-unknown-key diagnostic; got {diags:?}"
    );
}

/// A workspace holding the directive-vocabulary demo package as a WORKSPACE
/// manifest (the authoring path): `demo.package.nml` + the model source it
/// declares. The store stays empty — coverage must come from the manifest.
fn directive_workspace(base: &Path, model_text: &str) -> (PathBuf, PathBuf) {
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    fs::write(ws.join("demo.package.nml"), DEMO_MANIFEST_WITH_DIRECTIVES).expect("write manifest");
    let model = ws.join("core.model.nml");
    fs::write(&model, model_text).expect("write model source");
    (ws, model)
}

/// NML2054 in the editor: opening the declared schema source that carries
/// an arm field named like the discriminator publishes the ERROR at the
/// field (its content, not its indentation), the union's declaration as
/// `relatedInformation`, and the deletion as the structured suggestion;
/// the code action on the row is the one resolver's "Delete this field
/// definition" — the field's row as one edit, never preferred — and the
/// buffer re-published with it applied carries no NML2054. A required
/// seal's action is the `?` insertion at the type's end.
#[tokio::test]
async fn a_shadowed_discriminator_offers_its_deletion_as_a_quick_fix_on_the_model_buffer() {
    let base = temp_dir("shadow-quickfix");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let text = "model core:\n    // the entry's kind\n    kind string?\n    msg string?\n\n\
                oneof record by kind:\n    \"log\" -> core\n";
    let (ws, model) = directive_workspace(&base, text);
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&model, text).await;
    assert_eq!(codes_of(&report), ["NML2054"], "{report}");
    let diag = report["diagnostics"][0].clone();
    assert_eq!(diag["severity"], json!(1), "an error: {diag}");
    assert_eq!(
        diag["range"],
        range(2, 4, 16),
        "at the field's content: {diag}"
    );
    let related = &diag["relatedInformation"][0];
    assert_eq!(
        related["message"],
        json!("oneof 'record' selects its arm by 'kind' here"),
        "{diag}"
    );
    assert_eq!(
        related["location"]["range"]["start"],
        json!({ "line": 5, "character": 0 }),
        "{diag}"
    );
    assert_eq!(
        diag["data"]["suggestions"][0]["kind"],
        json!("delete"),
        "{diag}"
    );
    let result = one_code_action(&mut harness, &model, diag.clone()).await;
    let actions: Vec<Value> = result.as_array().cloned().unwrap_or_default();
    let action = actions
        .iter()
        .find(|a| a["title"] == json!("Delete this field definition"))
        .unwrap_or_else(|| panic!("no deletion action in {result}"));
    assert_eq!(action["kind"], json!("quickfix"), "{action}");
    assert!(
        action["isPreferred"].is_null(),
        "a structural edit is never auto-applied: {action}"
    );
    let edits = action["edit"]["changes"][file_uri(&model)]
        .as_array()
        .expect("workspace edit")
        .clone();
    assert_eq!(edits.len(), 1, "{edits:?}");
    assert_eq!(
        edits[0]["range"],
        json!({ "start": { "line": 2, "character": 0 }, "end": { "line": 3, "character": 0 } }),
        "the field's row: {edits:?}"
    );
    assert_eq!(edits[0]["newText"], json!(""), "{edits:?}");
    // Applied: the comment stays, the finding is gone.
    let fixed = text.replacen("    kind string?\n", "", 1);
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&model), "version": 2 },
                "contentChanges": [{ "text": fixed }],
            }),
        )
        .await;
    let report = harness.diagnostics(&file_uri(&model)).await;
    assert!(codes_of(&report).is_empty(), "loads clean: {report}");
    // A required seal: the `?` rides as a verbatim fix at the type's end.
    let sealed = "model core:\n    kind string #sealed\n    msg string?\n\noneof record by kind:\n    \
                  \"log\" -> core\n";
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&model), "version": 3 },
                "contentChanges": [{ "text": sealed }],
            }),
        )
        .await;
    let report = harness.diagnostics(&file_uri(&model)).await;
    // `#sealed` is the language's (RFC 0019): known under the demo package's
    // declared vocabulary, never an unknown directive.
    assert!(
        !codes_of(&report).iter().any(|c| c == "NML5000"),
        "a language directive is never unknown: {report}"
    );
    let diag = report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .find(|d| d["code"] == json!("NML2054"))
        .cloned()
        .unwrap_or_else(|| panic!("no NML2054 in {report}"));
    assert_eq!(
        diag["data"]["suggestions"][0]["kind"],
        json!("fix"),
        "{diag}"
    );
    let result = one_code_action(&mut harness, &model, diag.clone()).await;
    let actions: Vec<Value> = result.as_array().cloned().unwrap_or_default();
    let action = actions
        .iter()
        .find(|a| a["title"] == json!("Insert `?`"))
        .unwrap_or_else(|| panic!("no `Insert `?`` action in {result}"));
    let edits = action["edit"]["changes"][file_uri(&model)]
        .as_array()
        .expect("workspace edit")
        .clone();
    assert_eq!(edits.len(), 1, "{edits:?}");
    assert_eq!(
        edits[0]["range"],
        range(1, 15, 15),
        "after `string`: {edits:?}"
    );
    assert_eq!(edits[0]["newText"], json!("?"), "{edits:?}");
}

/// A file governed by a binding whose declared source carries the NML2054
/// shape is refused with NML2091 — its `relatedInformation` the NML2054
/// finding at the field in the SOURCE's own file — the CLI's row and note;
/// the source's own remedy rides the row too (`data.suggestions[0].source`
/// names the source), so the quick fix offered on the governed document
/// edits the SOURCE, titled with the file it changes (RFC 0026 B-24).
#[tokio::test]
async fn a_bound_instance_under_a_shadowed_source_is_refused_with_the_cause_as_related() {
    let base = temp_dir("shadow-bound");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let text = "model core:\n    kind string?\n\noneof record by kind:\n    \"log\" -> core\n";
    let (ws, model) = directive_workspace(&base, text);
    let app = ws.join("demo.nml");
    let app_text = "core main:\n    kind = \"log\"\n";
    fs::write(&app, app_text).expect("write app");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&app, app_text).await;
    assert_eq!(codes_of(&report), ["NML2091"], "{report}");
    let diag = &report["diagnostics"][0];
    let related = &diag["relatedInformation"][0];
    assert_eq!(
        related["location"]["uri"],
        json!(file_uri(&model)),
        "{diag}"
    );
    assert_eq!(related["location"]["range"], range(1, 4, 16), "{diag}");
    assert!(
        related["message"]
            .as_str()
            .is_some_and(|m| m
                .starts_with("oneof 'record' arm \"log\": model 'core' declares a field 'kind'")),
        "{diag}"
    );
    let suggestion = &diag["data"]["suggestions"][0];
    assert_eq!(suggestion["kind"], json!("delete"), "{diag}");
    assert_eq!(suggestion["source"], json!("core.model.nml"), "{diag}");
    let actions = one_code_action(&mut harness, &app, diag.clone()).await;
    let actions = actions.as_array().expect("actions");
    let fixes: Vec<&Value> = actions
        .iter()
        .filter(|a| {
            a["title"]
                .as_str()
                .is_some_and(|t| t.ends_with(" in core.model.nml"))
        })
        .collect();
    assert_eq!(
        fixes.len(),
        1,
        "one action, naming the file it changes: {actions:?}"
    );
    // This harness client declares no `workspace.workspaceEdit.
    // documentChanges`, so the server hands it the plain `changes`
    // map (the only shape it can apply) — keyed by the SOURCE's uri,
    // never the governed document's.
    let changes = &fixes[0]["edit"]["changes"];
    assert_eq!(
        changes
            .as_object()
            .map(|o| o.keys().cloned().collect::<Vec<_>>()),
        Some(vec![file_uri(&model)]),
        "the edit lands in the source, and nowhere else: {fixes:?}"
    );
    let edits = changes[file_uri(&model)].as_array().expect("edits");
    assert_eq!(edits.len(), 1, "{fixes:?}");
    assert_eq!(edits[0]["newText"], json!(""), "{fixes:?}");
}

/// TEST C — directive vocabulary end-to-end (RFC 0030/0032): opening a
/// declared schema source with a typo'd directive (`#lvie`) publishes the
/// unknown-directive error with the did-you-mean and the structured
/// suggestion, through the real didOpen → validate → publish path.
#[tokio::test]
async fn declared_model_file_gets_directive_did_you_mean() {
    let base = temp_dir("directive-vocab");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let text = "model core:\n    name string+ #lvie\n    mode string?\n";
    let (ws, model) = directive_workspace(&base, text);

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let published = harness.open(&model, text).await;
    let diags = published["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    let dym = diags
        .iter()
        .find(|d| {
            d["message"]
                .as_str()
                .is_some_and(|m| m.contains("unknown directive '#lvie'"))
        })
        .unwrap_or_else(|| panic!("no unknown-directive diagnostic in: {published}"));
    let message = dym["message"].as_str().expect("message");
    assert!(message.contains("did you mean \"#live\""), "{message}");
    assert_eq!(
        dym["data"]["suggestions"][0]["replacement"],
        json!("#live"),
        "structured suggestion must ride Diagnostic.data: {dym}"
    );
}

/// TEST D — `#` completion in a covered model file offers the vocabulary
/// (label = name, detail = arg kind, documentation = doc), and nothing else.
/// RFC 0015 end-to-end: `as`-position completion through the REAL completion
/// handler (didOpen → schema registry → completion), not the unit-tested
/// detector alone — the union's nameable variants are offered at `slot as ⌖`.
#[tokio::test]
async fn as_position_completion_offers_union_variants_end_to_end() {
    let base = temp_dir("as-completion");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model = ws.join("union.model.nml");
    let model_text = "model modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel host:\n    slot (modelA | modelB)?\n";
    fs::write(&model, model_text).expect("write model");
    let config = ws.join("app.nml");
    let config_text = "host H:\n    slot as \n";
    fs::write(&config, config_text).expect("write config");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&model, model_text).await;
    harness.open(&config, config_text).await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&config) },
                // End of `    slot as ` — the annotation type slot.
                "position": { "line": 1, "character": 12 },
            }),
        )
        .await;
    let items = result.as_array().expect("completion item array");
    let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
    assert_eq!(
        labels,
        ["modelA", "modelB"],
        "the union's nameable variants, source order: {result}"
    );
}

/// Round-10 F4: the ELEMENT-level twin — `- one as ⌖` inside `slots:` must
/// offer the enclosing list field's union variants (the item name is not a
/// field; the union lives on `slots`). Previously returned [] end-to-end.
#[tokio::test]
async fn as_position_completion_works_on_list_elements_end_to_end() {
    let base = temp_dir("as-completion-element");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model = ws.join("union.model.nml");
    let model_text = "model modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel host:\n    slots [](modelA | modelB)?\n";
    fs::write(&model, model_text).expect("write model");
    let config = ws.join("app.nml");
    let config_text = "host H:\n    slots:\n        - one as \n";
    fs::write(&config, config_text).expect("write config");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&model, model_text).await;
    harness.open(&config, config_text).await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&config) },
                // End of `        - one as ` — the item's annotation slot.
                "position": { "line": 2, "character": 17 },
            }),
        )
        .await;
    let items = result.as_array().expect("completion item array");
    let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
    assert_eq!(
        labels,
        ["modelA", "modelB"],
        "the enclosing list field's variants: {result}"
    );
}

/// RFC 0015 F4 — the union-of-fields completion at the EMPTY ambiguous body
/// (the just-typed discovery moment, previously resolving to the parent):
/// both variants' unique fields offered with provenance, each carrying the
/// auto-annotation `additionalTextEdits` on the header (strictly ABOVE the
/// cursor — the eager-safety invariant), shared fields merged with no edit.
#[tokio::test]
async fn ambiguous_union_body_offers_union_of_fields_with_auto_annotation() {
    let base = temp_dir("f4-union-of-fields");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model = ws.join("union.model.nml");
    let model_text = "model modelA:\n    a string?\n    shared string?\nmodel modelB:\n    b string?\n    shared string?\nmodel host:\n    slot (modelA | modelB)?\n";
    fs::write(&model, model_text).expect("write model");
    let config = ws.join("app.nml");
    // The discovery moment: `slot:` just typed, cursor on the fresh blank line.
    let config_text = "host H:\n    slot:\n        \n";
    fs::write(&config, config_text).expect("write config");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&model, model_text).await;
    harness.open(&config, config_text).await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&config) },
                "position": { "line": 2, "character": 8 },
            }),
        )
        .await;
    let items = result.as_array().expect("completion item array");
    let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
    assert!(
        labels.contains(&"a") && labels.contains(&"b") && labels.contains(&"shared"),
        "the UNION of both variants' fields: {labels:?}"
    );
    // Tier 0: `a` is unique to modelA → provenance + the header auto-edit.
    let a = items.iter().find(|i| i["label"] == json!("a")).unwrap();
    assert!(
        a["detail"].as_str().unwrap().contains("modelA"),
        "provenance: {a}"
    );
    assert!(
        a["sortText"].as_str().unwrap().starts_with("0_"),
        "discriminating fields rank first: {a}"
    );
    let edit = &a["additionalTextEdits"][0];
    assert_eq!(
        edit["newText"],
        json!("slot as modelA"),
        "picking a discriminating field auto-annotates: {a}"
    );
    // Eager-safety invariant: the edit is strictly ABOVE the cursor line.
    assert!(
        edit["range"]["end"]["line"].as_u64().unwrap() < 2,
        "the auto-edit must lie above the cursor: {a}"
    );
    // Tier 1: `shared` is in both → merged provenance, NO auto-edit.
    let shared = items
        .iter()
        .find(|i| i["label"] == json!("shared"))
        .unwrap();
    assert!(
        shared["sortText"].as_str().unwrap().starts_with("1_"),
        "shared fields rank after: {shared}"
    );
    assert!(
        shared["additionalTextEdits"].is_null(),
        "a shared field must not auto-annotate: {shared}"
    );
    assert!(
        shared["detail"]
            .as_str()
            .unwrap()
            .contains("modelA | modelB"),
        "merged provenance: {shared}"
    );
}

/// RFC 0015 F4 — D2's repair tier: the code-action request surfaces one
/// "Apply fix" action per candidate, and NEITHER is preferred (an editor
/// auto-applying one would resurrect the guess D2 forbids).
#[tokio::test]
async fn d2_offers_two_annotate_actions_neither_preferred() {
    let base = temp_dir("f4-d2-actions");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model = ws.join("union.model.nml");
    let model_text = "model modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel host:\n    slot (modelA | modelB)?\n";
    fs::write(&model, model_text).expect("write model");
    let config = ws.join("app.nml");
    let config_text = "host H:\n    slot:\n        a = \"x\"\n";
    fs::write(&config, config_text).expect("write config");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&model, model_text).await;
    let report = harness.open(&config, config_text).await;
    let diags = report["diagnostics"].as_array().expect("diagnostics");
    let d2 = diags
        .iter()
        .find(|d| d["code"] == json!("NML2052"))
        .expect("D2 diagnostic");
    let actions = harness
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": file_uri(&config) },
                "range": d2["range"],
                "context": { "diagnostics": [d2] },
            }),
        )
        .await;
    let actions = actions.as_array().expect("actions");
    let titles: Vec<&str> = actions
        .iter()
        .filter_map(|a| a["title"].as_str())
        .filter(|t| t.starts_with("Apply fix"))
        .collect();
    assert_eq!(
        titles,
        vec!["Apply fix: `slot as modelA`", "Apply fix: `slot as modelB`"],
        "one mutually exclusive fix per candidate: {actions:?}"
    );
    for a in actions.iter().filter(|a| {
        a["title"]
            .as_str()
            .is_some_and(|t| t.starts_with("Apply fix"))
    }) {
        assert!(
            a["isPreferred"].is_null(),
            "alternatives must never be preferred: {a}"
        );
    }
}

/// RFC 0015 round 21, end-to-end through the real handler: a PLAIN
/// oneof-typed field's fresh body is a discovery moment — the discriminator
/// is offered honestly (no `as` announcement, no edit: an annotation on a
/// non-union field is a stray) — and the discriminator's VALUE position
/// completes the arm keys.
#[tokio::test]
async fn oneof_discovery_moment_end_to_end() {
    let base = temp_dir("oneof-discovery");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model = ws.join("mail.model.nml");
    let model_text = "model logM:\n    level string?\nmodel postM:\n    server string?\n\noneof mail by kind:\n    \"log\" -> logM\n    \"post\" -> postM\n\nmodel host:\n    slot mail?\n";
    fs::write(&model, model_text).expect("write model");
    let config = ws.join("app.nml");
    // The discovery moment: `slot:` just typed, no discriminator yet.
    let config_text = "host H:\n    slot:\n        \n";
    fs::write(&config, config_text).expect("write config");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&model, model_text).await;
    harness.open(&config, config_text).await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&config) },
                "position": { "line": 2, "character": 8 },
            }),
        )
        .await;
    let items = result.as_array().expect("completion item array");
    let kind = items
        .iter()
        .find(|i| i["label"] == json!("kind"))
        .unwrap_or_else(|| panic!("the discriminator must be offered: {result}"));
    assert_eq!(kind["insertText"], json!("kind = "), "{kind}");
    assert!(
        kind["additionalTextEdits"].is_null(),
        "no annotation may attach on a non-union field: {kind}"
    );
    assert!(
        !kind["detail"].as_str().unwrap_or_default().contains("adds"),
        "the label must not announce an edit it does not attach: {kind}"
    );

    // The value position the scaffold creates: arm keys complete.
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&config), "version": 2 },
                "contentChanges": [{ "text": "host H:\n    slot:\n        kind = \n" }],
            }),
        )
        .await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&config) },
                "position": { "line": 2, "character": 15 },
            }),
        )
        .await;
    let items = result.as_array().expect("completion item array");
    let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
    assert!(
        labels.contains(&"\"log\"") && labels.contains(&"\"post\""),
        "arm keys must complete at the discriminator value: {labels:?}"
    );

    // The SWITCHING state (round 24): a VALID authored discriminator still
    // completes every arm — the author is changing variants, not setting one.
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&config), "version": 3 },
                "contentChanges": [{ "text": "host H:\n    slot:\n        kind = \"log\"\n" }],
            }),
        )
        .await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&config) },
                "position": { "line": 2, "character": 16 },
            }),
        )
        .await;
    let items = result.as_array().expect("completion item array");
    let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
    assert!(
        labels.contains(&"\"log\"") && labels.contains(&"\"post\""),
        "the switching state must complete every arm: {labels:?}"
    );
    // The honest label: arm keys are discriminator values, not enum variants.
    let log = items
        .iter()
        .find(|i| i["label"] == json!("\"log\""))
        .unwrap();
    assert_eq!(
        log["detail"],
        json!("discriminator value"),
        "arm keys must render as what they are: {log}"
    );
}

/// Round 26 (mutation-found gap): the top-level oneof discriminator path
/// sorts arm keys in DECLARATION order — one regime across all value
/// positions. The fixture's declaration order deliberately differs from
/// alphabetical, so a regression to the old alphabetical key fails.
#[tokio::test]
async fn top_level_discriminator_values_sort_in_declaration_order() {
    let base = temp_dir("oneof-toplevel-sort");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model = ws.join("mail.model.nml");
    let model_text = "model zebraM:\n    z string?\nmodel alphaM:\n    a string?\n\noneof mail by kind:\n    \"zebra\" -> zebraM\n    \"alpha\" -> alphaM\n";
    fs::write(&model, model_text).expect("write model");
    let config = ws.join("app.nml");
    let config_text = "mail X:\n    kind = \n";
    fs::write(&config, config_text).expect("write config");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&model, model_text).await;
    harness.open(&config, config_text).await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&config) },
                "position": { "line": 1, "character": 11 },
            }),
        )
        .await;
    let items = result.as_array().expect("completion item array");
    let sort_of = |label: &str| {
        items
            .iter()
            .find(|i| i["label"] == json!(label))
            .and_then(|i| i["sortText"].as_str().map(str::to_owned))
            .unwrap_or_else(|| panic!("{label} offered: {result}"))
    };
    assert!(
        sort_of("\"zebra\"") < sort_of("\"alpha\""),
        "declaration order, not alphabetical: zebra={} alpha={}",
        sort_of("\"zebra\""),
        sort_of("\"alpha\"")
    );
}

/// Round 23, end-to-end: a body resolved through a oneof's DEFAULT
/// discriminator offers BOTH the default variant's fields and the
/// discriminator itself as a defaulted knob — field parity (a defaulted
/// field is shown with its default; so is the defaulted discriminator).
#[tokio::test]
async fn defaulted_discriminator_stays_discoverable_end_to_end() {
    let base = temp_dir("oneof-defaulted-knob");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model = ws.join("mail.model.nml");
    let model_text = "model logM:\n    level string?\nmodel postM:\n    server string?\n\noneof mail by kind = \"log\":\n    \"log\" -> logM\n    \"post\" -> postM\n\nmodel host:\n    slot mail?\n";
    fs::write(&model, model_text).expect("write model");
    let config = ws.join("app.nml");
    let config_text = "host H:\n    slot:\n        \n";
    fs::write(&config, config_text).expect("write config");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&model, model_text).await;
    harness.open(&config, config_text).await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&config) },
                "position": { "line": 2, "character": 8 },
            }),
        )
        .await;
    let items = result.as_array().expect("completion item array");
    let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
    assert!(
        labels.contains(&"level"),
        "the default variant's fields complete: {labels:?}"
    );
    let kind = items
        .iter()
        .find(|i| i["label"] == json!("kind"))
        .unwrap_or_else(|| panic!("the defaulted discriminator must stay discoverable: {result}"));
    assert_eq!(kind["insertText"], json!("kind = "), "{kind}");
    let detail = kind["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("(default)") && detail.contains("\"log\""),
        "the knob states its default: {kind}"
    );
    // Sorted after the variant's declared fields (it is optional-with-default).
    let kind_sort = kind["sortText"].as_str().unwrap();
    let level_sort = items
        .iter()
        .find(|i| i["label"] == json!("level"))
        .and_then(|i| i["sortText"].as_str())
        .unwrap();
    assert!(
        kind_sort > level_sort,
        "the defaulted knob ranks after declared fields: {kind_sort} vs {level_sort}"
    );
}

#[tokio::test]
async fn directive_completion_offers_vocabulary() {
    let base = temp_dir("directive-completion");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let text = "model core:\n    name string+ #\n";
    let (ws, model) = directive_workspace(&base, text);

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&model, text).await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&model) },
                // End of `    name string+ #` — directly after the `#`.
                "position": { "line": 1, "character": 18 },
            }),
        )
        .await;
    let items = result.as_array().expect("completion item array");
    let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
    assert_eq!(
        labels,
        [
            "sealed", "identity", "append", "overlay", "live", "restart", "key"
        ],
        "the language's directives first, then the vocabulary in declaration order: {result}"
    );
    let sealed = items
        .iter()
        .find(|i| i["label"] == json!("sealed"))
        .expect("sealed item");
    assert_eq!(sealed["detail"], json!("no argument"), "{sealed}");
    assert!(
        sealed["documentation"]
            .as_str()
            .is_some_and(|d| d.starts_with("Write-once from the bottom")),
        "{sealed}"
    );
    let key = items
        .iter()
        .find(|i| i["label"] == json!("key"))
        .expect("key item");
    assert_eq!(key["detail"], json!("ident"), "{key}");
    assert_eq!(
        key["documentation"],
        json!("Names the element-identity field for set pairing"),
        "{key}"
    );
}

/// TEST F — out-of-band store heal, PULL model (RFC 0035): a pinned file opened
/// against an EMPTY store resolves unbound; publishing the package into the
/// store OUT-OF-BAND (plain fs writes through a second `Store` handle — exactly
/// what `nudge schema sync` does from another process) must heal the editor on
/// its NEXT pull. There is no background poll: the store cache is stat-guarded,
/// so the very next `textDocument/diagnostic` re-resolves against the freshly
/// published package. This is the "heals on next interaction" contract.
#[tokio::test]
async fn out_of_band_store_publish_heals_on_repull() {
    let base = temp_dir("out-of-band-heal");
    let store_base = base.join("store");
    // The store *directory* exists but holds no packages — the cold-store,
    // brand-new-operator baseline.
    fs::create_dir_all(&store_base).expect("create store dir");

    let ws = demo_workspace(&base);
    // `demo.nml` matches the demo package's validator globs, so it binds the
    // moment the pinned package becomes loadable — the heal is observable.
    let file = ws.join("demo.nml");
    fs::write(&file, "").expect("write demo.nml");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let published = harness.open(&file, "").await;
    let notes: Vec<&str> = published["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .filter_map(|d| d["message"].as_str())
        .collect();
    assert!(
        notes.iter().any(|m| m.contains("'demo' is not installed")),
        "cold store must surface the missing-pin note: {notes:?}"
    );
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&file) }))
        .await;
    assert_eq!(info["bound"], json!(false), "must open unbound: {info}");

    // The out-of-band sync, then a re-pull — what the editor issues on the
    // next interaction (edit/focus) with the file.
    publish_demo(&Store::at(&store_base));
    let healed = harness.diagnostics(&file_uri(&file)).await;
    let healed_notes: Vec<&str> = healed["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .filter_map(|d| d["message"].as_str())
        .collect();
    assert!(
        !healed_notes.iter().any(|m| m.contains("not installed")),
        "re-pull after the sync must drop the missing-pin note: {healed_notes:?}"
    );

    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&file) }))
        .await;
    assert_eq!(
        info["bound"],
        json!(true),
        "re-pull did not heal binding: {info}"
    );
    assert_eq!(info["package"], json!("demo"), "{info}");
    assert_eq!(info["source"], json!("store current"), "{info}");
}

/// TEST E — cross-file heal, PULL model (RFC 0035): editing a schema (`model`)
/// file makes a dependent instance file's diagnostics stale. There is no
/// background sweep — the dependent heals when it is next PULLED (what VS Code
/// issues when the file regains focus). This is the exact cross-file promise
/// the pull migration rests on: fix the schema, re-pull the instance, clean.
#[tokio::test]
async fn model_edit_heals_other_documents_on_repull() {
    let base = temp_dir("cross-file-heal");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model = ws.join("core.model.nml");
    let model_v1 = "model server:\n    port number\n";
    fs::write(&model, model_v1).expect("write model");
    let app = ws.join("app.nml");
    let app_text = "server main:\n    port = \"x\"\n";
    fs::write(&app, app_text).expect("write app");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&model, model_v1).await;
    let published = harness.open(&app, app_text).await;
    let initial = published["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    assert!(
        !initial.is_empty(),
        "string-for-number must diagnose before the fix: {published}"
    );

    // Fix the schema instead of the instance: `port` becomes a string, so the
    // app's `port = "x"` is now valid — but its published set is stale.
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&model), "version": 2 },
                "contentChanges": [{ "text": "model server:\n    port string\n" }],
            }),
        )
        .await;

    // Re-pull the app — the client's focus-change pull. It re-resolves against
    // the edited model and comes back clean.
    let healed = harness.diagnostics(&file_uri(&app)).await;
    assert!(
        healed["diagnostics"].as_array().is_some_and(Vec::is_empty),
        "re-pull after the schema fix must clear the app's diagnostic: {healed}"
    );
}

/// RFC 0026 decision 3: the editor's freshness no longer rests ENTIRELY on
/// the client's file watcher.
///
/// Every `.nml` under a root is INDEXED, so the universe memo answers a
/// store STAMP for it and never stats it — and the stamp moves only on a
/// buffer edit or a `didChangeWatchedFiles` event. A client that cannot
/// watch was therefore stale forever: the manifest below is repaired ON
/// DISK, with no event, and the tenant it governs kept its NML2088 across
/// every pull. Now discovery re-stats an INDEXED copy before answering from
/// it, and the repair lands on the next pull.
///
/// Three clients, one fixture: a bare one (no capability), one that
/// DECLARES `didChangeWatchedFiles.dynamicRegistration` — the watcher is its
/// job, so the server leaves the disk alone — and one that declares it and
/// then REFUSES the registration, which is the case the discarded result hid.
#[tokio::test]
async fn an_indexed_copy_is_re_read_from_disk_when_the_client_does_not_watch() {
    for (tag, declares, refuses, heals) in [
        ("bare", false, false, true),
        ("watching", true, false, false),
        ("refused", true, true, true),
    ] {
        let base = temp_dir(&format!("disk-freshness-{tag}"));
        let store_base = base.join("store");
        fs::create_dir_all(&store_base).expect("create store dir");
        let ws = base.join("ws");
        fs::create_dir_all(ws.join("tenants/cu")).expect("create workspace");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/manifest-rules/did-you-mean");
        for f in [
            "demo.package.nml",
            "core.model.nml",
            "tenants/cu/plain.flow.nml",
        ] {
            fs::copy(fixture.join(f), ws.join(f)).expect(f);
        }
        let manifest = ws.join("demo.package.nml");
        let broken = fs::read_to_string(&manifest).expect("manifest");
        let tenant = ws.join("tenants/cu/plain.flow.nml");
        let tenant_text = fs::read_to_string(&tenant).expect("tenant");

        let mut harness = Harness::new(Store::at(&store_base));
        harness.refuse_registration = refuses;
        if declares {
            harness.initialize_watching(&ws).await;
        } else {
            harness.initialize(&ws).await;
        }
        // The universe is unloadable, so the governed file carries its row.
        let report = harness.open(&tenant, &tenant_text).await;
        assert_eq!(codes_of(&report), ["NML2088"], "{tag}: {report}");

        // Repaired on DISK, with no watcher event of any kind. The text is
        // a different LENGTH, so the stamp moves on every filesystem.
        let repaired = broken.replace("versio = ", "version = ");
        assert_ne!(repaired, broken, "{tag}");
        fs::write(&manifest, &repaired).expect("repair the manifest");

        let report = harness.diagnostics(&file_uri(&tenant)).await;
        if heals {
            assert!(
                codes_of(&report).is_empty(),
                "{tag}: the repair on disk must reach the next pull: {report}"
            );
        } else {
            assert_eq!(
                codes_of(&report),
                ["NML2088"],
                "{tag}: a watching client's events are the contract; the server must not \
                 second-guess the disk: {report}"
            );
        }
    }
}

/// LSP 3.17 has NO static spelling for file watching: a server may register
/// `workspace/didChangeWatchedFiles` dynamically only where the client
/// declared `workspace.didChangeWatchedFiles.dynamicRegistration`. Asked
/// anyway, a client that cannot answer the request never replies, the
/// `register_capability` await inside `initialized` never resolves, and the
/// server process then survives BOTH the `exit` notification and stdin's EOF
/// — one leaked language server per editor session. The harness answers every
/// server request, so only the request's presence can be asserted here.
#[tokio::test]
async fn a_file_watch_is_registered_only_with_a_client_that_declared_it() {
    for (tag, declares) in [("bare", false), ("declaring", true)] {
        let base = temp_dir(&format!("watch-registration-{tag}"));
        let store_base = base.join("store");
        fs::create_dir_all(&store_base).expect("create store dir");
        let ws = base.join("ws");
        fs::create_dir_all(&ws).expect("create workspace");

        let mut harness = Harness::new(Store::at(&store_base));
        if declares {
            harness.initialize_watching(&ws).await;
        } else {
            harness.initialize(&ws).await;
        }
        let asked = harness
            .requests
            .iter()
            .any(|m| m == "client/registerCapability");
        assert_eq!(
            asked, declares,
            "{tag}: registerCapability is asked of a declaring client and of no other: \
             {:?}",
            harness.requests
        );
    }
}

/// The other half of the rule: an OPEN buffer is the master while it is
/// open (LSP 3.17), so the disk fallback never touches one — a keystroke
/// pays no `stat`, and a file edited on disk under an open buffer does not
/// overwrite what the user is typing.
#[tokio::test]
async fn an_open_buffer_is_never_re_read_from_disk() {
    let base = temp_dir("disk-freshness-buffer");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu")).expect("create workspace");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/manifest-rules/did-you-mean");
    for f in [
        "demo.package.nml",
        "core.model.nml",
        "tenants/cu/plain.flow.nml",
    ] {
        fs::copy(fixture.join(f), ws.join(f)).expect(f);
    }
    let manifest = ws.join("demo.package.nml");
    let broken = fs::read_to_string(&manifest).expect("manifest");
    let tenant = ws.join("tenants/cu/plain.flow.nml");
    let tenant_text = fs::read_to_string(&tenant).expect("tenant");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    // The manifest is OPEN — and still broken in the buffer.
    harness.open(&manifest, &broken).await;
    let report = harness.open(&tenant, &tenant_text).await;
    assert_eq!(codes_of(&report), ["NML2088"], "{report}");

    // The disk is repaired underneath it. The buffer is the master: the
    // tenant's verdict must not move until the buffer does.
    fs::write(&manifest, broken.replace("versio = ", "version = ")).expect("repair on disk");
    let report = harness.diagnostics(&file_uri(&tenant)).await;
    assert_eq!(
        codes_of(&report),
        ["NML2088"],
        "an open buffer is the master: {report}"
    );

    // The buffer catches up: the same repair, through `didChange`.
    harness
        .open(&manifest, &broken.replace("versio = ", "version = "))
        .await;
    let report = harness.diagnostics(&file_uri(&tenant)).await;
    assert!(codes_of(&report).is_empty(), "{report}");
}

/// TEST G — a watched DELETED event for an OPEN document must not touch it:
/// per the LSP spec, after didOpen the client buffer is the source of truth,
/// so disk deletion is irrelevant while the file is open. The server must
/// keep both the text AND the schema registry contribution (a half-alive doc
/// would be worse than either state). Pinned end-to-end: field hover through
/// the deleted-but-open model file still works afterwards.
#[tokio::test]
async fn watched_delete_of_open_document_is_ignored() {
    let base = temp_dir("watched-delete-open");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model = ws.join("core.model.nml");
    let model_text = "model server:\n    // Port the listener binds\n    port number\n";
    fs::write(&model, model_text).expect("write model");
    let app = ws.join("app.nml");
    let app_text = "server main:\n    port = 80\n";
    fs::write(&app, app_text).expect("write app");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&model, model_text).await;
    harness.open(&app, app_text).await;

    // The file vanishes from disk while its buffer stays open — exactly the
    // git-checkout / external-rm race the guard exists for.
    fs::remove_file(&model).expect("delete model on disk");
    harness
        .notify(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": [{ "uri": file_uri(&model), "type": 3 }] }),
        )
        .await;

    // Field hover in app.nml resolves through the model registry AND the
    // open buffer — both must have survived the DELETE.
    let result = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                // Inside `port` of `    port = 80`.
                "position": { "line": 1, "character": 5 },
            }),
        )
        .await;
    let value = result["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("hover must still resolve after watched DELETE: {result}"));
    assert!(
        value.contains("port number"),
        "field signature lost after watched DELETE: {value}"
    );
}

/// TEST G′ — sibling of the DELETE guard: a watched CHANGED event for an OPEN
/// document must not adopt the disk content either. Same LSP-spec rule (the
/// client buffer is the sole source of truth after didOpen): the disk gets a
/// DIFFERENT model definition, and both observable surfaces must still
/// reflect the BUFFER text afterwards — a re-pull of the dependent validates
/// against the buffer's schema, and hover resolves the buffer's field.
#[tokio::test]
async fn watched_change_of_open_document_is_ignored() {
    let base = temp_dir("watched-change-open");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model = ws.join("core.model.nml");
    let model_text = "model server:\n    port number\n";
    fs::write(&model, model_text).expect("write model");
    let app = ws.join("app.nml");
    let app_text = "server main:\n    port = 80\n";
    fs::write(&app, app_text).expect("write app");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&model, model_text).await;
    harness.open(&app, app_text).await;

    // Disk diverges while the buffer stays open — a git checkout / external
    // formatter race. `port` becomes a string on DISK only.
    fs::write(&model, "model server:\n    port string\n").expect("rewrite model on disk");
    harness
        .notify(
            "workspace/didChangeWatchedFiles",
            // FileChangeType::CHANGED = 2.
            json!({ "changes": [{ "uri": file_uri(&model), "type": 2 }] }),
        )
        .await;

    // Buffer-is-truth, surface 1 (pull model): re-pull the APP. `port = 80`
    // is valid against the BUFFER's `port number` (empty diagnostics); had the
    // server adopted the DISK's `port string`, 80 would flag a type error. An
    // empty set therefore proves the disk CHANGED was ignored.
    let app_diags = harness.diagnostics(&file_uri(&app)).await;
    assert!(
        app_diags["diagnostics"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "watched CHANGED of an open model must not adopt disk text: {app_diags}"
    );

    // Buffer-is-truth, surface 2: hover still resolves the BUFFER's schema
    // (`port number`), not the disk's (`port string`).
    let result = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                // Inside `port` of `    port = 80`.
                "position": { "line": 1, "character": 5 },
            }),
        )
        .await;
    let value = result["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("hover must still resolve after watched CHANGED: {result}"));
    assert!(
        value.contains("port number") && !value.contains("port string"),
        "hover must reflect the buffer text, not the disk text: {value}"
    );
}

/// A watched CREATE is the ONLY thing that can tell the server a file
/// APPEARED — and nothing drove it.
///
/// The universe cache is re-validated by FINGERPRINTS of the files a
/// discovery read (`PackageResolver::universe_for`), so a file that did not
/// exist when the universe was built is invisible to it: it is in nobody's
/// read set, and every later pull is served from the cache. That is what
/// `did_change_watched_files` calls `invalidate_claims_for` for — it drops
/// the roots containing a created or deleted path, and with them the wasm
/// editor's listings memo.
///
/// MEASURED before this test existed: deleting that call left the whole
/// `nml-lsp` suite green. The only watched-files test drove a CHANGED event,
/// which that path deliberately skips.
#[tokio::test]
async fn a_watched_create_of_a_manifest_is_what_re_discovers_the_universe() {
    let base = temp_dir("watched-create-manifest");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu")).expect("create workspace");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/manifest-rules/did-you-mean");
    // Everything EXCEPT the manifest: the universe is discovered without it.
    for f in ["core.model.nml", "tenants/cu/plain.flow.nml"] {
        fs::copy(fixture.join(f), ws.join(f)).expect(f);
    }
    let tenant = ws.join("tenants/cu/plain.flow.nml");
    let tenant_text = fs::read_to_string(&tenant).expect("tenant");
    // The fixture's manifest is deliberately misspelled; this one is whole.
    let manifest_text = fs::read_to_string(fixture.join("demo.package.nml"))
        .expect("manifest")
        .replace("versio = ", "version = ");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_watching(&ws).await;
    harness.open(&tenant, &tenant_text).await;
    let before = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&tenant) }))
        .await;
    assert_eq!(
        before["bound"],
        json!(false),
        "unbound to start with: {before}"
    );

    // The manifest appears on disk. Nothing the discovery READ has changed —
    // this file was not there to be read — so the cache answers as before.
    let manifest = ws.join("demo.package.nml");
    fs::write(&manifest, &manifest_text).expect("create the manifest");
    let blind = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&tenant) }))
        .await;
    assert_eq!(
        blind["bound"],
        json!(false),
        "a file that appeared is invisible until its event arrives: {blind}"
    );

    // …and the event is what closes it. FileChangeType::CREATED = 1.
    harness
        .notify(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": [{ "uri": file_uri(&manifest), "type": 1 }] }),
        )
        .await;
    let after = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&tenant) }))
        .await;
    assert_eq!(
        after["bound"],
        json!(true),
        "the watched CREATE did not re-discover the universe: {after}"
    );
    assert_eq!(after["package"], json!("demo"), "{after}");
}

/// TEST B — a field's leading comment block (RFC 0004 §4.3) rides extraction
/// into both editor surfaces: hover renders it as a markdown paragraph under
/// the signature, and field completion carries it as the item documentation.
#[tokio::test]
async fn field_doc_comment_surfaces_in_hover_and_completion() {
    let base = temp_dir("field-doc");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model_text = "\
model server:
    // Port the listener binds
    port number
    // Hostname clients use
    host string?
";
    fs::write(ws.join("core.model.nml"), model_text).expect("write model");
    let app = ws.join("app.nml");
    let app_text = "server main:\n    port = 80\n    \n";
    fs::write(&app, app_text).expect("write app");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&app, app_text).await;

    let hover = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                // Inside `port` of `    port = 80`.
                "position": { "line": 1, "character": 5 },
            }),
        )
        .await;
    let value = hover["contents"]["value"].as_str().expect("markdown hover");
    assert!(
        value.contains("```nml") && value.contains("port number"),
        "signature block missing: {value}"
    );
    assert!(
        value.contains("\n\nPort the listener binds"),
        "doc paragraph missing under the signature: {value}"
    );

    let completion = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                // The empty body line — field-name completion position.
                "position": { "line": 2, "character": 4 },
            }),
        )
        .await;
    let items = completion.as_array().expect("completion item array");
    let host = items
        .iter()
        .find(|i| i["label"] == json!("host"))
        .unwrap_or_else(|| panic!("host field not offered: {completion}"));
    assert_eq!(
        host["documentation"],
        json!("Hostname clients use"),
        "field doc must ride the completion item: {host}"
    );
}

/// TEST D — the editor's coverage question has no walk cap of its own
/// any more (step 0e): a root with >2048 entries — which capped the
/// pre-0e claims walk and left the file's directive vocabulary
/// "undetermined" forever — is enumerated by the kernel's one bounded
/// walk (65,536 entries, A16), the glob-bound file behind the filler wall
/// is seen, and the stray model file gets its vocabulary with no
/// undetermined-coverage diagnostic.
#[tokio::test]
async fn wide_root_keeps_directive_vocabulary_without_an_undetermined_note() {
    let base = temp_dir("walk-cap-diag");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("apps/site")).expect("create workspace");
    fs::write(ws.join("demo.package.nml"), DEMO_MANIFEST_WITH_DIRECTIVES).expect("write manifest");
    fs::write(
        ws.join("core.model.nml"),
        "model core:\n    name string+\n    mode string?\n",
    )
    .expect("write declared source");
    let stray = ws.join("stray.model.nml");
    // An unknown directive: reported only when the vocabulary is KNOWN.
    let stray_text = "model stray:\n    name string #bogus\n";
    fs::write(&stray, stray_text).expect("write stray model");
    fs::write(ws.join("apps/site/app.nml"), "").expect("write bound file");
    for i in 0..2100 {
        fs::write(ws.join(format!("filler-{i}.txt")), "").expect("write filler");
    }

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let published = harness.open(&stray, stray_text).await;
    let diags = published["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    assert!(
        !diags.iter().any(|d| d["message"]
            .as_str()
            .is_some_and(|m| m.contains("package coverage undetermined"))),
        "no undetermined-coverage note on a wide root: {published}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d["message"].as_str().is_some_and(|m| m.contains("bogus"))),
        "the covering package's vocabulary judged the directive: {published}"
    );
}

/// Step 0e-b: the editor's index is the kernel's enumeration, so a root
/// the walk cannot enumerate indexes NOTHING — loud and fail-closed, as
/// `nml check` validates nothing under it — and the editor says so once,
/// as a `window/logMessage` warning in the kernel's words (NML2089); the
/// same tree, listable, indexes the model. (The old index walk silently
/// skipped a directory it could not list and kept the rest.)
#[cfg(unix)]
#[tokio::test]
async fn a_root_the_walk_cannot_enumerate_indexes_nothing_and_says_so() {
    use std::os::unix::fs::PermissionsExt;
    let base = temp_dir("index-denied");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    fs::write(ws.join("ok.model.nml"), "model okmodel:\n    a number\n").expect("write model");
    let app = ws.join("app.nml");
    fs::write(&app, "\n").expect("write app");
    let locked = ws.join("locked");
    fs::create_dir_all(&locked).expect("create locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
    if fs::read_dir(&locked).is_ok() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod");
        return; // root: the lock does not bite
    }
    let offered = |completion: Value| -> Vec<String> {
        completion
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|i| i["label"].as_str().map(str::to_string))
            .collect()
    };
    let completion_params = json!({
        "textDocument": { "uri": file_uri(&app) },
        "position": { "line": 0, "character": 0 },
    });

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let denial = loop {
        let params = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        let message = params["message"].as_str().expect("a message").to_string();
        if message.contains("[NML2089]") {
            assert_eq!(params["type"], json!(2), "a warning: {params}");
            break message;
        }
    };
    assert!(
        denial.starts_with(&format!(
            "NML: nothing under `{}` is indexed: [NML2089] ",
            ws.display()
        )) && denial.contains("the walk stopped at `locked` (unreadable:"),
        "{denial}"
    );
    harness.open(&app, "\n").await;
    let labels = offered(
        harness
            .request("textDocument/completion", completion_params.clone())
            .await,
    );
    assert!(
        !labels.iter().any(|l| l == "okmodel"),
        "nothing is indexed under a root the walk could not enumerate: {labels:?}"
    );

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&app, "\n").await;
    let labels = offered(
        harness
            .request("textDocument/completion", completion_params)
            .await,
    );
    assert!(
        labels.iter().any(|l| l == "okmodel"),
        "the same tree, listable, indexes the model: {labels:?}"
    );
}

/// An indexed file is read up to [`MAX_INDEX_BYTES`] —
/// the CLI's target bound; past it the file is not indexed and the
/// editor says so, while the files beside it are indexed as ever.
#[tokio::test]
async fn an_oversized_file_is_not_indexed_and_the_editor_says_so() {
    let base = temp_dir("index-oversized");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    fs::write(ws.join("ok.model.nml"), "model okmodel:\n    a number\n").expect("write model");
    let big = ws.join("big.model.nml");
    fs::File::create(&big)
        .expect("create big")
        .set_len(MAX_INDEX_BYTES as u64 + 1)
        .expect("size big");
    let app = ws.join("app.nml");
    fs::write(&app, "\n").expect("write app");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let refusal = loop {
        let params = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        let message = params["message"].as_str().expect("a message").to_string();
        if message.contains("not indexed") {
            assert_eq!(params["type"], json!(2), "a warning: {params}");
            break message;
        }
    };
    // The kernel's one cap sentence (r85 D4) — the same voice `nml check`
    // refuses an oversized target in; the index used to say `exceeds the
    // 16777216-byte bound`.
    assert_eq!(
        refusal,
        format!(
            "NML: `{}` is not indexed: too large: over 16 MiB (16777217 bytes) — an indexed \
             workspace file is read only up to 16 MiB (16777216 bytes)",
            big.display()
        )
    );
    harness.open(&app, "\n").await;
    let completion = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                "position": { "line": 0, "character": 0 },
            }),
        )
        .await;
    let labels: Vec<&str> = completion
        .as_array()
        .map(|items| items.iter().filter_map(|i| i["label"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        labels.contains(&"okmodel"),
        "the file beside it is indexed: {labels:?}"
    );
}

/// TEST E — hover on `#live` in a covered model file renders the vocabulary
/// entry: `**#name** (arg) — doc`.
#[tokio::test]
async fn directive_hover_renders_vocabulary_entry() {
    let base = temp_dir("directive-hover");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let text = "model core:\n    name string+ #live\n";
    let (ws, model) = directive_workspace(&base, text);

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&model, text).await;
    let result = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(&model) },
                // Inside the `live` name of `    name string+ #live`.
                "position": { "line": 1, "character": 19 },
            }),
        )
        .await;
    let value = result["contents"]["value"]
        .as_str()
        .expect("markdown hover");
    assert_eq!(
        value, "**#live** (no argument) — Change applies without a restart",
        "{result}"
    );
}

/// RFC 0010 tier 1 end-to-end: hovering a diagnostic's span returns the
/// error-index explanation summary through the real handler chain — cache
/// fill, narrowest-hit selection, compose, wire — with the diagnostic's
/// range as the hover range (explanation-only case).
#[tokio::test]
async fn hover_on_a_diagnostic_explains_the_code() {
    let base = temp_dir("hover-explanation");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create ws");
    let app = ws.join("app.nml");
    let text = "service Api:\n    x = 1.2.3\n";
    fs::write(&app, text).expect("write app");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&app, text).await;
    let result = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                // Inside the `1.2.3` literal (NML0013's token-width span).
                "position": { "line": 1, "character": 10 },
            }),
        )
        .await;
    let value = result["contents"]["value"]
        .as_str()
        .expect("markdown hover");
    assert!(value.contains("**NML0013**"), "{value}");
    assert!(value.contains("Invalid number"), "{value}");
    assert!(value.contains("nml explain NML0013"), "{value}");
    assert_eq!(
        result["range"]["start"]["line"], 1,
        "explanation-only hover carries the diagnostic's range: {result}"
    );
}

// ── RFC 0017 §10 duration LSP tooling (CST-driven) ───────────────────────

fn duration_schema_workspace(base: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model = ws.join("service.model.nml");
    let model_text = "model service:\n    timeout duration?\n    interval duration(min = 1s)\n";
    fs::write(&model, model_text).expect("write model");
    let app = ws.join("app.nml");
    (ws, model, app)
}

const DUR_APP: &str = "service Api:\n    timeout = 1h30m\n    interval = 30\n";

/// Compound duration hover: ranged markdown with per-component breakdown and total.
#[tokio::test]
async fn duration_hover_on_compound_literal() {
    let base = temp_dir("duration-hover");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, model, app) = duration_schema_workspace(&base);

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness
        .open(&model, fs::read_to_string(&model).unwrap().as_str())
        .await;
    harness.open(&app, DUR_APP).await;
    let result = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                // Inside `1h30m` on `    timeout = 1h30m`.
                "position": { "line": 1, "character": 17 },
            }),
        )
        .await;
    let value = result["contents"]["value"]
        .as_str()
        .expect("markdown hover");
    assert!(value.contains("**duration**"), "{value}");
    assert!(value.contains("1h30m"), "{value}");
    assert!(
        value.contains("1h + 30m = 90m"),
        "breakdown with human respelling: {value}"
    );
    assert!(value.contains("total"), "{value}");
    assert_eq!(
        result["range"]["start"]["character"], 14,
        "hover range starts at the literal: {result}"
    );
    assert_eq!(
        result["range"]["end"]["character"], 19,
        "hover range ends after the literal: {result}"
    );
}

/// Bare-number unit completion in a duration-typed config field (end of digits).
#[tokio::test]
async fn duration_unit_completion_on_bare_number() {
    let base = temp_dir("duration-completion-bare");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, model, app) = duration_schema_workspace(&base);

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness
        .open(&model, fs::read_to_string(&model).unwrap().as_str())
        .await;
    harness.open(&app, DUR_APP).await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                // After `30` on `    interval = 30`.
                "position": { "line": 2, "character": 17 },
            }),
        )
        .await;
    let items = result.as_array().expect("completion item array");
    let labels: Vec<&str> = items
        .iter()
        .filter_map(|i| i["label"].as_str())
        .filter(|l| l.starts_with("30"))
        .collect();
    assert!(
        labels.contains(&"30s"),
        "duration units offered for bare magnitude: {labels:?}"
    );
}

/// Mid-compound completion offers only units finer than the previous segment.
#[tokio::test]
async fn duration_mid_compound_unit_completion() {
    let base = temp_dir("duration-completion-mid");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, model, app) = duration_schema_workspace(&base);
    let text = "service Api:\n    timeout = 1h30\n";

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness
        .open(&model, fs::read_to_string(&model).unwrap().as_str())
        .await;
    harness.open(&app, text).await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                // On the dangling `30` in `1h30`.
                "position": { "line": 1, "character": 18 },
            }),
        )
        .await;
    let items = result.as_array().expect("completion item array");
    let minute = items
        .iter()
        .find(|i| i["label"].as_str() == Some("30m"))
        .unwrap_or_else(|| panic!("30m offered: {result}"));
    assert!(
        !items.iter().any(|i| i["label"].as_str() == Some("30h")),
        "hours must not be re-offered after `1h`: {result}"
    );
    assert!(
        minute["labelDetails"]["detail"]
            .as_str()
            .is_some_and(|d| d.contains('=')),
        "preview total in label details: {minute}"
    );
}

/// Document highlight selects the whole duration literal, not just the token.
#[tokio::test]
async fn duration_document_highlight_covers_literal() {
    let base = temp_dir("duration-highlight");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, model, app) = duration_schema_workspace(&base);

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness
        .open(&model, fs::read_to_string(&model).unwrap().as_str())
        .await;
    harness.open(&app, DUR_APP).await;
    let result = harness
        .request(
            "textDocument/documentHighlight",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                "position": { "line": 1, "character": 16 },
            }),
        )
        .await;
    let highlights = result.as_array().expect("highlight array");
    assert_eq!(highlights.len(), 1, "{result}");
    assert_eq!(
        highlights[0]["range"]["start"]["character"], 14,
        "highlight starts at literal: {result}"
    );
    assert_eq!(
        highlights[0]["range"]["end"]["character"], 19,
        "highlight ends after literal: {result}"
    );
}

/// Semantic tokens mark duration literals with the `duration` modifier on `number`.
#[tokio::test]
async fn duration_semantic_tokens_full() {
    let base = temp_dir("duration-semtok");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, model, app) = duration_schema_workspace(&base);

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness
        .open(&model, fs::read_to_string(&model).unwrap().as_str())
        .await;
    harness.open(&app, DUR_APP).await;
    let result = harness
        .request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": file_uri(&app) } }),
        )
        .await;
    let data = result["data"].as_array().expect("semantic token data");
    assert!(
        data.len() >= 5,
        "duration literals produce semantic tokens: {result}"
    );
    assert_eq!(
        data[4],
        json!(1),
        "duration modifier bit set on first token: {data:?}"
    );
}

/// Inlay hints show the coarsest exact total for multi-component literals.
#[tokio::test]
async fn duration_inlay_hint_shows_total() {
    let base = temp_dir("duration-inlay");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, model, app) = duration_schema_workspace(&base);

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness
        .open(&model, fs::read_to_string(&model).unwrap().as_str())
        .await;
    harness.open(&app, DUR_APP).await;
    let result = harness
        .request(
            "textDocument/inlayHint",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 10, "character": 0 },
                },
            }),
        )
        .await;
    let hints = result.as_array().expect("inlay hint array");
    assert!(
        hints.iter().any(|h| {
            h["label"]
                .as_str()
                .is_some_and(|l| l.starts_with("= ") && l.contains('m'))
        }),
        "coarsest total hint after compound literal: {result}"
    );
}

/// Selection range parent chain includes the enclosing duration literal span.
#[tokio::test]
async fn duration_selection_range_includes_literal() {
    let base = temp_dir("duration-selection");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, model, app) = duration_schema_workspace(&base);

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness
        .open(&model, fs::read_to_string(&model).unwrap().as_str())
        .await;
    harness.open(&app, DUR_APP).await;
    let result = harness
        .request(
            "textDocument/selectionRange",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                "positions": [{ "line": 1, "character": 17 }],
            }),
        )
        .await;
    let ranges = result.as_array().expect("selection range array");
    let mut out = Vec::new();
    let mut cur = &ranges[0];
    loop {
        let r = &cur["range"];
        out.push((
            r["start"]["character"].as_u64().unwrap() as u32,
            r["end"]["character"].as_u64().unwrap() as u32,
        ));
        if cur.get("parent").is_none_or(Value::is_null) {
            break;
        }
        cur = &cur["parent"];
    }
    assert!(
        out.contains(&(13, 19)),
        "literal span appears in selection parent chain: {out:?}"
    );
}

/// NML3008 dangling magnitude carries related-information on the break.
#[tokio::test]
async fn duration_dangling_magnitude_surfaces_related_info() {
    let base = temp_dir("duration-nml3008");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, model, app) = duration_schema_workspace(&base);
    let text = "service Api:\n    timeout = 1h30\n";

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness
        .open(&model, fs::read_to_string(&model).unwrap().as_str())
        .await;
    let report = harness.open(&app, text).await;
    let diags = report["diagnostics"].as_array().expect("diagnostics");
    let d = diags
        .iter()
        .find(|d| d["code"] == json!("NML3008"))
        .unwrap_or_else(|| panic!("NML3008 diagnostic: {diags:?}"));
    let related = d["relatedInformation"]
        .as_array()
        .expect("related information");
    assert!(
        !related.is_empty(),
        "dangling magnitude points at the break: {d}"
    );
}

/// NML3005 sole-component fractional fix is offered as a quick-fix.
#[tokio::test]
async fn duration_fractional_quickfix() {
    let base = temp_dir("duration-nml3005");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, model, app) = duration_schema_workspace(&base);
    let text = "service Api:\n    timeout = 1.5h\n";

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness
        .open(&model, fs::read_to_string(&model).unwrap().as_str())
        .await;
    let report = harness.open(&app, text).await;
    let diags = report["diagnostics"].as_array().expect("diagnostics");
    let d = diags
        .iter()
        .find(|d| d["code"] == json!("NML3005"))
        .unwrap_or_else(|| panic!("NML3005 diagnostic: {diags:?}"));
    let actions = harness
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                "range": d["range"],
                "context": { "diagnostics": [d] },
            }),
        )
        .await;
    let titles: Vec<&str> = actions
        .as_array()
        .expect("actions")
        .iter()
        .filter_map(|a| a["title"].as_str())
        .collect();
    assert!(
        titles.iter().any(|t| t.contains("1h30m")),
        "granularity-preserving compound fix: {titles:?}"
    );
}

/// NML3007 duplicate-unit merge fix is offered as a quick-fix.
#[tokio::test]
async fn duration_duplicate_unit_quickfix() {
    let base = temp_dir("duration-nml3007");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, model, app) = duration_schema_workspace(&base);
    let text = "service Api:\n    timeout = 1h2h\n";

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness
        .open(&model, fs::read_to_string(&model).unwrap().as_str())
        .await;
    let report = harness.open(&app, text).await;
    let diags = report["diagnostics"].as_array().expect("diagnostics");
    let d = diags
        .iter()
        .find(|d| d["code"] == json!("NML3007"))
        .unwrap_or_else(|| panic!("NML3007 diagnostic: {diags:?}"));
    let actions = harness
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                "range": d["range"],
                "context": { "diagnostics": [d] },
            }),
        )
        .await;
    let titles: Vec<&str> = actions
        .as_array()
        .expect("actions")
        .iter()
        .filter_map(|a| a["title"].as_str())
        .collect();
    assert!(
        titles.iter().any(|t| t.contains("3h")),
        "merged whole-literal fix: {titles:?}"
    );
}

/// UTF-16 position encoding is declared and hover ranges stay correct past non-ASCII.
#[tokio::test]
async fn duration_utf16_position_encoding_and_ranges() {
    let base = temp_dir("duration-utf16");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, model, app) = duration_schema_workspace(&base);
    let text = "service Api:\n    label = \"☕\"\n    timeout = 30s\n";

    let mut harness = Harness::new(Store::at(&store_base));
    let caps = harness
        .request(
            "initialize",
            json!({ "capabilities": {}, "rootUri": file_uri(&ws) }),
        )
        .await;
    assert_eq!(
        caps["capabilities"]["positionEncoding"],
        json!("utf-16"),
        "server declares UTF-16: {caps}"
    );
    harness.notify("initialized", json!({})).await;
    harness
        .open(&model, fs::read_to_string(&model).unwrap().as_str())
        .await;
    harness.open(&app, text).await;
    let result = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                // On `30s` — line index shifted by the emoji string on line 1.
                "position": { "line": 2, "character": 16 },
            }),
        )
        .await;
    let value = result["contents"]["value"]
        .as_str()
        .expect("markdown hover");
    assert!(value.contains("30s"), "{value}");
    assert_eq!(
        result["range"]["start"]["character"], 14,
        "UTF-16 range pins literal start: {result}"
    );
}

/// Schema-driven block-keyword completion (RFC 0012 editor package): an open
/// document completes block keywords from its resolved schema context — the
/// scope registry's concrete models — labeled with "schema" provenance.
#[tokio::test]
async fn keyword_completion_offers_schema_models() {
    let base = temp_dir("schema-keyword-completion");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create ws");
    let model = ws.join("cache.model.nml");
    let model_text = "model cache:\n    maxEntries number\n";
    fs::write(&model, model_text).expect("write model");
    let app = ws.join("app.nml");
    let app_text = "\n";
    fs::write(&app, app_text).expect("write app");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&model, model_text).await;
    harness.open(&app, app_text).await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                "position": { "line": 0, "character": 0 },
            }),
        )
        .await;
    let items = result.as_array().expect("completion item array");
    let cache = items
        .iter()
        .find(|i| i["label"] == json!("cache"))
        .unwrap_or_else(|| panic!("schema keyword offered: {result}"));
    assert_eq!(cache["detail"], json!("schema"), "{cache}");
}

/// RFC 0010 tier 2: `nml/explain` serves the full index entry from the
/// running binary — canonical heading, case-normalized lookup, `null` for
/// unknowns, error-as-data for malformed params — and `nml/explainIndex`
/// lists every code with its summary. The wire shapes are never-migrate;
/// this test IS the contract.
#[tokio::test]
async fn explain_methods_serve_entries_and_index_over_the_wire() {
    let base = temp_dir("explain-methods");
    let ws = demo_workspace(&base);
    let mut harness = Harness::new(Store::at(base.join("store")));
    harness.initialize(&ws).await;

    // Case-normalized full entry, heading from the MATCHED head.
    let entry = harness
        .request("nml/explain", json!({ "code": "nml0013" }))
        .await;
    let markdown = entry["markdown"].as_str().expect("markdown field");
    assert!(markdown.starts_with("# NML0013\n\n"), "{markdown}");
    assert!(markdown.contains("Invalid number"), "{markdown}");

    // Unknown and hostile codes are null — a lookup miss, not a fault.
    for bogus in ["NML9999", "../../etc/passwd", "NML0013 OR 1=1"] {
        let miss = harness
            .request("nml/explain", json!({ "code": bogus }))
            .await;
        assert_eq!(miss, Value::Null, "{bogus}");
    }
    // Malformed params answer as data (schemaInfo's convention).
    let bad = harness.request("nml/explain", json!({})).await;
    assert!(bad["error"].as_str().is_some(), "{bad}");

    // The index: every entry has a code and a non-empty summary; NML0013 is
    // present; tolerant of empty params.
    let index = harness.request("nml/explainIndex", json!({})).await;
    let entries = index.as_array().expect("index array");
    assert!(
        entries.len() > 50,
        "expected the full code space, got {}",
        entries.len()
    );
    let invalid_number = entries
        .iter()
        .find(|e| e["code"] == json!("NML0013"))
        .unwrap_or_else(|| panic!("NML0013 missing from index: {index}"));
    assert!(
        invalid_number["summary"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "{invalid_number}"
    );
    // The headline beside the summary: one line, no markdown emphasis —
    // what the palette labels a row with (r105-ux P3).
    assert!(
        invalid_number["headline"]
            .as_str()
            .is_some_and(|h| !h.is_empty() && !h.contains("**") && h.chars().count() <= 80),
        "{invalid_number}"
    );
}

/// RFC 0010 tier 2: the "Explain NML0000" code action is NEGOTIATION-GATED —
/// emitted (deduped, command id echoed, code as argument) only when the
/// client declared `initializationOptions.explainCommand`; a client that
/// declared nothing never receives an action it cannot execute.
#[tokio::test]
async fn explain_code_action_is_negotiation_gated() {
    let base = temp_dir("explain-action");
    let ws = demo_workspace(&base);
    let app = ws.join("app.nml");
    // Two NML0013 diagnostics — the action must dedup to one.
    let bad = "service Api:\n    x = 1.2.3\n    y = 4.5.6\n";
    fs::write(&app, bad).expect("write app");

    for declared in [true, false] {
        let mut harness = Harness::new(Store::at(base.join(format!("store-{declared}"))));
        if declared {
            harness
                .initialize_with_options(&ws, json!({ "explainCommand": "nml.explain" }))
                .await;
        } else {
            harness.initialize(&ws).await;
        }
        let report = harness.open(&app, bad).await;
        let diags = report["diagnostics"].as_array().expect("diagnostics");
        assert!(!diags.is_empty(), "fixture must produce diagnostics");
        // The border invariant the Explain filter (and hover explanations)
        // stand on: every CODED diagnostic is stamped `source: "nml"` with a
        // STRING code — true today because all coded findings flow through
        // the one converter (`push_diagnostic`); the direct-construction
        // sites in `validate_document` are uncoded advisories. A new coded
        // path that bypasses the converter fails here, not in a user's
        // editor as a silently missing action.
        for diag in diags {
            if let Some(code) = diag.get("code") {
                assert!(
                    code.is_string(),
                    "coded diagnostic with non-string code: {diag}"
                );
                assert_eq!(
                    diag["source"],
                    json!("nml"),
                    "coded diagnostic without source stamp: {diag}"
                );
            }
        }

        // Exactly what a client does: round-trip the pulled diagnostics as
        // the code-action context.
        let result = harness
            .request(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": file_uri(&app) },
                    "range": diags[0]["range"],
                    "context": { "diagnostics": diags },
                }),
            )
            .await;
        let actions: Vec<Value> = result.as_array().cloned().unwrap_or_default();
        let explains: Vec<&Value> = actions
            .iter()
            .filter(|a| {
                a["title"]
                    .as_str()
                    .is_some_and(|t| t.starts_with("Explain "))
            })
            .collect();
        if declared {
            assert_eq!(explains.len(), 1, "deduped by code: {result}");
            let action = explains[0];
            assert_eq!(action["title"], json!("Explain NML0013"), "{action}");
            assert_eq!(
                action["command"]["command"],
                json!("nml.explain"),
                "{action}"
            );
            assert_eq!(
                action["command"]["arguments"],
                json!(["NML0013"]),
                "{action}"
            );
            assert!(action.get("kind").is_none(), "kind stays empty: {action}");
        } else {
            assert!(
                explains.is_empty(),
                "undeclared client got an action: {result}"
            );
        }
    }
}

// ── Universe provenance matrix ────────────────────────────────────────

/// One fixture trio, four transports. Every leg's universe holds the SAME
/// three texts — a trait the buffer resolves against, a "polluted" source
/// carrying a facet-violating default, and the buffer itself (one mixin
/// that resolves, one deliberately missing) — delivered through each
/// provenance the server's universe `match` can select:
///
/// | leg       | channel                                | variant    |
/// |-----------|----------------------------------------|------------|
/// | declared  | workspace manifest `[]schema` files    | `Declared` |
/// | store     | published, hash-verified store package | `Snapshot` |
/// | in-binary | injected provider (the `nudge lsp` \
///               wiring)                                | `Snapshot` |
/// | registry  | plain workspace files (indexed set)    | `None`     |
///
/// Identical universe content must produce identical verdicts, asserted
/// through the REAL JSON-RPC pull per leg:
/// 1. the missing mixin is reported (the universe was consulted),
/// 2. the trait-resolved mixin is clean (the universe's content bound),
/// 3. the polluted source's default error never lands on the buffer
///    (the round-29 attribution filter, on every channel).
///
/// The Snapshot legs additionally plant a DECOY directory-mate defining
/// the missing name — and assert it stays missing: hash-pinned published
/// sources beat unpinned disk neighbors, as an executable claim rather
/// than a doc comment.
const UNI_BASE: &str = "trait base:\n    x string\n";
const UNI_POLLUTED: &str = "model polluted:\n    n number(min = 1) = 0\n";
const UNI_BUFFER: &str = "model child is base, nope:\n    y string\n";
const UNI_DECOY: &str = "model nope:\n    z string\n";

fn universe_package() -> nml_validate::package::SchemaPackage {
    let manifest = "\
package uni:
    version = \"0.1.0\"
    formatVersion = 1
    rootMarkers:
        - \"app.nml\"

[]schema schemas:
    - base:
        file = \"base.model.nml\"
    - polluted:
        file = \"polluted.model.nml\"

[]validator validators:
    - base:
        files:
            - \"app.nml\"
        schemas:
            - base
";
    nml_validate::package::SchemaPackage::from_parts(manifest, |file| match file {
        "base.model.nml" => Ok(UNI_BASE.to_string()),
        "polluted.model.nml" => Ok(UNI_POLLUTED.to_string()),
        other => Err(format!("unexpected schema file {other}")),
    })
    .expect("universe fixture package")
}

fn assert_universe_verdicts(leg: &str, report: &Value) {
    let diags = report["diagnostics"].as_array().expect("diagnostics array");
    let msgs: Vec<&str> = diags.iter().filter_map(|d| d["message"].as_str()).collect();
    assert!(
        msgs.iter()
            .any(|m| m.contains("unknown `is` target 'nope'")),
        "[{leg}] the missing mixin must reach the editor: {msgs:?}"
    );
    assert!(
        !msgs
            .iter()
            .any(|m| m.contains("'base'") && m.contains("unknown")),
        "[{leg}] the universe's trait must resolve 'base': {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("default for")),
        "[{leg}] the polluted source's default error must never land here: {msgs:?}"
    );
}

#[tokio::test]
async fn universe_provenance_matrix() {
    // Leg 1 — DECLARED: manifest + schema files on disk; the buffer is a
    // covered-but-undeclared sibling (root rule), appended last by the
    // assembler. Mirrors the proven `directive_workspace` discovery path.
    {
        let base = temp_dir("uni-declared");
        let ws = base.join("ws");
        fs::create_dir_all(&ws).expect("create workspace");
        let manifest = universe_package();
        fs::write(ws.join("uni.package.nml"), &manifest.manifest_text).expect("manifest");
        fs::write(ws.join("base.model.nml"), UNI_BASE).expect("base");
        fs::write(ws.join("polluted.model.nml"), UNI_POLLUTED).expect("polluted");
        fs::write(ws.join("app.nml"), "").expect("claim anchor");

        let mut h = Harness::new(Store::at(base.join("store")));
        h.initialize(&ws).await;
        let report = h.open(&ws.join("child.model.nml"), UNI_BUFFER).await;
        assert_universe_verdicts("declared", &report);
    }

    // Leg 2 — STORE Snapshot: the same package published to a store; a
    // decoy dir-mate defines the missing name and must be IGNORED.
    {
        let base = temp_dir("uni-store");
        let store_base = base.join("store");
        fs::create_dir_all(&store_base).expect("store dir");
        Store::at(&store_base)
            .publish(&universe_package())
            .expect("publish universe package");
        let ws = base.join("ws");
        fs::create_dir_all(&ws).expect("create workspace");
        fs::write(ws.join("app.nml"), "").expect("claim anchor");
        fs::write(ws.join("decoy.model.nml"), UNI_DECOY).expect("decoy");

        let mut h = Harness::new(Store::at(&store_base));
        h.initialize(&ws).await;
        let report = h.open(&ws.join("child.model.nml"), UNI_BUFFER).await;
        assert_universe_verdicts("store", &report);
    }

    // Leg 3 — IN-BINARY Snapshot: the same package value injected through
    // the provider constructor (the `nudge lsp` wiring); decoy again.
    {
        let base = temp_dir("uni-inbinary");
        let ws = base.join("ws");
        fs::create_dir_all(&ws).expect("create workspace");
        fs::write(ws.join("app.nml"), "").expect("claim anchor");
        fs::write(ws.join("decoy.model.nml"), UNI_DECOY).expect("decoy");

        let mut h = Harness::new_provider(universe_package(), Store::at(base.join("store")));
        h.initialize(&ws).await;
        let report = h.open(&ws.join("child.model.nml"), UNI_BUFFER).await;
        assert_universe_verdicts("in-binary", &report);
    }

    // Leg 4 — REGISTRY fallback: no manifest, no store, no decoy (the
    // missing name must stay genuinely missing). The universe is the
    // workspace registry set, so the trait deliberately lives in a
    // DIFFERENT directory than the buffer — the exact layout that once
    // produced a false `unknown \`is\` target` squiggle on a mixin the
    // same server could F12 to (the load pass's fallback used to see
    // only parent-directory neighbors).
    {
        let base = temp_dir("uni-registry");
        let ws = base.join("ws");
        fs::create_dir_all(ws.join("a")).expect("create ws/a");
        fs::create_dir_all(ws.join("b")).expect("create ws/b");
        fs::write(ws.join("a/base.model.nml"), UNI_BASE).expect("base");
        fs::write(ws.join("a/polluted.model.nml"), UNI_POLLUTED).expect("polluted");

        let mut h = Harness::new(Store::at(base.join("store")));
        h.initialize(&ws).await;
        let report = h.open(&ws.join("b/child.model.nml"), UNI_BUFFER).await;
        assert_universe_verdicts("registry", &report);
    }

    // Leg 5 — BUFFER-FIRST LIVENESS through the production read path: a
    // declared sibling's UNSAVED edit must change the buffer's verdict
    // (the unit tests cover the assembler; this pins the server's own
    // documents-lock closure with genuinely divergent buffer vs disk).
    {
        let base = temp_dir("uni-liveness");
        let ws = base.join("ws");
        fs::create_dir_all(&ws).expect("create workspace");
        let manifest = universe_package();
        fs::write(ws.join("uni.package.nml"), &manifest.manifest_text).expect("manifest");
        fs::write(ws.join("base.model.nml"), UNI_BASE).expect("base");
        fs::write(ws.join("polluted.model.nml"), UNI_POLLUTED).expect("polluted");
        fs::write(ws.join("app.nml"), "").expect("claim anchor");

        let mut h = Harness::new(Store::at(base.join("store")));
        h.initialize(&ws).await;
        let report = h.open(&ws.join("child.model.nml"), UNI_BUFFER).await;
        assert_universe_verdicts("liveness-pre", &report);

        // Rename the trait in the OPEN buffer only — disk still says
        // `base`. The child's next pull must see the buffer.
        h.open(&ws.join("base.model.nml"), "trait renamed:\n    x string\n")
            .await;
        let report = h.diagnostics(&file_uri(&ws.join("child.model.nml"))).await;
        let diags = report["diagnostics"].as_array().expect("array");
        assert!(
            diags.iter().any(|d| d["message"]
                .as_str()
                .is_some_and(|m| m.contains("unknown `is` target 'base'"))),
            "an unsaved sibling edit must reach the buffer's verdict: {diags:?}"
        );
    }
}

/// One `textDocument/codeAction` round-trip for one diagnostic.
async fn one_code_action(harness: &mut Harness, path: &Path, diag: Value) -> Value {
    harness
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": file_uri(path) },
                "range": diag["range"],
                "context": { "diagnostics": [diag] },
            }),
        )
        .await
}

/// RFC 0023 A.3 — the editor's quick-fix path. Staleness is settled by
/// MEMBERSHIP (`data` equality against the current cache), each
/// suggestion resolves through the ONE resolver as a singleton batch,
/// titles derive from the outcome (`Deleted::title` for structural
/// deletions; `Remove` for the empty verbatim fix), and a deletion is
/// never a preferred action.
#[tokio::test]
async fn code_actions_gate_on_membership_and_resolve_deletions() {
    let base = temp_dir("resolver-actions");
    let ws = demo_workspace(&base);
    let mut harness = Harness::new(Store::at(base.join("store")));
    harness.initialize(&ws).await;

    let app = ws.join("uses-on-def.nml");
    let text = "model core uses ghost:\n    name string\n";
    fs::write(&app, text).expect("write app");
    let report = harness.open(&app, text).await;
    let diags = report["diagnostics"].as_array().expect("diagnostics");
    let d2062 = diags
        .iter()
        .find(|d| d["code"] == json!("NML2062"))
        .unwrap_or_else(|| panic!("no NML2062 in {report}"))
        .clone();
    assert_eq!(
        d2062["data"]["suggestions"][0]["kind"],
        json!("delete"),
        "the structural kind rides the wire: {d2062}"
    );

    let deletions = |result: &Value| -> Vec<Value> {
        result
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| a["title"] == json!("Delete the `uses` clause"))
            .collect()
    };

    // A CURRENT diagnostic yields the resolver-backed action.
    let result = one_code_action(&mut harness, &app, d2062.clone()).await;
    let dels = deletions(&result);
    assert_eq!(dels.len(), 1, "one action per suggestion: {result}");
    let action = &dels[0];
    assert_eq!(action["kind"], json!("quickfix"), "{action}");
    assert!(
        action["isPreferred"].is_null(),
        "a structural removal is never auto-applied: {action}"
    );
    let edits = action["edit"]["changes"][file_uri(&app)]
        .as_array()
        .expect("workspace edit")
        .clone();
    assert_eq!(edits.len(), 1, "the clause is one splice: {edits:?}");
    assert_eq!(edits[0]["newText"], json!(""), "{edits:?}");

    // An unknown wire kind is no action, never a guess.
    let mut forged = d2062.clone();
    forged["data"]["suggestions"][0]["kind"] = json!("mystery");
    let result = one_code_action(&mut harness, &app, forged).await;
    assert!(
        deletions(&result).is_empty(),
        "an unknown kind must offer nothing: {result}"
    );

    // A STALE diagnostic (the buffer moved under it) is no action —
    // membership, not a version: the cached diagnostics for the new text
    // carry different data.
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&app), "version": 2 },
                "contentChanges": [{ "text": format!("// moved\n{text}") }],
            }),
        )
        .await;
    let result = one_code_action(&mut harness, &app, d2062.clone()).await;
    assert!(
        deletions(&result).is_empty(),
        "a stale suggestion must fail closed: {result}"
    );

    // Restoring the text restores the action: membership is data
    // equality on the CURRENT text, not a version counter.
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&app), "version": 3 },
                "contentChanges": [{ "text": text }],
            }),
        )
        .await;
    let result = one_code_action(&mut harness, &app, d2062).await;
    assert_eq!(deletions(&result).len(), 1, "current again: {result}");
}

/// The empty VERBATIM fix (the trailing-dot removal) is titled `Remove` —
/// a byte removal, not a structural deletion — and is not preferred.
#[tokio::test]
async fn an_empty_verbatim_fix_is_titled_remove() {
    let base = temp_dir("remove-title");
    let ws = demo_workspace(&base);
    let mut harness = Harness::new(Store::at(base.join("store")));
    harness.initialize(&ws).await;

    let app = ws.join("trailing-dot.nml");
    let text = "service Api:\n    x = 1299.\n";
    fs::write(&app, text).expect("write app");
    let report = harness.open(&app, text).await;
    let diags = report["diagnostics"].as_array().expect("diagnostics");
    let dot = diags
        .iter()
        .find(|d| {
            d["data"]["suggestions"][0]["replacement"] == json!("")
                && d["data"]["suggestions"][0]["kind"] == json!("fix")
        })
        .unwrap_or_else(|| panic!("no empty verbatim fix in {report}"))
        .clone();
    let result = harness
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                "range": dot["range"],
                "context": { "diagnostics": [dot] },
            }),
        )
        .await;
    let actions: Vec<Value> = result.as_array().cloned().unwrap_or_default();
    let remove = actions
        .iter()
        .find(|a| a["title"] == json!("Remove"))
        .unwrap_or_else(|| panic!("no Remove action in {result}"));
    assert!(remove["isPreferred"].is_null(), "{remove}");
}

/// The D1 repair taxonomy at the editor: an in-string NEL's THREE
/// alternatives (line break | kept byte | mojibake ellipsis) arrive as
/// three separate quick-fix actions, none preferred — the editor
/// presents the resolution space and a human picks; nothing may
/// auto-apply a guess.
#[tokio::test]
async fn in_string_alternatives_are_separate_never_preferred_actions() {
    let base = temp_dir("nel-alternatives");
    let ws = demo_workspace(&base);
    let mut harness = Harness::new(Store::at(base.join("store")));
    harness.initialize(&ws).await;

    let app = ws.join("nel.nml");
    let text = "service Api:\n    note = \"x\u{85}y\"\n";
    fs::write(&app, text).expect("write app");
    let report = harness.open(&app, text).await;
    let diags = report["diagnostics"].as_array().expect("diagnostics");
    let nel = diags
        .iter()
        .find(|d| d["code"] == json!("NML0017"))
        .unwrap_or_else(|| panic!("no NML0017 in {report}"))
        .clone();
    assert_eq!(
        nel["data"]["suggestions"].as_array().map(Vec::len),
        Some(3),
        "the wire carries the whole resolution space: {nel}"
    );
    let result = harness
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                "range": nel["range"],
                "context": { "diagnostics": [nel] },
            }),
        )
        .await;
    let actions: Vec<Value> = result.as_array().cloned().unwrap_or_default();
    for title in ["Apply fix: `\\n`", "Apply fix: `\\u{85}`", "Apply fix: `…`"] {
        let action = actions
            .iter()
            .find(|a| a["title"] == json!(title))
            .unwrap_or_else(|| panic!("no {title:?} action in {result}"));
        assert!(
            action["isPreferred"].is_null(),
            "an alternative must never be preferred: {action}"
        );
    }
    // The mojibake repair's edit really is the ellipsis, in place.
    let repair = actions
        .iter()
        .find(|a| a["title"] == json!("Apply fix: `…`"))
        .expect("repair action");
    let edits = &repair["edit"]["changes"][file_uri(&app).as_str()];
    assert_eq!(edits[0]["newText"], json!("…"), "{repair}");
}

/// The D-C collapse at the editor: a FEFF holding two quote runs
/// apart has NO sound removal (deleting it would glue a closing
/// delimiter), so the wire carries ONE suggestion — the escape — and
/// the code-action list offers exactly the escape quick-fix, with no
/// `Remove` action for the editor to present.
#[tokio::test]
async fn an_unsound_remove_collapses_to_the_escape_action_alone() {
    let base = temp_dir("collapsed-remove");
    let ws = demo_workspace(&base);
    let mut harness = Harness::new(Store::at(base.join("store")));
    harness.initialize(&ws).await;

    let app = ws.join("collapsed.nml");
    let text = "service Api:\n    doc = \"\"\"\n        a\n        \"\"\u{FEFF}\"\n        \"\"\"\n    port = 1\n";
    fs::write(&app, text).expect("write app");
    let report = harness.open(&app, text).await;
    let diags = report["diagnostics"].as_array().expect("diagnostics");
    let feff = diags
        .iter()
        .find(|d| d["code"] == json!("NML0018"))
        .unwrap_or_else(|| panic!("no NML0018 in {report}"))
        .clone();
    assert_eq!(
        feff["data"]["suggestions"].as_array().map(Vec::len),
        Some(1),
        "an unsound removal must collapse to the escape alone: {feff}"
    );
    assert_eq!(
        feff["data"]["suggestions"][0]["replacement"],
        json!("\\u{FEFF}"),
        "{feff}"
    );
    let result = harness
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                "range": feff["range"],
                "context": { "diagnostics": [feff] },
            }),
        )
        .await;
    let actions: Vec<Value> = result.as_array().cloned().unwrap_or_default();
    assert!(
        actions
            .iter()
            .any(|a| a["title"] == json!("Apply fix: `\\u{FEFF}`")),
        "the escape quick-fix is offered: {result}"
    );
    assert!(
        actions.iter().all(|a| a["title"] != json!("Remove")),
        "no phantom Remove action may reach the editor: {result}"
    );
}

/// A structural deletion whose resolution is TWO splices — the entry's
/// row plus the colon drop on the emptied clause-carrying header —
/// arrives as one action with one `WorkspaceEdit` holding both
/// `TextEdit`s (RFC 0023 §A.3's two-edit row, at the editor surface).
#[tokio::test]
async fn a_two_splice_deletion_is_one_action_with_two_text_edits() {
    let base = temp_dir("two-splice-action");
    let ws = demo_workspace(&base);
    let mut harness = Harness::new(Store::at(base.join("store")));
    harness.initialize(&ws).await;

    let app = ws.join("sealed-restatement.nml");
    let text = concat!(
        "model spec:\n    x string #sealed\n\n",
        "spec base:\n    x = \"1\"\n\n",
        "spec t uses base:\n    x = \"1\"\n",
    );
    fs::write(&app, text).expect("write app");
    let report = harness.open(&app, text).await;
    let diags = report["diagnostics"].as_array().expect("diagnostics");
    let d2060 = diags
        .iter()
        .find(|d| d["code"] == json!("NML2060"))
        .unwrap_or_else(|| panic!("no NML2060 in {report}"))
        .clone();
    let result = one_code_action(&mut harness, &app, d2060).await;
    let actions: Vec<Value> = result.as_array().cloned().unwrap_or_default();
    let action = actions
        .iter()
        .find(|a| a["title"] == json!("Delete this property"))
        .unwrap_or_else(|| panic!("no deletion action in {result}"));
    let edits = action["edit"]["changes"][file_uri(&app)]
        .as_array()
        .expect("workspace edit")
        .clone();
    assert_eq!(
        edits.len(),
        2,
        "the row and the emptied header's colon: {edits:?}"
    );
    assert!(edits.iter().all(|e| e["newText"] == json!("")), "{edits:?}");
}

/// A SINGLETON did-you-mean is the one preferred action (`Replace with
/// "…"`); N alternatives are N actions, none preferred (the RFC 0015
/// rule — the editor must never auto-apply a guess).
#[tokio::test]
async fn a_singleton_did_you_mean_is_preferred() {
    let base = temp_dir("dym-preferred");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let text = "model core:\n    name string+ #lvie\n    mode string?\n";
    let (ws, model) = directive_workspace(&base, text);
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&model, text).await;
    let diags = report["diagnostics"].as_array().expect("diagnostics");
    let dym = diags
        .iter()
        .find(|d| {
            d["message"]
                .as_str()
                .is_some_and(|m| m.contains("unknown directive '#lvie'"))
        })
        .unwrap_or_else(|| panic!("no did-you-mean in {report}"))
        .clone();
    let result = one_code_action(&mut harness, &model, dym).await;
    let actions: Vec<Value> = result.as_array().cloned().unwrap_or_default();
    let replace = actions
        .iter()
        .find(|a| a["title"] == json!("Replace with \"#live\""))
        .unwrap_or_else(|| panic!("no replace action in {result}"));
    assert_eq!(
        replace["isPreferred"],
        json!(true),
        "a singleton did-you-mean auto-applies: {replace}"
    );
}

/// The REGISTRY-REBUILD leg of code-action staleness (RFC 0023 §A.3's
/// stated reason membership beats a document version): a store
/// re-publish — content-addressed, same version string, NO buffer edit —
/// bumps the resolver generation, the diagnostics cache recomputes, and
/// the old suggestion's `data` is no longer a member, so it yields no
/// action; the fresh pull's diagnostic acts. One unchanging buffer with
/// two typos keeps every leg distinguishable: `nme` is a near-miss of
/// v1's `name` only, `lable` of v2's `label` only (the suggest cutoff
/// is max(len)/3 — every cross pair is out of range). Facts the fixture
/// depends on: the buffer MUST be named `demo.nml` (the validator
/// globs; any other name silently unbinds), and the diagnostic counts
/// are ASYMMETRIC (the block name satisfies v1's `name`, so only v2
/// adds a missing-required finding) — so diagnostics are selected by
/// typo token, never by count, field name, or full message (messages
/// embed the flipping content hash).
#[tokio::test]
async fn a_store_republish_stales_old_suggestions_and_heals_forward() {
    let base = temp_dir("store-republish-staleness");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let store = Store::at(&store_base);
    let publish = |schema: &'static str| {
        let package = SchemaPackage::from_parts(DEMO_MANIFEST, |_| Ok(schema.to_string()))
            .expect("package loads");
        store.publish(&package).expect("publish");
    };
    publish("model core:\n    name string+\n    mode string?\n");

    let ws = demo_workspace(&base);
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;

    let app = ws.join("demo.nml");
    let text = "core Main:\n    nme = \"x\"\n    lable = \"y\"\n";
    fs::write(&app, text).expect("write app");
    let report = harness.open(&app, text).await;

    let diag_for = |report: &Value, token: &str| -> Option<Value> {
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .find(|d| d["message"].as_str().is_some_and(|m| m.contains(token)))
            .cloned()
    };
    let replace_offered = |result: &Value, replacement: &str| -> bool {
        result
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .any(|a| a["title"] == json!(format!("Replace with \"{replacement}\"")))
    };

    // v1 positive control: `nme` acts (did-you-mean `name`); `lable`
    // carries no suggestion at all.
    let nme_v1 = diag_for(&report, "'nme'").expect("nme diagnostic");
    assert!(
        nme_v1["data"]["suggestions"][0]["replacement"] == json!("name"),
        "{nme_v1}"
    );
    let lable_v1 = diag_for(&report, "'lable'").expect("lable diagnostic");
    assert!(lable_v1["data"].is_null(), "no v1 suggestion: {lable_v1}");
    let result = one_code_action(&mut harness, &app, nme_v1.clone()).await;
    assert!(
        replace_offered(&result, "name"),
        "positive control: {result}"
    );

    // The registry rebuild: same version, different content — the
    // pointer is content-addressed, so the stat-guard notices on the
    // next resolve with no sleeps and NO buffer edit.
    publish("model core:\n    label string+\n    mode string?\n");

    // The OLD suggestion fails closed…
    let result = one_code_action(&mut harness, &app, nme_v1).await;
    assert!(
        !replace_offered(&result, "name"),
        "a registry rebuild must stale the old suggestion: {result}"
    );

    // …and the system heals forward: a fresh pull's `lable` diagnostic
    // carries v2's suggestion and acts, while `nme` has gone dark.
    let report = harness.diagnostics(&file_uri(&app)).await;
    let lable_v2 = diag_for(&report, "'lable'").expect("lable diagnostic (v2)");
    assert!(
        lable_v2["data"]["suggestions"][0]["replacement"] == json!("label"),
        "{lable_v2}"
    );
    let nme_v2 = diag_for(&report, "'nme'").expect("nme diagnostic (v2)");
    assert!(
        nme_v2["data"].is_null(),
        "no v2 suggestion for nme: {nme_v2}"
    );
    let result = one_code_action(&mut harness, &app, lable_v2).await;
    assert!(replace_offered(&result, "label"), "heals forward: {result}");
}

/// r73 — the workspace index walk's SAFETY rules, pinned independently of how
/// the walk asks the filesystem. A file reaches the scope registry only by the
/// index sweep here (none of these models is ever opened), so completion in an
/// unbound document is a direct read-out of what the walk kept:
///
/// - `target/`, `node_modules/` and dot-directories are pruned;
/// - symlinks are never followed — neither a symlinked directory nor a
///   symlinked `.nml` file;
/// - a non-`.nml` extension is not indexed;
/// - ordinary nested `.nml` files ARE indexed.
///
/// These are exactly the invariants a rewrite of the per-entry test can break
/// silently, which is why they are pinned by observable behaviour rather than
/// by the walk's shape.
#[tokio::test]
async fn workspace_index_walk_prunes_and_never_follows_symlinks() {
    let base = temp_dir("index-walk-rules");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");

    let write_model = |dir: &Path, name: &str| {
        fs::create_dir_all(dir).expect("create dir");
        fs::write(
            dir.join(format!("{name}.model.nml")),
            format!("model {name}:\n    a number\n"),
        )
        .expect("write model");
    };
    write_model(&ws.join("kept/deeper"), "keptmodel");
    write_model(&ws.join("target"), "targetmodel");
    write_model(&ws.join("node_modules"), "nodemodulesmodel");
    write_model(&ws.join(".hidden"), "hiddenmodel");
    // Indexed by extension, not by being an nml-shaped name.
    fs::write(
        ws.join("kept/wrongext.model.nmlx"),
        "model wrongextmodel:\n    a number\n",
    )
    .expect("write wrong-extension model");
    // Symlink targets live OUTSIDE the root, so anything they contribute
    // arrived by following a link.
    let outside = base.join("outside");
    write_model(&outside.join("dir"), "linkeddirmodel");
    write_model(&outside, "linkedfilemodel");

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.join("dir"), ws.join("linkdir"))
            .expect("symlink directory");
        std::os::unix::fs::symlink(
            outside.join("linkedfilemodel.model.nml"),
            ws.join("linkfile.model.nml"),
        )
        .expect("symlink file");
    }

    let app = ws.join("app.nml");
    fs::write(&app, "\n").expect("write app");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&app, "\n").await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                "position": { "line": 0, "character": 0 },
            }),
        )
        .await;
    let labels: Vec<String> = result
        .as_array()
        .expect("completion item array")
        .iter()
        .filter_map(|i| i["label"].as_str().map(str::to_string))
        .collect();

    assert!(
        labels.iter().any(|l| l == "keptmodel"),
        "a nested workspace model must be indexed by the sweep alone: {labels:?}"
    );
    for pruned in [
        "targetmodel",
        "nodemodulesmodel",
        "hiddenmodel",
        "wrongextmodel",
    ] {
        assert!(
            !labels.iter().any(|l| l == pruned),
            "{pruned} must not be indexed: {labels:?}"
        );
    }
    #[cfg(unix)]
    for followed in ["linkeddirmodel", "linkedfilemodel"] {
        assert!(
            !labels.iter().any(|l| l == followed),
            "the walk must never follow a symlink ({followed}): {labels:?}"
        );
    }
}

/// r73 — the workspace sweep runs at `initialized`, NOT inside the
/// `initialize` response. `initialize` gates the entire handshake: the client
/// may send nothing until it answers, and tower-lsp drives every handler from
/// one task (`join!(print_output, read_input, process_server_tasks)`), so a
/// sweep there holds the whole server. Measured on a 73k-entry checkout: 458 ms
/// warm inside `initialize`; 0.45 ms once moved.
///
/// Pinned by observable state: after the handshake alone the scope registry is
/// empty, and the `initialized` notification is what fills it.
#[tokio::test]
async fn workspace_index_is_built_at_initialized_not_in_the_handshake() {
    let base = temp_dir("index-scheduling");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create ws");
    fs::write(
        ws.join("swept.model.nml"),
        "model sweptmodel:\n    a number\n",
    )
    .expect("write model");
    let app = ws.join("app.nml");
    fs::write(&app, "\n").expect("write app");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.handshake_only(&ws).await;
    harness.open(&app, "\n").await;
    let offers_swept = |result: &Value| {
        result
            .as_array()
            .expect("completion item array")
            .iter()
            .any(|i| i["label"] == json!("sweptmodel"))
    };
    let completion = json!({
        "textDocument": { "uri": file_uri(&app) },
        "position": { "line": 0, "character": 0 },
    });
    let before = harness
        .request("textDocument/completion", completion.clone())
        .await;
    assert!(
        !offers_swept(&before),
        "the handshake must not have swept the workspace: {before}"
    );

    harness.notify("initialized", json!({})).await;
    let after = harness.request("textDocument/completion", completion).await;
    assert!(
        offers_swept(&after),
        "`initialized` must build the workspace index: {after}"
    );
}

/// The first document an editor opens right after `initialized` (VS Code
/// opens the active editor at once) is pulled while the workspace sweep
/// is still running — tower-lsp runs the handlers concurrently — and its
/// report is judged under an unindexed universe: an instance beside a
/// sibling model shows nothing, and a pull client pulls again only on an
/// edit, a focus, or `workspace/diagnostic/refresh`. Once the index
/// stands, a client that declared `refreshSupport` is asked to pull
/// again — exactly once — and the re-pull diagnoses. A client whose
/// first open came after the sweep (every sequential caller) is asked
/// nothing, as the refresh pins above hold.
#[tokio::test]
async fn a_document_opened_before_the_sweep_finished_is_asked_to_refresh() {
    let base = temp_dir("sweep-race-refresh");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create ws");
    fs::write(
        ws.join("core.model.nml"),
        "model server:\n    port number\n",
    )
    .expect("write model");
    let app = ws.join("app.nml");
    let app_text = "server main:\n    port = \"x\"\n";
    fs::write(&app, app_text).expect("write app");

    let mut harness = Harness::new(Store::at(&store_base));
    // The handshake as VS Code, `initialized` withheld: the sweep is pending.
    harness
        .request(
            "initialize",
            json!({
                "capabilities": { "workspace": { "diagnostics": { "refreshSupport": true } } },
                "rootUri": file_uri(&ws),
            }),
        )
        .await;
    let before = harness.open(&app, app_text).await;
    assert!(
        before["diagnostics"].as_array().is_some_and(Vec::is_empty),
        "a pull before the sweep sees no sibling model: {before}"
    );
    assert_eq!(harness.refreshes(), 0, "{:?}", harness.requests);

    harness.notify("initialized", json!({})).await;
    assert_eq!(
        harness.refreshes(),
        1,
        "the sweep found an open document: one refresh — {:?}",
        harness.requests
    );
    let after = harness.diagnostics(&file_uri(&app)).await;
    assert!(
        after["diagnostics"]
            .as_array()
            .is_some_and(|d| d.iter().any(|d| d["code"] == json!("NML2008"))),
        "the re-pull is judged under the index: {after}"
    );
    // Nothing further: the re-pull hit a fresh compute, not a rediscovery.
    harness.diagnostics(&file_uri(&app)).await;
    assert_eq!(harness.refreshes(), 1, "{:?}", harness.requests);
}

/// r85 D6: an inert input (NML2080) is reported ONCE, on ITS OWN
/// document, at its declaration, as information — never on every file
/// beneath it (three permanent 1:1 warnings on every tenant file, for
/// inputs the tenant could not act on, in the round-84 drive). The CLI
/// keeps its once-per-run line.
#[tokio::test]
async fn an_inert_input_is_noted_once_on_its_own_document() {
    let base = temp_dir("inert-once");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu")).expect("create workspace");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace");
    for f in [
        "demo.package.nml",
        "core.model.nml",
        "tenants/cu/plain.flow.nml",
        "tenants/cu/nml-project.nml",
    ] {
        fs::copy(fixture.join(f), ws.join(f)).expect(f);
    }
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let plain = ws.join("tenants/cu/plain.flow.nml");
    let report = harness
        .open(&plain, &fs::read_to_string(&plain).expect("read plain"))
        .await;
    assert!(
        !report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["code"] == "NML2080"),
        "the inert note rode the file beneath the input: {report}"
    );
    let config = ws.join("tenants/cu/nml-project.nml");
    // r86 (mutant MR21 survived): a comment line ABOVE the declaration,
    // so a note anchored at the top of the file (line 0) is told apart
    // from one anchored at the declaration (line 1).
    let text = format!(
        "// the tenant's own note\n{}",
        fs::read_to_string(&config).expect("read config")
    );
    fs::write(&config, &text).expect("rewrite config");
    let report = harness.open(&config, &text).await;
    let inert: Vec<&Value> = report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter(|d| d["code"] == "NML2080")
        .collect();
    assert_eq!(inert.len(), 1, "{report}");
    assert_eq!(inert[0]["severity"], json!(3), "information: {}", inert[0]);
    assert_eq!(
        inert[0]["range"]["start"]["line"],
        json!(1),
        "at the `project` declaration, not the top of the file: {}",
        inert[0]
    );
    assert!(
        inert[0]["message"]
            .as_str()
            .expect("message")
            .starts_with("project config `tenants/cu/nml-project.nml` is inert:"),
        "{}",
        inert[0]
    );
}

/// r85 D7: under a universe the walk could not enumerate, the editor
/// validates NOTHING — the NML2089 row is the whole report, no parse
/// finding, no composition (NML2064 used to claim `0 manifest(s)
/// discovered` for a walk that did not finish) — exactly as `nml check`
/// validates nothing and exits 1; the same tree, listable, reports the
/// file's own findings.
#[cfg(unix)]
#[tokio::test]
async fn a_truncated_universe_validates_nothing_in_the_editor() {
    use std::os::unix::fs::PermissionsExt;
    let base = temp_dir("refused-universe");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let app = ws.join("app.nml");
    let text = "thing base:\n    v =\n\nthing t uses base:\n    v = 2\n";
    fs::write(&app, text).expect("write app");
    let locked = ws.join("locked");
    fs::create_dir_all(&locked).expect("create locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
    if fs::read_dir(&locked).is_ok() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod");
        return; // root: the lock does not bite
    }
    let codes = |report: &Value| -> Vec<String> {
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .map(|d| d["code"].as_str().unwrap_or("").to_string())
            .collect()
    };
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&app, text).await;
    assert_eq!(codes(&report), ["NML2089"], "{report}");
    assert!(
        report["diagnostics"][0]["message"]
            .as_str()
            .expect("message")
            .starts_with("cannot enumerate manifests: the walk stopped at "),
        "{report}"
    );

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&app, text).await;
    let healthy = codes(&report);
    assert!(
        !healthy.is_empty() && !healthy.iter().any(|c| c == "NML2089"),
        "the same tree, listable, reports the file's own findings: {report}"
    );
}

/// r85 (r84-sec F2 + D3): a watched `.nml` that appears or grows while
/// the editor is open is read under the index's own per-file bound —
/// never whole into the store (+96 MB for a 48 MiB file) — and the
/// refusal is SAID through `window/logMessage` in the kernel's one cap
/// sentence, exactly as the startup index says it; the same file one
/// byte shorter is indexed by the event.
#[tokio::test]
async fn a_watched_file_past_the_index_bound_is_refused_and_said() {
    let base = temp_dir("watch-oversized");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let app = ws.join("app.nml");
    fs::write(&app, "\n").expect("write app");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&app, "\n").await;
    let big = ws.join("big.model.nml");
    fs::File::create(&big)
        .expect("create big")
        .set_len(MAX_INDEX_BYTES as u64 + 1)
        .expect("size big");
    harness
        .notify(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": [{ "uri": file_uri(&big), "type": 1 }] }),
        )
        .await;
    let refusal = loop {
        let params = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        let message = params["message"].as_str().expect("a message").to_string();
        if message.contains("not indexed") {
            assert_eq!(params["type"], json!(2), "a warning: {params}");
            break message;
        }
    };
    assert_eq!(
        refusal,
        format!(
            "NML: `{}` is not indexed: too large: over 16 MiB (16777217 bytes) — an indexed \
             workspace file is read only up to 16 MiB (16777216 bytes)",
            big.display()
        )
    );
    let completion_params = json!({
        "textDocument": { "uri": file_uri(&app) },
        "position": { "line": 0, "character": 0 },
    });
    let offered = |completion: Value| -> Vec<String> {
        completion
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|i| i["label"].as_str().map(str::to_string))
            .collect()
    };
    // Exactly the bound reads whole: a model file AT the cap is indexed
    // by the same event and its model is offered.
    let text = "model watchedmodel:\n    a number\n";
    let mut padded = text.to_string();
    padded.push_str(&" ".repeat(MAX_INDEX_BYTES - text.len()));
    fs::write(&big, padded).expect("rewrite big at the cap");
    harness
        .notify(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": [{ "uri": file_uri(&big), "type": 2 }] }),
        )
        .await;
    let labels = offered(
        harness
            .request("textDocument/completion", completion_params)
            .await,
    );
    assert!(
        labels.iter().any(|l| l == "watchedmodel"),
        "a file exactly at the bound is indexed by the watcher: {labels:?}"
    );
}

/// r85 (arch D6): a buffer opened THROUGH a linked directory is a buffer
/// at the link in the overlay, never at its target — the one
/// document-path rule at the overlay too. An unsaved manifest opened at
/// `linkdir/evil.package.nml` (`linkdir` → `docs/`) used to sit at
/// `docs/evil.package.nml` in the overlay: a live resolution input,
/// from an unsaved buffer behind a link, that closed the universe
/// (NML2088 on the tenant's file). Now it sits under a link the walk
/// never enters, and the tenant's file is untouched.
#[cfg(unix)]
#[tokio::test]
async fn a_buffer_behind_a_linked_directory_never_reaches_its_target() {
    let base = temp_dir("buffer-behind-link");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu")).expect("create workspace");
    fs::create_dir_all(ws.join("docs")).expect("create docs");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace");
    for f in [
        "demo.package.nml",
        "core.model.nml",
        "tenants/cu/plain.flow.nml",
        "docs/unclaimed.nml",
    ] {
        fs::copy(fixture.join(f), ws.join(f)).expect(f);
    }
    std::os::unix::fs::symlink("docs", ws.join("linkdir")).expect("link");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let plain = ws.join("tenants/cu/plain.flow.nml");
    let text = fs::read_to_string(&plain).expect("read plain");
    let codes = |report: &Value| -> Vec<String> {
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .map(|d| d["code"].as_str().unwrap_or("").to_string())
            .collect()
    };
    let before = codes(&harness.open(&plain, &text).await);
    harness
        .open(
            &ws.join("linkdir/evil.package.nml"),
            "package evil:\n    version = \"0.1.0\"\n    formatVersion = 1\n",
        )
        .await;
    let after = codes(&harness.diagnostics(&file_uri(&plain)).await);
    assert!(
        !after.iter().any(|c| c == "NML2088" || c == "NML2087"),
        "an unsaved buffer behind a link reached the universe: {after:?}"
    );
    assert_eq!(after, before, "the tenant's file is untouched");
}

/// r85 (arch D8): a document with no live project config — a file
/// OUTSIDE every workspace root — reads the EMBEDDER default, never a
/// root's `nml-project.nml`: with the root declaring `modifiers: allow`,
/// a file inside it using `|deny` is NML2002 (unknown modifier) and the
/// same text outside every root is not (the default accepts every
/// modifier). The root's config used to be read into a global at index
/// time and on every edit, so the outside file got the last indexed
/// root's modifiers.
#[tokio::test]
async fn a_file_outside_every_root_reads_the_embedder_default_config() {
    let base = temp_dir("outside-default-config");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    fs::write(
        ws.join("nml-project.nml"),
        "project P:\n    modifiers = [\"allow\"]\n",
    )
    .expect("write project file");
    // A model for `thing` (indexed into the registry both files validate
    // under): a block with no model is not walked, and the modifier
    // check falls through a model that declares no modifier fields to
    // the CONFIG's modifier set.
    fs::write(ws.join("core.model.nml"), "model thing:\n    v string?\n").expect("write model");
    let text = "thing t:\n    |deny = []\n";
    let inside = ws.join("inside.nml");
    fs::write(&inside, text).expect("write inside");
    let elsewhere = base.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("create elsewhere");
    let outside = elsewhere.join("outside.nml");
    fs::write(&outside, text).expect("write outside");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let codes = |report: &Value| -> Vec<String> {
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .map(|d| d["code"].as_str().unwrap_or("").to_string())
            .collect()
    };
    let inside_codes = codes(&harness.open(&inside, text).await);
    assert!(
        inside_codes.iter().any(|c| c == "NML2002"),
        "the root's config governs the file inside it: {inside_codes:?}"
    );
    let outside_codes = codes(&harness.open(&outside, text).await);
    assert!(
        !outside_codes.iter().any(|c| c == "NML2002"),
        "a file outside every root reads the embedder default: {outside_codes:?}"
    );
}

/// r86 (r85 decision 1): under a universe whose live input FAILED TO LOAD
/// (NML2088) the editor validates nothing the manifest would govern — the
/// NML2088 row is the whole report on a tenant file, as `nml check` exits 1
/// before any target (it used to report NML2064 "no binding governs this
/// file in the closed universe … (1 manifest(s) discovered)" — false: the
/// manifest exists and failed — and findings from the registry, a verdict
/// the CLI never gives). The universe's OWN inputs keep their findings:
/// the manifest whose meta-validation failure IS the load error reports
/// its own finding ONCE — the NML2088 twin at the same place is folded
/// into it, the load named as a related location (RFC 0026 decision 6) —
/// so the operator repairs it where they look.
#[tokio::test]
async fn an_unloadable_universe_validates_nothing_it_would_govern() {
    let base = temp_dir("unloadable-refused");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu")).expect("create workspace");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace");
    fs::copy(fixture.join("core.model.nml"), ws.join("core.model.nml")).expect("model");
    // The manifest fails meta-validation (`versio`): NML2088, located.
    let manifest = ws.join("demo.package.nml");
    let manifest_text = fs::read_to_string(fixture.join("demo.package.nml"))
        .expect("manifest")
        .replace("version = \"0.1.0\"", "versio = \"0.1.0\"");
    fs::write(&manifest, &manifest_text).expect("write manifest");
    let tenant = ws.join("tenants/cu/plain.flow.nml");
    let text = "thing t\n    v = = 1\n\nthing u uses t:\n    v = \"x\"\n";
    fs::write(&tenant, text).expect("write tenant");
    let codes = |report: &Value| -> Vec<String> {
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .map(|d| d["code"].as_str().unwrap_or("").to_string())
            .collect()
    };
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&tenant, text).await;
    assert_eq!(codes(&report), ["NML2088"], "{report}");
    // The row's place — the manifest's first finding, `versio` at 2:5 —
    // is the row's own location, once: on the tenant document it
    // travels as related information into the manifest (the sentence
    // names no line).
    let row = &report["diagnostics"][0];
    assert!(
        row["message"]
            .as_str()
            .expect("message")
            .contains("manifest failed to load (finding 1 of 2): "),
        "{report}"
    );
    let related = &row["relatedInformation"][0];
    assert!(
        related["location"]["uri"]
            .as_str()
            .is_some_and(|u| u.ends_with("demo.package.nml")),
        "{report}"
    );
    assert_eq!(related["location"]["range"]["start"]["line"], 1, "{report}");
    assert_eq!(
        related["location"]["range"]["start"]["character"], 4,
        "{report}"
    );
    let report = harness.open(&manifest, &manifest_text).await;
    let manifest_codes = codes(&report);
    assert!(
        !manifest_codes.iter().any(|c| c == "NML2088"),
        "the twin is folded into the manifest's own finding: {report}"
    );
    // On the manifest document the finding is ONE row, AT its place —
    // the document's own (NML2001, `versio`), carrying the load note.
    let at_finding: Vec<&Value> = report["diagnostics"]
        .as_array()
        .expect("rows")
        .iter()
        .filter(|d| d["range"]["start"]["line"] == 1 && d["range"]["start"]["character"] == 4)
        .collect();
    assert_eq!(at_finding.len(), 1, "one row at the finding: {report}");
    assert_eq!(at_finding[0]["code"], json!("NML2001"), "{report}");
    assert!(
        carries_load_note(at_finding[0]),
        "the load named on the row: {report}"
    );
    // A SCHEMA SOURCE is an input document too — the third arm of the
    // editor's test, beside the manifest and the project config, and the
    // one that reads the kernel's schema-source admission. Under the same
    // unloadable universe it keeps its OWN findings, because it is where
    // the operator repairs them: NML2009 is the SCHEMA pass's word, which
    // a Refused resolution would not reach (NML1000 is the parse band's
    // and rides either way; NML2088 is the universe's note).
    let model = ws.join("core.model.nml");
    let model_text = "model thing:\n    v string\n\nmodel thing:\n    v string\n";
    fs::write(&model, model_text).expect("write model");
    let report = harness.open(&model, model_text).await;
    let model_codes = codes(&report);
    assert!(
        model_codes.iter().any(|c| c == "NML2009"),
        "a schema source keeps its own schema findings under an unloadable universe: {report}"
    );
    assert!(
        model_codes.iter().any(|c| c == "NML2088"),
        "and still carries the universe's load error: {report}"
    );
}

/// A `[]directive` entry redeclaring one of the language's merge-policy
/// directives is the manifest's refusal under the rule's OWN code
/// (NML2082, RFC 0026 decision 1), as the grant's NML2081 rides: on the
/// manifest document the row sits AT the entry; on a governed file it is
/// the whole report, at the top, the entry a related location away.
#[tokio::test]
async fn a_reserved_directive_is_nml2082_on_both_documents() {
    let base = temp_dir("reserved-directive");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, manifest, manifest_text, tenant, text) = grant_workspace(
        &base,
        "\n[]directive directives:\n    - sealed:\n        arg = \"none\"\n        doc = \"Our own seal.\"\n",
    );
    let entry_line = manifest_text
        .lines()
        .position(|l| l.contains("- sealed:"))
        .expect("the entry");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&manifest, &manifest_text).await;
    assert_eq!(codes_of(&report), ["NML2082"], "{report}");
    let row = &report["diagnostics"][0];
    assert_eq!(
        row["range"]["start"],
        json!({ "line": entry_line, "character": 6 }),
        "at the entry: {row}"
    );
    assert!(
        row["message"]
            .as_str()
            .is_some_and(|m| m
                .starts_with("manifest failed to load: `[]directive` entry 'sealed' redeclares")),
        "{row}"
    );
    let report = harness.open(&tenant, &text).await;
    assert_eq!(codes_of(&report), ["NML2082"], "{report}");
    let row = &report["diagnostics"][0];
    assert_eq!(
        row["range"]["start"],
        json!({ "line": 0, "character": 0 }),
        "{row}"
    );
    let related = &row["relatedInformation"][0];
    assert_eq!(
        related["location"]["uri"],
        json!(file_uri(&manifest)),
        "{row}"
    );
    assert_eq!(
        related["location"]["range"]["start"]["line"],
        json!(entry_line),
        "{row}"
    );
}

/// A repository the r88 P2 pins open documents from OUTSIDE every
/// workspace folder: a `.git` fence, the operator's manifest and its
/// source at the top, a strict tenant binding, a tenant file with two
/// findings under it.
fn fenced_repo(base: &Path) -> PathBuf {
    let repo = base.join("repo");
    fs::create_dir_all(repo.join(".git")).expect("create .git");
    fs::create_dir_all(repo.join("tenants/cu/flows")).expect("create tenant");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace");
    for f in ["demo.package.nml", "core.model.nml"] {
        fs::copy(fixture.join(f), repo.join(f)).expect(f);
    }
    fs::write(
        repo.join("tenants/cu/flows/bad.flow.nml"),
        "thing a:\n    v = 1\n    w = \"extra\"\n",
    )
    .expect("write bad");
    repo
}

/// Whether `row` carries the folded universe row's context — the related
/// location the manifest's own finding gains when the NML2088 twin at
/// the same place is folded into it (RFC 0026 decision 6).
fn carries_load_note(row: &Value) -> bool {
    row["relatedInformation"].as_array().is_some_and(|rel| {
        rel.iter().any(|r| {
            r["message"] == json!("the manifest fails to load here (NML2088)")
                && r["location"]["range"] == row["range"]
        })
    })
}

fn codes_of(report: &Value) -> Vec<String> {
    report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .map(|d| d["code"].as_str().unwrap_or("").to_string())
        .collect()
}

/// r88 P2: a document outside every workspace folder resolves under the
/// root the KERNEL derives — the `.git` fence, the outermost marker
/// within it — exactly as `nml check <file>` does: the same binding, the
/// same two strict-error verdicts with the same identity suffix, the
/// origin on the wire (`derivedVcsFence`), the derivation said once as a
/// log message. A derived universe lives while a buffer sits under it:
/// once the document is closed and another document resolves, it is
/// dropped, and reopening derives (and says) it again. (The document
/// used to be unbound under the embedder default: no findings at all.)
#[tokio::test]
async fn a_document_outside_every_folder_resolves_under_the_kernels_derived_root() {
    let base = temp_dir("derived-root");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let repo = fenced_repo(&base);
    let bad = repo.join("tenants/cu/flows/bad.flow.nml");
    let text = fs::read_to_string(&bad).expect("bad");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_folderless().await;
    let report = harness.open(&bad, &text).await;
    let codes = codes_of(&report);
    assert_eq!(codes, ["NML2008", "NML2001"], "{report}");
    for d in report["diagnostics"].as_array().unwrap() {
        assert_eq!(
            d["severity"],
            json!(1),
            "strict: errors, as the CLI says: {d}"
        );
        assert!(
            d["message"]
                .as_str()
                .unwrap()
                .contains("(schema: demo blake3:"),
            "the binding's identity: {d}"
        );
    }
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&bad) }))
        .await;
    assert_eq!(info["bound"], json!(true), "{info}");
    assert_eq!(info["binding"], json!("tenantFlows"), "{info}");
    assert_eq!(info["rootOrigin"], json!("derivedVcsFence"), "{info}");
    let derived_log = format!(
        "derived a workspace root at `{}` (derivedVcsFence) for documents outside every \
         workspace folder",
        message_path(&repo)
    );
    let mut said = 0;
    loop {
        let params = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        let message = params["message"].as_str().expect("a message");
        if message.contains(&derived_log) {
            said += 1;
            break;
        }
    }
    // Retention: close the document, let another document resolve
    // (which prunes derived roots no buffer sits under), reopen: derived
    // and said again.
    harness
        .notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": file_uri(&bad) } }),
        )
        .await;
    let loose = base.join("loose.nml");
    fs::write(&loose, "\n").expect("write loose");
    harness.open(&loose, "\n").await;
    harness.open(&bad, &text).await;
    loop {
        let params = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        let message = params["message"].as_str().expect("a message");
        if message.contains(&derived_log) {
            said += 1;
            break;
        }
    }
    assert_eq!(said, 2, "derived once per retention");
}

/// r88 P2: the kernel REFUSES to derive a root for a document under a
/// planted `.git` FILE below the operator's marker (E21's shadow rule —
/// `nml check` exits 2 there): the editor shows one row — the kernel's
/// sentence, then this front end's advice — and validates NOTHING (the
/// file's own type error never appears); `nml/schemaInfo` carries it.
#[tokio::test]
async fn a_planted_git_file_below_the_marker_refuses_derivation_in_the_editor() {
    let base = temp_dir("derived-refused");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let repo = fenced_repo(&base);
    fs::create_dir_all(repo.join("tenants/evil/flows")).expect("create evil");
    fs::write(repo.join("tenants/evil/.git"), "gitdir: /nowhere\n").expect("plant");
    let evil = repo.join("tenants/evil/flows/x.flow.nml");
    let text = "thing a:\n    v = 1\n";
    fs::write(&evil, text).expect("write evil");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_folderless().await;
    let report = harness.open(&evil, text).await;
    let rows = report["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(rows.len(), 1, "{report}");
    assert_eq!(rows[0]["severity"], json!(1), "{report}");
    assert!(
        rows[0]["code"].is_null(),
        "uncoded, like the CLI's usage error: {report}"
    );
    let message = rows[0]["message"].as_str().expect("message");
    assert!(
        message.starts_with("cannot derive a workspace root for this document: the root marker `")
            && message.contains("sits above the fence at `")
            && message.ends_with("— open its workspace folder, which fixes the universe"),
        "{message}"
    );
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&evil) }))
        .await;
    assert_eq!(info["bound"], json!(false), "{info}");
    assert_eq!(info["notes"][0]["message"], json!(message), "{info}");
    assert!(info["rootOrigin"].is_null(), "{info}");
}

/// r88 P2 (r86 F11): a project config BESIDE a folder-less document
/// governs it through the kernel's nearest live config — the effect the
/// global config's deletion (D8) removed by accident returns by design:
/// `modifiers = ["allow"]` in `nml-project.nml` makes `|deny` NML2002.
/// With no `.git` above, the fence is the document's own directory
/// (`derivedTargetDir`), as for `nml check <file>`.
#[tokio::test]
async fn the_project_config_beside_a_folderless_document_governs_it() {
    let base = temp_dir("derived-config");
    if base.ancestors().any(|d| d.join(".git").exists()) {
        return; // a checkout above the temp dir: the fence would be its own
    }
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let loose = base.join("loose");
    fs::create_dir_all(&loose).expect("create loose");
    fs::write(
        loose.join("nml-project.nml"),
        "project P:\n    modifiers = [\"allow\"]\n",
    )
    .expect("write project file");
    let x = loose.join("x.nml");
    let text = "thing t:\n    |deny = []\n";
    fs::write(&x, text).expect("write x");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_folderless().await;
    // A derived root is not indexed (the registry stays the folders'):
    // the model the modifier check needs is an OPEN buffer here, as it
    // is in a single-file session.
    let model = "model thing:\n    v string?\n";
    fs::write(loose.join("core.model.nml"), model).expect("write model");
    harness.open(&loose.join("core.model.nml"), model).await;
    let codes = codes_of(&harness.open(&x, text).await);
    assert!(
        codes.iter().any(|c| c == "NML2002"),
        "the config beside the document governs it: {codes:?}"
    );
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&x) }))
        .await;
    assert_eq!(info["rootOrigin"], json!("derivedTargetDir"), "{info}");
}

/// r88 P2 (R1): INSIDE a workspace folder the folder is the universe —
/// never a derivation. A folder opened at `repo/tenants` under a
/// manifest at `repo/` sees no manifest: the document is unbound in an
/// open universe (`rootOrigin: editor`), where `nml check <file>` would
/// derive `repo` and bind it — the one deliberate divergence, the
/// editor's root being the user's explicit choice.
#[tokio::test]
async fn inside_a_folder_the_folder_wins_never_a_derivation() {
    let base = temp_dir("folder-wins");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let repo = fenced_repo(&base);
    let bad = repo.join("tenants/cu/flows/bad.flow.nml");
    let text = fs::read_to_string(&bad).expect("bad");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&repo.join("tenants")).await;
    let codes = codes_of(&harness.open(&bad, &text).await);
    assert!(!codes.iter().any(|c| c == "NML2008"), "{codes:?}");
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&bad) }))
        .await;
    assert_eq!(info["bound"], json!(false), "{info}");
    assert_eq!(info["rootOrigin"], json!("editor"), "{info}");
}

/// r88 P2: workspace folders added or removed while the server runs are
/// honoured (`workspace/didChangeWorkspaceFolders`, declared in the
/// capabilities): a document derived outside every folder resolves under
/// the folder once it is added (`editor`, indexed and said), and derives
/// again once the folder is removed. (Folders were read at `initialize`
/// only: a folder added later was invisible until a restart.)
#[tokio::test]
async fn workspace_folder_changes_are_honoured() {
    let base = temp_dir("folder-changes");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let repo = fenced_repo(&base);
    let bad = repo.join("tenants/cu/flows/bad.flow.nml");
    let text = fs::read_to_string(&bad).expect("bad");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_folderless().await;
    let origin = |info: &Value| info["rootOrigin"].as_str().unwrap_or("").to_string();
    harness.open(&bad, &text).await;
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&bad) }))
        .await;
    assert_eq!(origin(&info), "derivedVcsFence", "{info}");
    harness.change_folders(&[&repo], &[]).await;
    loop {
        let params = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        if params["message"].as_str().unwrap_or("") == "NML: indexed 1 workspace root(s)" {
            break;
        }
    }
    let report = harness.diagnostics(&file_uri(&bad)).await;
    assert_eq!(
        codes_of(&report),
        ["NML2008", "NML2001"],
        "still the binding's: {report}"
    );
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&bad) }))
        .await;
    assert_eq!(origin(&info), "editor", "{info}");
    harness.change_folders(&[], &[&repo]).await;
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&bad) }))
        .await;
    assert_eq!(origin(&info), "derivedVcsFence", "{info}");
}

/// r88 P4: an OPEN BUFFER past `MAX_INDEX_BYTES` is refused at the store
/// — one row in the kernel's cap sentence (uncoded, like `nml check`'s
/// refusal of the same file as a target), NO parse (the buffer's own
/// syntax error never appears; the parse counter does not move), hover
/// answers nothing, `nml/schemaInfo` carries the note — and a change
/// under the bound validates as usual; a buffer AT the bound is parsed.
/// Measured on the r87 tree: a 300 MiB buffer cost 244 s and 11 GB and
/// reported GREEN.
#[tokio::test]
async fn an_open_buffer_past_the_index_bound_is_refused_with_one_row() {
    let base = temp_dir("buffer-oversized");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let app = ws.join("app.nml");
    fs::write(&app, "\n").expect("write app");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let broken = "thing t\n    v = = 1\n";
    let mut over = broken.to_string();
    over.push_str(&" ".repeat(MAX_INDEX_BYTES + 1 - broken.len()));
    let parses_before = nml_core::cst::parses_on_this_thread();
    let report = harness.open(&app, &over).await;
    assert_eq!(
        nml_core::cst::parses_on_this_thread(),
        parses_before,
        "a refused buffer is never parsed"
    );
    let rows = report["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(rows.len(), 1, "{report}");
    assert_eq!(rows[0]["severity"], json!(1), "{report}");
    assert!(
        rows[0]["code"].is_null(),
        "uncoded, like the CLI's target refusal: {report}"
    );
    assert_eq!(
        rows[0]["message"],
        json!(format!(
            "too large: over 16 MiB ({} bytes) — an open document is read only up to 16 MiB \
             (16777216 bytes)",
            MAX_INDEX_BYTES + 1
        )),
        "{report}"
    );
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&app) }))
        .await;
    assert_eq!(info["bound"], json!(false), "{info}");
    assert_eq!(info["notes"][0]["message"], rows[0]["message"], "{info}");
    assert_eq!(info["notes"][0]["severity"], json!("error"), "{info}");
    let hover = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                "position": { "line": 0, "character": 0 },
            }),
        )
        .await;
    assert!(
        hover.is_null(),
        "nothing to hover on a refused buffer: {hover}"
    );
    // A change under the bound is validated as usual: the syntax error appears.
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&app), "version": 2 },
                "contentChanges": [{ "text": broken }],
            }),
        )
        .await;
    let report = harness.diagnostics(&file_uri(&app)).await;
    let codes: Vec<&str> = report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(codes.iter().any(|c| c.starts_with("NML000")), "{report}");
    // Exactly the bound is parsed.
    let at_cap = ws.join("cap.nml");
    let mut text = broken.to_string();
    text.push_str(&" ".repeat(MAX_INDEX_BYTES - broken.len()));
    let report = harness.open(&at_cap, &text).await;
    let messages: Vec<&str> = report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter_map(|d| d["message"].as_str())
        .collect();
    assert!(
        !messages.iter().any(|m| m.starts_with("too large")) && !messages.is_empty(),
        "a buffer at the bound is validated: {report}"
    );
}

/// r88 P1: a file whose binding cannot build its validator (a declared
/// source with a parse error) is REFUSED with the kernel's one NML2091
/// row — no parse band, no registry pass (the file's own type error must
/// not appear) — and the row's `relatedInformation` points at the
/// source's first finding in the source's own file, through the same
/// locator a finding's notes use. The source document keeps its own parse
/// errors; a file under the manifest's OTHER binding validates as usual;
/// `nml/schemaInfo` carries the row. The editor used to bind the file and
/// "fall back to basic validation" under an uncoded warning.
#[tokio::test]
async fn a_binding_whose_validator_cannot_be_built_refuses_the_files_it_governs() {
    let base = temp_dir("validator-unbuildable");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu")).expect("create workspace");
    fs::create_dir_all(ws.join("docs")).expect("create docs");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace-brokensrc");
    for f in ["demo.package.nml", "core.model.nml", "good.model.nml"] {
        fs::copy(fixture.join(f), ws.join(f)).expect(f);
    }
    let tenant = ws.join("tenants/cu/plain.flow.nml");
    // A syntax error too: a refused file shows no parse band either.
    let tenant_text = "thing a\n    v = = 1\n";
    fs::write(&tenant, tenant_text).expect("write tenant");
    let doc = ws.join("docs/readme.doc.nml");
    let doc_text = "note n:\n    text = 1\n";
    fs::write(&doc, doc_text).expect("write doc");
    let codes = |report: &Value| -> Vec<String> {
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .map(|d| d["code"].as_str().unwrap_or("").to_string())
            .collect()
    };
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&tenant, tenant_text).await;
    assert_eq!(codes(&report), ["NML2091"], "{report}");
    // A refused document is never PARSED: a change (defeating the
    // diagnostics cache) and a pull move the parse counter by nothing —
    // the universe and the memoized build are cached, the document's
    // own text is not parsed.
    let parses_before = nml_core::cst::parses_on_this_thread();
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&tenant), "version": 2 },
                "contentChanges": [{ "text": "thing a\n    v = = 2\n" }],
            }),
        )
        .await;
    let again = harness.diagnostics(&file_uri(&tenant)).await;
    assert_eq!(codes(&again), ["NML2091"], "{again}");
    assert_eq!(
        nml_core::cst::parses_on_this_thread(),
        parses_before,
        "a refused document is never parsed"
    );
    let row = &report["diagnostics"][0];
    assert_eq!(row["severity"], json!(1), "an error: {row}");
    assert!(
        row["message"].as_str().expect("message").starts_with(
            "binding 'tenantFlows' of demo.package.nml cannot build its validator: declared \
                 source `core` failed to load at core.model.nml:3:1 (finding 1 of 5): "
        ),
        "{row}"
    );
    let related = &row["relatedInformation"][0];
    assert_eq!(
        related["location"]["uri"],
        json!(file_uri(&ws.join("core.model.nml"))),
        "located in the source's own file: {row}"
    );
    assert_eq!(
        related["location"]["range"]["start"]["line"],
        json!(2),
        "{row}"
    );
    assert_eq!(
        related["location"]["range"]["start"]["character"],
        json!(0),
        "{row}"
    );
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&tenant) }))
        .await;
    assert_eq!(info["bound"], json!(false), "{info}");
    assert_eq!(info["notes"][0]["code"], json!("NML2091"), "{info}");
    // The source document reports itself; the sibling binding validates.
    let source_text = fs::read_to_string(ws.join("core.model.nml")).expect("source");
    let source_codes = codes(&harness.open(&ws.join("core.model.nml"), &source_text).await);
    assert!(
        source_codes.iter().any(|c| c == "NML0006") && !source_codes.iter().any(|c| c == "NML2091"),
        "{source_codes:?}"
    );
    let doc_codes = codes(&harness.open(&doc, doc_text).await);
    assert!(
        doc_codes.iter().any(|c| c == "NML2008") && !doc_codes.iter().any(|c| c == "NML2091"),
        "the good binding validates the doc: {doc_codes:?}"
    );
}

/// A workspace folder ADDED over a root the kernel had derived — the
/// same path, `editor` now — re-anchors its documents even when the
/// folder's index adds no document (a repository holding only the open
/// file): the universe cache is keyed on its anchor, not its path alone.
/// (The cached derived universe used to answer `derivedVcsFence` for the
/// folder's documents until something else invalidated it.) Removing
/// the folder derives again.
#[tokio::test]
async fn a_folder_added_over_a_derived_root_reanchors_its_documents() {
    let base = temp_dir("folder-over-derived");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let repo = base.join("repo");
    fs::create_dir_all(repo.join(".git")).expect("create .git");
    let x = repo.join("x.nml");
    let text = "thing t:\n    v = 1\n";
    fs::write(&x, text).expect("write x");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_folderless().await;
    let origin = |info: &Value| info["rootOrigin"].as_str().unwrap_or("").to_string();
    harness.open(&x, text).await;
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&x) }))
        .await;
    assert_eq!(origin(&info), "derivedVcsFence", "{info}");
    assert_eq!(info["rootFence"], json!("dir"), "{info}");
    assert!(info["rootShadowed"].is_null(), "{info}");
    harness.change_folders(&[&repo], &[]).await;
    loop {
        let params = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        if params["message"].as_str().unwrap_or("") == "NML: indexed 1 workspace root(s)" {
            break;
        }
    }
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&x) }))
        .await;
    assert_eq!(
        origin(&info),
        "editor",
        "the folder is the anchor now, though its index added nothing: {info}"
    );
    assert!(info["rootFence"].is_null(), "{info}");
    harness.change_folders(&[], &[&repo]).await;
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&x) }))
        .await;
    assert_eq!(origin(&info), "derivedVcsFence", "{info}");
}

/// A derived universe's facts the CLI's root note discloses — a fence
/// that is no directory (a linked worktree's, a submodule's or a planted
/// `.git` file), a root marker above a directory fence — are disclosed
/// by the editor too: the kernel's one fact sentence in the derivation's
/// log line, as a WARNING with the editor's advice, and `rootFence` /
/// `rootShadowed` on `nml/schemaInfo` in the `--json` root object's
/// vocabulary. (The editor used to say `derivedVcsFence` and nothing
/// more.)
#[tokio::test]
async fn a_derived_fence_that_is_no_directory_or_a_shadow_above_it_is_disclosed() {
    let base = temp_dir("derived-disclosed");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace");
    // A linked worktree: a `.git` FILE fence, the manifest inside it.
    let wt = base.join("wt");
    fs::create_dir_all(wt.join("tenants/cu/flows")).expect("create wt");
    fs::write(wt.join(".git"), "gitdir: /nowhere/.git/worktrees/wt\n").expect("plant");
    for f in ["demo.package.nml", "core.model.nml"] {
        fs::copy(fixture.join(f), wt.join(f)).expect(f);
    }
    let a = wt.join("tenants/cu/flows/a.flow.nml");
    let text = "thing a:\n    v = \"x\"\n";
    fs::write(&a, text).expect("write a");
    // A checkout nested under a workspace manifest: a marker above a
    // directory fence.
    let outer = base.join("outer");
    fs::create_dir_all(outer.join("inner/.git")).expect("create inner .git");
    fs::create_dir_all(outer.join("inner/tenants/cu/flows")).expect("create inner");
    for f in ["demo.package.nml", "core.model.nml"] {
        fs::copy(fixture.join(f), outer.join(f)).expect(f);
    }
    let b = outer.join("inner/tenants/cu/flows/b.flow.nml");
    fs::write(&b, text).expect("write b");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_folderless().await;
    let report = harness.open(&a, text).await;
    assert_eq!(codes_of(&report), Vec::<String>::new(), "{report}");
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&a) }))
        .await;
    assert_eq!(info["bound"], json!(true), "{info}");
    assert_eq!(info["rootOrigin"], json!("derivedVcsFence"), "{info}");
    assert_eq!(info["rootFence"], json!("file"), "{info}");
    assert!(info["rootShadowed"].is_null(), "{info}");
    let expected = format!(
        "derived a workspace root at `{}` (derivedVcsFence, within a .git FILE fence — a linked \
         worktree's, a submodule's or a planted entry) for documents outside every workspace \
         folder — open a workspace folder to fix the universe",
        message_path(&wt)
    );
    loop {
        let params = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        if params["message"].as_str().unwrap_or("") == expected {
            assert_eq!(params["type"], json!(2), "a warning: {params}");
            break;
        }
    }
    harness.open(&b, text).await;
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&b) }))
        .await;
    assert_eq!(info["bound"], json!(false), "{info}");
    assert_eq!(info["rootOrigin"], json!("derivedVcsFence"), "{info}");
    assert_eq!(info["rootFence"], json!("dir"), "{info}");
    assert_eq!(
        info["rootShadowed"],
        json!("../../../../demo.package.nml"),
        "spelled from the derived root: {info}"
    );
    let expected = format!(
        "derived a workspace root at `{}` (derivedVcsFence, within the .git fence; SHADOWED by \
         the root marker `{}` above it) for documents outside every workspace folder — open a \
         workspace folder to fix the universe",
        message_path(&outer.join("inner/tenants/cu/flows")),
        message_path(&outer.join("demo.package.nml"))
    );
    loop {
        let params = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        if params["message"].as_str().unwrap_or("") == expected {
            assert_eq!(params["type"], json!(2), "a warning: {params}");
            break;
        }
    }
}

/// Closing an INDEXED document restores its disk copy, under the index's
/// bound (LSP: after `didClose` the truth is the disk's): a model
/// buffer grown past the bound (refused — the registry lost its model)
/// and closed is indexed again from disk, and an unsaved edit goes with
/// the buffer. (The last buffer text used to stay in the index; the
/// refused one left the document absent until a watcher event.)
#[tokio::test]
async fn closing_an_indexed_document_restores_its_disk_copy_under_the_bound() {
    let base = temp_dir("close-reindexes");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model = ws.join("core.model.nml");
    let model_text = "model thing:\n    v string\n";
    fs::write(&model, model_text).expect("write model");
    let app = ws.join("app.nml");
    let app_text = "thing t:\n    v = \"x\"\n    w = 1\n";
    fs::write(&app, app_text).expect("write app");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let unknown_w = |report: &Value| codes_of(report).iter().any(|c| c == "NML2001");
    let report = harness.open(&app, app_text).await;
    assert!(
        unknown_w(&report),
        "the indexed model knows no `w`: {report}"
    );
    harness.open(&model, model_text).await;
    let mut over = model_text.to_string();
    over.push_str(&" ".repeat(MAX_INDEX_BYTES + 1 - model_text.len()));
    let change = |text: String| {
        json!({
            "textDocument": { "uri": file_uri(&model), "version": 2 },
            "contentChanges": [{ "text": text }],
        })
    };
    harness.notify("textDocument/didChange", change(over)).await;
    let report = harness.diagnostics(&file_uri(&app)).await;
    assert!(
        !unknown_w(&report),
        "the refused model buffer left the registry: {report}"
    );
    harness
        .notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": file_uri(&model) } }),
        )
        .await;
    let report = harness.diagnostics(&file_uri(&app)).await;
    assert!(
        unknown_w(&report),
        "closed: the disk copy is indexed again: {report}"
    );
    // An unsaved edit goes with the buffer.
    harness.open(&model, model_text).await;
    harness
        .notify(
            "textDocument/didChange",
            change("model thing:\n    v string\n    w number\n".to_string()),
        )
        .await;
    let report = harness.diagnostics(&file_uri(&app)).await;
    assert!(!unknown_w(&report), "the buffer's `w` is known: {report}");
    harness
        .notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": file_uri(&model) } }),
        )
        .await;
    let report = harness.diagnostics(&file_uri(&app)).await;
    assert!(
        unknown_w(&report),
        "closed: the disk's model, without `w`: {report}"
    );
}

/// r86 (mutant MR22b survived): D7's second half — a file under a budget
/// UNIT the walk could not finish (an unreadable directory inside the
/// tenant's subtree) is REFUSED too: the unit's NML2089 row is the whole
/// report, no parse band, no composition — while a sibling tenant's file
/// validates as usual. The whole-universe half was pinned; this half was
/// not, and dropping it survived every editor pin.
#[cfg(unix)]
#[tokio::test]
async fn a_file_under_a_denied_unit_validates_nothing_in_the_editor() {
    use std::os::unix::fs::PermissionsExt;
    let base = temp_dir("refused-unit");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu/locked")).expect("create workspace");
    fs::create_dir_all(ws.join("tenants/du")).expect("create du");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace");
    for f in ["demo.package.nml", "core.model.nml"] {
        fs::copy(fixture.join(f), ws.join(f)).expect(f);
    }
    let text = "thing t\n    v = = 1\n";
    let cu = ws.join("tenants/cu/plain.flow.nml");
    let du = ws.join("tenants/du/plain.flow.nml");
    fs::write(&cu, text).expect("write cu");
    fs::write(&du, text).expect("write du");
    let locked = ws.join("tenants/cu/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
    if fs::read_dir(&locked).is_ok() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod");
        return; // root: the lock does not bite
    }
    let codes = |report: &Value| -> Vec<String> {
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .map(|d| d["code"].as_str().unwrap_or("").to_string())
            .collect()
    };
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let cu_report = harness.open(&cu, text).await;
    let du_report = harness.open(&du, text).await;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_eq!(codes(&cu_report), ["NML2089"], "{cu_report}");
    assert!(
        cu_report["diagnostics"][0]["message"]
            .as_str()
            .expect("message")
            .starts_with("discovery under `tenants/cu` was cut short"),
        "{cu_report}"
    );
    let du_codes = codes(&du_report);
    assert!(
        !du_codes.is_empty() && !du_codes.iter().any(|c| c == "NML2089"),
        "the sibling tenant validates as usual: {du_report}"
    );
}

/// A folder added while the server runs is indexed exactly as at
/// `initialized`: its denials are SAID — a live manifest that failed to
/// load is a `window/logMessage` WARNING (`[NML2088] manifest failed to
/// load: …`) BEFORE the `indexed N workspace root(s)` line. (The
/// runtime path had no pin with a denial to say.)
#[tokio::test]
async fn a_folder_added_at_runtime_says_its_denials() {
    let base = temp_dir("added-folder-denial");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create folder");
    fs::write(
        ws.join("demo.package.nml"),
        "package demo:\n    version = \"0.1.0\"\n",
    )
    .expect("write a manifest that cannot load");
    fs::write(ws.join("x.flow.nml"), "\n").expect("write a file");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_folderless().await;
    harness.change_folders(&[&ws], &[]).await;
    let mut said = false;
    loop {
        let params = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        let message = params["message"].as_str().unwrap_or("").to_string();
        if message.starts_with("NML: [NML2088] manifest failed to load:") {
            assert_eq!(params["type"], json!(2), "a WARNING: {params}");
            said = true;
        }
        if message == "NML: indexed 1 workspace root(s)" {
            break;
        }
    }
    assert!(
        said,
        "the added folder's denial is said before the indexed line"
    );
}

/// The binding's `layers:` grant is the editor's verdict too (one grant
/// provider for both front ends; RFC 0026 B-1 brought the manifest side
/// forward): a `denyRefs` veto is NML2065 on the document, by rule index,
/// at error severity; an allowlist that admits the file composes clean;
/// and a binding WITHOUT the block is NML2064 whose remedy is a related
/// location IN THE MANIFEST, at the binding — the same three answers
/// `nml check` gives. (No fixture carried a grant: the editor's grant
/// path was pinned by the kernel's tests alone.)
#[tokio::test]
async fn a_grants_deny_veto_and_allowed_stack_reach_the_editor() {
    let manifest = |layers: &str| {
        format!(
            "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    - core:\n        \
             file = \"core.model.nml\"\n\n[]validator validators:\n    - tenantFlows:\n        files:\n            - \
             \"tenants/**/*.flow.nml\"\n        schemas:\n            - core\n        strict = true\n{layers}"
        )
    };
    let text = "thing base:\n    v = \"b\"\n\nthing t uses base:\n    v = \"t\"\n";
    let veto = "        layers:\n            allowRefs:\n                - \"tenants/**\"\n            denyRefs:\n                - \"tenants/cu/**\"\n";
    let allowed = "        layers:\n            allowRefs:\n                - \"tenants/**\"\n";
    for (tag, layers, want) in [
        ("veto", veto, Some("NML2065")),
        ("allowed", allowed, None),
        ("nogrant", "", Some("NML2064")),
    ] {
        let base = temp_dir(&format!("grant-{tag}"));
        let store_base = base.join("store");
        fs::create_dir_all(&store_base).expect("create store dir");
        let ws = base.join("ws");
        fs::create_dir_all(ws.join("tenants/cu")).expect("create tenant");
        fs::write(ws.join("demo.package.nml"), manifest(layers)).expect("manifest");
        workspace_fixture(&ws, &["core.model.nml"]);
        let file = ws.join("tenants/cu/member-lookup.flow.nml");
        fs::write(&file, text).expect("write tenant file");
        let mut harness = Harness::new(Store::at(&store_base));
        harness.initialize(&ws).await;
        let report = harness.open(&file, text).await;
        let codes = codes_of(&report);
        match want {
            Some(code) => {
                assert_eq!(codes, [code], "{tag}: {report}");
                let d = &report["diagnostics"][0];
                assert_eq!(d["severity"], json!(1), "{tag}: an error: {d}");
                let message = d["message"].as_str().expect("message");
                if code == "NML2065" {
                    assert!(
                        message.contains(
                            "denied by denyRefs[0] of binding 'tenantFlows' (demo.package.nml)"
                        ),
                        "{tag}: {d}"
                    );
                } else {
                    assert!(message.contains("carries no `layers:` grant"), "{tag}: {d}");
                    // The remedy: a related location at the binding in the
                    // manifest, naming the key to admit.
                    let related = &d["relatedInformation"][0];
                    let uri = related["location"]["uri"].as_str().expect("uri");
                    assert!(uri.ends_with("/demo.package.nml"), "{tag}: {related}");
                    assert_eq!(
                        related["location"]["range"]["start"]["line"],
                        json!(9),
                        "{tag}: {related}"
                    );
                    assert_eq!(
                        related["location"]["range"]["start"]["character"],
                        json!(6),
                        "{tag}: {related}"
                    );
                    assert!(
                        related["message"]
                            .as_str()
                            .expect("note")
                            .contains("admits \"tenants/cu/member-lookup.flow.nml\""),
                        "{tag}: {related}"
                    );
                }
            }
            None => assert!(codes.is_empty(), "{tag}: composes clean: {report}"),
        }
    }
}

/// The `workspace` fixture's files named, copied into a scratch workspace.
fn workspace_fixture(ws: &Path, files: &[&str]) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace");
    for f in files {
        let to = ws.join(f);
        if let Some(dir) = to.parent() {
            fs::create_dir_all(dir).expect("fixture dir");
        }
        fs::copy(fixture.join(f), to).expect(f);
    }
}

/// `textDocument/codeAction` at `0:0` with an empty context — the actions a
/// document offers on its own (the pin and opt-out among them).
async fn code_actions_at_top(harness: &mut Harness, path: &Path) -> Vec<Value> {
    harness
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": file_uri(path) },
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
                "context": { "diagnostics": [] },
            }),
        )
        .await
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The pin action among `actions`, by its title.
fn pin_action(actions: &[Value]) -> Value {
    actions
        .iter()
        .find(|a| a["title"] == json!("Pin schema package 'demo'"))
        .cloned()
        .unwrap_or_else(|| panic!("no pin action among {actions:?}"))
}

/// A file two live manifests claim is REFUSED in the editor as `nml check`
/// refuses it: exactly ONE row — the kernel's NML2087 in the CLI's sentence,
/// naming every claimant — no parse band, no composition verdict (the
/// ambiguous form of NML2064 used to ride beside it), `nml/schemaInfo`
/// unbound with that row as its one note; the document is never parsed
/// for diagnostics. Narrowing one claim in an UNSAVED manifest buffer
/// binds the file on the next pull (the pull model's full report replaces
/// the refusal — nothing to clear); widening it again refuses it again.
#[tokio::test]
async fn an_ambiguously_claimed_file_is_refused_with_the_clis_one_row() {
    let base = temp_dir("ambiguous-refused");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    workspace_fixture(
        &ws,
        &[
            "demo.package.nml",
            "other.package.nml",
            "core.model.nml",
            "shared/x.flow.nml",
        ],
    );
    let x = ws.join("shared/x.flow.nml");
    let text = fs::read_to_string(&x).expect("read x");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&x, &text).await;
    let rows = report["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(rows.len(), 1, "one row, the CLI's: {report}");
    let row = &rows[0];
    assert_eq!(row["code"], json!("NML2087"), "{row}");
    assert_eq!(row["severity"], json!(1), "an error: {row}");
    assert_eq!(
        row["range"]["start"],
        json!({ "line": 0, "character": 0 }),
        "{row}"
    );
    let message = row["message"].as_str().expect("message");
    assert!(
        message.starts_with(
            "2 manifests claim this file: demo.package.nml (shared, files[0] = \
             \"shared/**/*.flow.nml\"), other.package.nml (sharedToo, files[0] = \
             \"shared/**/*.flow.nml\") — an ambiguously-claimed file is denied: it \
             validates under no binding and nothing runs against it; remove or narrow one claim"
        ),
        "the kernel's sentence, the CLI's: {message}"
    );
    // Never parsed for diagnostics: a change (defeating the cache) and a
    // pull move the parse counter by nothing, and a syntax error in the
    // new text never appears.
    let parses_before = nml_core::cst::parses_on_this_thread();
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&x), "version": 2 },
                "contentChanges": [{ "text": "thing a\n    v = = 2\n" }],
            }),
        )
        .await;
    let again = harness.diagnostics(&file_uri(&x)).await;
    assert_eq!(codes_of(&again), ["NML2087"], "{again}");
    assert_eq!(
        nml_core::cst::parses_on_this_thread(),
        parses_before,
        "a refused document is never parsed"
    );
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&x) }))
        .await;
    assert_eq!(info["bound"], json!(false), "{info}");
    assert_eq!(info["notes"].as_array().map(Vec::len), Some(1), "{info}");
    assert_eq!(info["notes"][0]["code"], json!("NML2087"), "{info}");
    assert_eq!(info["notes"][0]["severity"], json!("error"), "{info}");
    // Refused withholds the VERDICT, not navigation: symbols and hover
    // still answer on the document (they parse for navigation only,
    // independently of validation).
    let symbols = harness
        .request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": file_uri(&x) } }),
        )
        .await;
    assert_eq!(symbols.as_array().map(Vec::len), Some(2), "{symbols}");
    let hover = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(&x) },
                "position": { "line": 0, "character": 6 },
            }),
        )
        .await;
    assert!(
        hover["contents"]["value"].is_string(),
        "hover answers on a refused document: {hover}"
    );
    // The document's real text back (the probe's broken text would be
    // parsed once the file binds), then the transition, both ways,
    // through an unsaved manifest buffer.
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&x), "version": 3 },
                "contentChanges": [{ "text": text }],
            }),
        )
        .await;
    let other = ws.join("other.package.nml");
    let wide = fs::read_to_string(&other).expect("read other");
    let narrow = wide.replace("\"shared/**/*.flow.nml\"", "\"shared/none/**/*.flow.nml\"");
    assert_ne!(narrow, wide, "the fixture's glob is the one narrowed");
    harness.open(&other, &narrow).await;
    let bound = harness.diagnostics(&file_uri(&x)).await;
    assert_eq!(
        codes_of(&bound),
        ["NML2064"],
        "bound under `shared`, whose own verdict is the no-grant form: {bound}"
    );
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&x) }))
        .await;
    assert_eq!(info["bound"], json!(true), "{info}");
    assert_eq!(info["binding"], json!("shared"), "{info}");
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&other), "version": 2 },
                "contentChanges": [{ "text": wide }],
            }),
        )
        .await;
    let refused = harness.diagnostics(&file_uri(&x)).await;
    assert_eq!(codes_of(&refused), ["NML2087"], "refused again: {refused}");
    assert_eq!(refused["diagnostics"].as_array().map(Vec::len), Some(1));
}

/// The pin and opt-out code actions write into the kernel's nearest LIVE
/// `nml-project.nml`, never a config inside claimed content: over the
/// `workspace` fixture — whose `tenants/cu/nml-project.nml` is inert
/// (NML2080) and already says `autoAssociate = false` — both actions
/// are offered and both CREATE `nml-project.nml` at the binding's
/// anchor. A disk walk by `is_file()` used to write the pin into the
/// inert file (changing nothing) and, finding the opt-out "already
/// there", offered no opt-out at all while the file stayed
/// auto-associated.
#[tokio::test]
async fn the_pin_and_opt_out_actions_never_target_an_inert_config() {
    let base = temp_dir("pin-inert");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    workspace_fixture(
        &ws,
        &[
            "demo.package.nml",
            "core.model.nml",
            "tenants/cu/plain.flow.nml",
            "tenants/cu/nml-project.nml",
        ],
    );
    let plain = ws.join("tenants/cu/plain.flow.nml");
    let text = fs::read_to_string(&plain).expect("read plain");
    // A client that declared no `create` resource operation is offered
    // no action that creates a file (LSP 3.17 §WorkspaceEdit: resource
    // operations are the client's to declare) — never an action it
    // cannot apply.
    let mut plain_client = Harness::new(Store::at(&store_base));
    plain_client.initialize(&ws).await;
    plain_client.open(&plain, &text).await;
    let offered = code_actions_at_top(&mut plain_client, &plain).await;
    assert!(
        !offered
            .iter()
            .any(|a| a["edit"]["documentChanges"].is_array()),
        "no file creation for a client that cannot create one: {offered:?}"
    );
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_as_vscode(&ws).await;
    harness.open(&plain, &text).await;
    let actions = code_actions_at_top(&mut harness, &plain).await;
    let titles: Vec<&str> = actions.iter().filter_map(|a| a["title"].as_str()).collect();
    assert!(
        titles.contains(&"Pin schema package 'demo'")
            && titles
                .contains(&"Not a demo project? Disable schema auto-association for this root"),
        "both actions offered: {titles:?}"
    );
    let inert = file_uri(&ws.join("tenants/cu/nml-project.nml"));
    let root_config = file_uri(&ws.join("nml-project.nml"));
    for action in &actions {
        let edit = &action["edit"];
        assert!(
            edit["changes"].is_null(),
            "no edit on an existing file: {action}"
        );
        let ops = edit["documentChanges"].as_array().expect("documentChanges");
        assert_eq!(ops[0]["kind"], json!("create"), "{action}");
        assert_eq!(
            ops[0]["uri"],
            json!(root_config),
            "created at the anchor: {action}"
        );
        assert_eq!(
            ops[1]["textDocument"]["uri"],
            json!(root_config),
            "{action}"
        );
        assert!(
            !action.to_string().contains(&inert),
            "the inert config is never a target: {action}"
        );
    }
}

/// With a LIVE config above the inert one, the actions edit the live one.
#[tokio::test]
async fn the_pin_action_edits_the_live_config_above_an_inert_one() {
    let base = temp_dir("pin-live-disk");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    workspace_fixture(
        &ws,
        &[
            "demo.package.nml",
            "core.model.nml",
            "tenants/cu/plain.flow.nml",
            "tenants/cu/nml-project.nml",
        ],
    );
    fs::write(
        ws.join("nml-project.nml"),
        "project P:\n    keywords = [\"k\"]\n",
    )
    .expect("write live config");
    let plain = ws.join("tenants/cu/plain.flow.nml");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness
        .open(&plain, &fs::read_to_string(&plain).expect("read plain"))
        .await;
    let action = pin_action(&code_actions_at_top(&mut harness, &plain).await);
    let live = file_uri(&ws.join("nml-project.nml"));
    let edits = action["edit"]["changes"][&live]
        .as_array()
        .unwrap_or_else(|| panic!("an edit on the live config: {action}"));
    // The ONE hunk — the inserted lines at their line start — never a
    // whole-file rewrite (the client's undo and diff see an insertion).
    assert_eq!(
        edits[0],
        json!({
            "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 0 } },
            "newText": "    schemaPackages:\n        - demo\n",
        }),
        "{action}"
    );
    assert!(
        action["edit"]["documentChanges"].is_null(),
        "nothing created: {action}"
    );
}

/// An UNSAVED `nml-project.nml` buffer at the root — no file on disk — is
/// the live config the kernel resolves pins through (the overlay), so the
/// pin action edits the buffer's text rather than creating a file the
/// buffer already replaces.
#[tokio::test]
async fn the_pin_action_edits_an_unsaved_live_config_buffer() {
    let base = temp_dir("pin-buffer");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    workspace_fixture(
        &ws,
        &[
            "demo.package.nml",
            "core.model.nml",
            "tenants/cu/plain.flow.nml",
        ],
    );
    let plain = ws.join("tenants/cu/plain.flow.nml");
    let config = ws.join("nml-project.nml");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness
        .open(&config, "project P:\n    keywords = [\"k\"]\n")
        .await;
    assert!(!config.exists(), "the buffer is unsaved");
    harness
        .open(&plain, &fs::read_to_string(&plain).expect("read plain"))
        .await;
    let action = pin_action(&code_actions_at_top(&mut harness, &plain).await);
    let edits = action["edit"]["changes"][file_uri(&config)]
        .as_array()
        .unwrap_or_else(|| panic!("an edit on the buffer: {action}"));
    assert_eq!(
        edits[0],
        json!({
            "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 0 } },
            "newText": "    schemaPackages:\n        - demo\n",
        }),
        "{action}"
    );
    assert!(
        action["edit"]["documentChanges"].is_null(),
        "nothing created: {action}"
    );
}

/// The pin nests by the config's OWN indentation, through the overlay: a
/// two-space config nests the item by two; a two-space `project` block
/// beside a four-space one still nests by its own two (the block's step,
/// never another block's); a bare header in a two-space file nests by
/// the document's step; a bare header alone nests by the canonical unit.
#[tokio::test]
async fn the_pin_nests_by_the_configs_own_step_then_the_documents_then_the_canonical() {
    let base = temp_dir("pin-unit");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    workspace_fixture(
        &ws,
        &[
            "demo.package.nml",
            "core.model.nml",
            "tenants/cu/plain.flow.nml",
        ],
    );
    let plain = ws.join("tenants/cu/plain.flow.nml");
    let config = ws.join("nml-project.nml");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness
        .open(&config, "project P:\n  keywords = [\"k\"]\n")
        .await;
    harness
        .open(&plain, &fs::read_to_string(&plain).expect("read plain"))
        .await;
    let cases: [(&str, u32, &str); 4] = [
        // (the config buffer's text, the hunk's line, the hunk)
        (
            "project P:\n  keywords = [\"k\"]\n  meta:\n      owner = \"x\"\n",
            1,
            "  schemaPackages:\n    - demo\n",
        ),
        (
            "host H:\n  x = 1\n\nproject P:\n",
            4,
            "  schemaPackages:\n    - demo\n",
        ),
        ("project P:\n", 1, "    schemaPackages:\n        - demo\n"),
        (
            "project P:\r\n  keywords = [\"k\"]\r\n",
            1,
            "  schemaPackages:\r\n    - demo\r\n",
        ),
    ];
    let first = pin_action(&code_actions_at_top(&mut harness, &plain).await);
    assert_eq!(
        first["edit"]["changes"][file_uri(&config)][0],
        json!({
            "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 0 } },
            "newText": "  schemaPackages:\n    - demo\n",
        }),
        "a two-space config nests by two: {first}"
    );
    for (version, (text, line, hunk)) in cases.iter().enumerate() {
        harness
            .notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": file_uri(&config), "version": version + 2 },
                    "contentChanges": [{ "text": text }],
                }),
            )
            .await;
        let action = pin_action(&code_actions_at_top(&mut harness, &plain).await);
        assert_eq!(
            action["edit"]["changes"][file_uri(&config)][0],
            json!({
                "range": { "start": { "line": line, "character": 0 }, "end": { "line": line, "character": 0 } },
                "newText": hunk,
            }),
            "config {text:?}: {action}"
        );
    }
}

/// A config the pin or opt-out CREATES is what `nml fmt` writes: the
/// skeleton with the same insertion an existing config receives, nested
/// by the canonical unit — the formatter's fixed point, byte for byte.
#[tokio::test]
async fn a_created_config_is_the_formatters_fixed_point() {
    let base = temp_dir("pin-create-canonical");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    workspace_fixture(
        &ws,
        &[
            "demo.package.nml",
            "core.model.nml",
            "tenants/cu/plain.flow.nml",
        ],
    );
    let plain = ws.join("tenants/cu/plain.flow.nml");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_as_vscode(&ws).await;
    harness
        .open(&plain, &fs::read_to_string(&plain).expect("read plain"))
        .await;
    let actions = code_actions_at_top(&mut harness, &plain).await;
    let expected = [
        (
            "Pin schema package 'demo'",
            "project Project:\n    schemaPackages:\n        - demo\n",
        ),
        (
            "Not a demo project? Disable schema auto-association for this root",
            "project Project:\n    autoAssociate = false\n",
        ),
    ];
    for (title, content) in expected {
        let action = actions
            .iter()
            .find(|a| a["title"] == json!(title))
            .unwrap_or_else(|| panic!("{title}: {actions:?}"));
        let ops = action["edit"]["documentChanges"]
            .as_array()
            .expect("documentChanges");
        assert_eq!(ops[0]["kind"], json!("create"), "{action}");
        let created = ops[1]["edits"][0]["newText"]
            .as_str()
            .expect("the created text");
        assert_eq!(created, content, "{action}");
        assert_eq!(
            nml_fmt::formatter::format_source(created).expect("a created config parses"),
            created,
            "the created config is the formatter's fixed point"
        );
    }
}

/// `textDocument/formatting` on a document that DOES parse: the edit the
/// server returns, applied, is byte for byte what `nml fmt` writes — the
/// editor door and the CLI door are one formatter. The edit's RANGE is the
/// part only an application can check: it replaces the whole document, so
/// an end position one character short silently truncates the buffer, and
/// a CRLF document's last line is where that would happen first. The
/// client's `tabSize`/`insertSpaces` are deliberately wrong here: the
/// canonical style has no options (`spec/style.md` §5), and a client that
/// asks for two-space tabs gets four spaces.
#[tokio::test]
async fn formatting_returns_the_edit_that_makes_the_buffer_what_nml_fmt_writes() {
    let base = temp_dir("formatting-edit");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let file = ws.join("t.nml");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;

    /// Apply one whole-document LSP edit to `text`. Positions are
    /// (line, UTF-16 character) — the fixtures are ASCII, so the unit is
    /// the byte, and the point of the helper is the RANGE arithmetic.
    fn apply(text: &str, edit: &Value) -> String {
        let at = |line: u64, ch: u64| -> usize {
            let mut off = 0usize;
            for (i, l) in text.split_inclusive('\n').enumerate() {
                if i as u64 == line {
                    return off + ch as usize;
                }
                off += l.len();
            }
            off + ch as usize
        };
        let r = &edit["range"];
        let start = at(
            r["start"]["line"].as_u64().expect("start line"),
            r["start"]["character"].as_u64().expect("start char"),
        );
        let end = at(
            r["end"]["line"].as_u64().expect("end line"),
            r["end"]["character"].as_u64().expect("end char"),
        )
        .min(text.len());
        let mut out = String::new();
        out.push_str(&text[..start]);
        out.push_str(edit["newText"].as_str().expect("newText"));
        out.push_str(&text[end..]);
        out
    }

    for (name, source) in [
        (
            "a two-space document",
            "service A:\n  x   =  1\n  b:\n    y = 2\n",
        ),
        ("a CRLF document", "service A:\r\n  x = 1\r\n"),
        ("no terminator at the end", "service A:\n  x = 1"),
        (
            "a blank line inside a block body",
            "service A:\n    p = \"\"\"\n        one\n\n        two\n        \"\"\"\n",
        ),
        ("a document that is only comments", "// a\n//  b\n"),
    ] {
        harness
            .notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": file_uri(&file),
                        "languageId": "nml",
                        "version": 1,
                        "text": source,
                    }
                }),
            )
            .await;
        let edits = harness
            .request(
                "textDocument/formatting",
                json!({
                    "textDocument": { "uri": file_uri(&file) },
                    "options": { "tabSize": 2, "insertSpaces": false },
                }),
            )
            .await;
        let want = nml_fmt::formatter::format_source(source).expect("the fixture formats");
        if want == source {
            assert!(
                edits.is_null(),
                "{name}: a canonical document needs no edit: {edits}"
            );
            continue;
        }
        let list = edits
            .as_array()
            .unwrap_or_else(|| panic!("{name}: {edits}"));
        assert_eq!(list.len(), 1, "{name}: one whole-document edit: {edits}");
        assert_eq!(apply(source, &list[0]), want, "{name}: the applied edit");
    }
}

/// A new line after a block header is indented by the unit the
/// structural insertions use — the document's own, through the overlay —
/// so the cursor lands where a quick fix would put a nested line: in a
/// two-space document two deeper (the client's `tabSize` is its guess at
/// the document; the tree knows).
#[tokio::test]
async fn a_new_line_after_a_header_nests_by_the_documents_unit() {
    let base = temp_dir("on-type-unit");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let file = ws.join("t.nml");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    /// Enter at the start of `line` — the client's `tabSize` deliberately
    /// disagrees with the document.
    async fn on_type(harness: &mut Harness, file: &Path, line: u32) -> Value {
        harness
            .request(
                "textDocument/onTypeFormatting",
                json!({
                    "textDocument": { "uri": file_uri(file) },
                    "position": { "line": line, "character": 0 },
                    "ch": "\n",
                    "options": { "tabSize": 8, "insertSpaces": true },
                }),
            )
            .await
    }
    // Enter after `  slot:` in a two-space document: four spaces (2 + 2).
    harness
        .open(&file, "project P:\n  keywords = [\"k\"]\n  slot:\n\n")
        .await;
    let edits = on_type(&mut harness, &file, 3).await;
    assert_eq!(edits[0]["newText"], json!("    "), "{edits}");
    // Enter after a top-level header in a two-space document: two.
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&file), "version": 2 },
                "contentChanges": [{ "text": "host H:\n  x = 1\n\nproject P:\n\n" }],
            }),
        )
        .await;
    let edits = on_type(&mut harness, &file, 4).await;
    assert_eq!(edits[0]["newText"], json!("  "), "{edits}");
    // A document with nothing nested: the canonical four, whatever the
    // client's `tabSize` says.
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&file), "version": 3 },
                "contentChanges": [{ "text": "project P:\n\n" }],
            }),
        )
        .await;
    let edits = on_type(&mut harness, &file, 1).await;
    assert_eq!(edits[0]["newText"], json!("    "), "{edits}");
}

/// A document more than 64 directories below its root cannot be keyed:
/// the editor refuses it with the kernel's sentence as its one ERROR row
/// — no parse finding, `nml/schemaInfo` unbound with the note — as
/// `nml check` fails that target (`<path>: more than 64 path components —
/// nothing this deep is keyable; …`, exit 1) and judges nothing.
#[tokio::test]
async fn a_document_past_the_component_bound_is_refused_in_the_editor() {
    let base = temp_dir("deep-refused");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    workspace_fixture(&ws, &["demo.package.nml", "core.model.nml"]);
    let deep = (0..nml_validate::workspace::MAX_COMPONENTS)
        .fold(ws.clone(), |p, i| p.join(format!("d{i}")));
    fs::create_dir_all(&deep).expect("deep dirs");
    let file = deep.join("x.flow.nml");
    let broken = "thing a\n    v = = 2\n";
    fs::write(&file, broken).expect("write deep");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&file, broken).await;
    let rows = report["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(rows.len(), 1, "one row, no parse finding: {report}");
    assert_eq!(rows[0]["severity"], json!(1), "an error: {report}");
    assert!(rows[0]["code"].is_null(), "uncoded, as the CLI's: {report}");
    assert_eq!(rows[0]["message"], json!(DEEP_REFUSAL));
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&file) }))
        .await;
    assert_eq!(info["bound"], json!(false), "{info}");
    assert_eq!(info["notes"][0]["severity"], json!("error"), "{info}");
    assert_eq!(info["notes"][0]["message"], json!(DEEP_REFUSAL));
}

/// The kernel's one sentence for a typed target past the component
/// bound — the remedy inside it, on the CLI and in the editor alike.
const DEEP_REFUSAL: &str = "more than 64 path components — nothing this deep is keyable; flatten \
                            the tree, or move the file where the walk lists it";

/// A workspace whose manifest binds `tenants/**` with `layers` as its
/// grant block (empty: none — the NML2064 no-grant denial a quick fix
/// repairs) and a tenant file composing under it. Returns the workspace,
/// the manifest's path and text, the tenant file's path and text.
fn grant_workspace(base: &Path, layers: &str) -> (PathBuf, PathBuf, String, PathBuf, String) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace");
    let manifest_text = format!(
        "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    - core:\n        \
         file = \"core.model.nml\"\n\n[]validator validators:\n    - tenantFlows:\n        files:\n            - \
         \"tenants/**/*.flow.nml\"\n        schemas:\n            - core\n        strict = true\n{layers}"
    );
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu")).expect("create tenant");
    let manifest = ws.join("demo.package.nml");
    fs::write(&manifest, &manifest_text).expect("manifest");
    fs::copy(fixture.join("core.model.nml"), ws.join("core.model.nml")).expect("core");
    let tenant = ws.join("tenants/cu/member-lookup.flow.nml");
    let text = "thing base:\n    v = \"b\"\n\nthing t uses base:\n    v = \"t\"\n".to_string();
    fs::write(&tenant, &text).expect("write tenant file");
    (ws, manifest, manifest_text, tenant, text)
}

/// `textDocument/codeAction` over `range` of `path` with `diagnostics` in
/// context — the actions offered there.
async fn code_actions_in(
    harness: &mut Harness,
    path: &Path,
    range: Value,
    diagnostics: Vec<Value>,
) -> Vec<Value> {
    harness
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": file_uri(path) },
                "range": range,
                "context": { "diagnostics": diagnostics },
            }),
        )
        .await
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The grant quick fix among `actions` (an array or `null`), by its title.
fn grant_fix(actions: &Value) -> Option<Value> {
    actions
        .as_array()?
        .iter()
        .find(|a| {
            a["title"]
                .as_str()
                .is_some_and(|t| t.starts_with("Add `layers` under"))
        })
        .cloned()
}

/// The `(0,0)` hover of `path` — the binding summary.
async fn hover_top(harness: &mut Harness, path: &Path) -> Value {
    harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(path) },
                "position": { "line": 0, "character": 0 },
            }),
        )
        .await
}

fn range(line: u64, from: u64, to: u64) -> Value {
    json!({ "start": { "line": line, "character": from }, "end": { "line": line, "character": to } })
}

/// The block the denial names, inserted under the binding — the quick
/// fix (RFC 0026 B-1, the editor's half of the structured suggestion): the
/// NML2064 row on the CONTENT file carries the insertion (`kind: insert`,
/// the manifest as its `source`, the zero-indented block), the action is
/// a `quickfix` whose edit is a VERSIONED `documentChanges` entry on the
/// MANIFEST (not open: `version: null`, the disk is the master), the one
/// hunk after the binding's last line nested by the manifest's own unit.
/// Applied (the manifest re-opened with the block), the denial is gone and
/// `nml/schemaInfo.layers` — and the `(0,0)` hover — read the grant the
/// fix produced, in the `--json` `binding` row's one spelling.
#[tokio::test]
async fn the_grant_quick_fix_lands_the_block_under_the_binding() {
    let base = temp_dir("grant-fix");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, manifest, manifest_text, tenant, text) = grant_workspace(&base, "");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_as_vscode(&ws).await;
    let report = harness.open(&tenant, &text).await;
    assert_eq!(codes_of(&report), ["NML2064"], "{report}");
    let diag = report["diagnostics"][0].clone();
    // The located remedy (LSP 3.17 §DiagnosticRelatedInformation): the
    // row's related location is the binding in the manifest — the place
    // the quick fix edits — and nothing else rides the row for it: no
    // `help` key on the wire, the block is the suggestion.
    let related = &diag["relatedInformation"][0];
    assert!(
        related["location"]["uri"]
            .as_str()
            .is_some_and(|u| u.ends_with("/demo.package.nml")),
        "{diag}"
    );
    assert_eq!(
        related["location"]["range"],
        json!({ "start": { "line": 9, "character": 6 }, "end": { "line": 9, "character": 17 } }),
        "{diag}"
    );
    assert!(
        diag.get("help").is_none() && diag["data"].get("help").is_none(),
        "nothing is rendered for a help: {diag}"
    );
    let suggestion = &diag["data"]["suggestions"][0];
    assert_eq!(suggestion["kind"], json!("insert"), "{diag}");
    assert_eq!(suggestion["source"], json!("demo.package.nml"), "{diag}");
    assert_eq!(
        suggestion["replacement"],
        json!("layers:\n    allowRefs:\n        - \"tenants/cu/member-lookup.flow.nml\""),
        "{diag}"
    );
    // Before the fix: the introspection says the binding grants nothing.
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&tenant) }))
        .await;
    assert_eq!(info["layers"], json!({ "granted": false }), "{info}");
    let hover = hover_top(&mut harness, &tenant).await;
    assert!(
        hover["contents"]["value"]
            .as_str()
            .is_some_and(|v| v.contains("layers: none — composition denied (NML2064)")),
        "{hover}"
    );
    // The action, on the denial.
    let actions = one_code_action(&mut harness, &tenant, diag.clone()).await;
    let fix = grant_fix(&actions).unwrap_or_else(|| panic!("the grant fix: {actions}"));
    assert_eq!(fix["kind"], json!("quickfix"), "{fix}");
    assert_eq!(
        fix["title"],
        json!("Add `layers` under 'tenantFlows' in demo.package.nml"),
        "{fix}"
    );
    assert!(
        fix["isPreferred"].is_null(),
        "a structural edit is never auto-applied: {fix}"
    );
    assert!(
        fix["edit"]["changes"].is_null(),
        "a versioned edit for a client that declared documentChanges: {fix}"
    );
    let change = &fix["edit"]["documentChanges"][0];
    assert_eq!(
        change["textDocument"]["uri"],
        json!(file_uri(&manifest)),
        "{fix}"
    );
    assert_eq!(
        change["textDocument"]["version"],
        json!(null),
        "a manifest the client did not open: the disk is the master: {fix}"
    );
    let edit = &change["edits"][0];
    assert_eq!(
        edit["range"],
        range(15, 0, 0),
        "after the binding's last line: {fix}"
    );
    assert_eq!(
        edit["newText"],
        json!(
            "        layers:\n            allowRefs:\n                - \"tenants/cu/member-lookup.flow.nml\"\n"
        ),
        "{fix}"
    );
    assert_eq!(
        change["edits"].as_array().map(Vec::len),
        Some(1),
        "one hunk: {fix}"
    );
    // Applied — the manifest holds the block (an unsaved buffer, the
    // overlay): the denial is gone on the next pull; the introspection
    // and the hover read the grant the fix produced.
    let fixed = format!(
        "{manifest_text}{}",
        edit["newText"].as_str().expect("newText")
    );
    harness.open(&manifest, &fixed).await;
    let report = harness.diagnostics(&file_uri(&tenant)).await;
    assert!(codes_of(&report).is_empty(), "composes clean: {report}");
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&tenant) }))
        .await;
    assert_eq!(
        info["layers"],
        json!({
            "granted": true,
            "allowRefs": ["tenants/cu/member-lookup.flow.nml"],
            "denyRefs": [],
            "maxStackDepth": null,
        }),
        "{info}"
    );
    let hover = hover_top(&mut harness, &tenant).await;
    assert!(
        hover["contents"]["value"].as_str().is_some_and(|v| {
            v.contains("layers: granted — allowRefs[0] = \"tenants/cu/member-lookup.flow.nml\"")
        }),
        "{hover}"
    );
    // Nothing left to fix: no grant action on the tenant, nor on the manifest.
    let none = code_actions_at_top(&mut harness, &tenant).await;
    assert!(grant_fix(&json!(none)).is_none(), "{none:?}");
    let none = code_actions_in(&mut harness, &manifest, range(9, 6, 17), vec![]).await;
    assert!(grant_fix(&json!(none)).is_none(), "{none:?}");
}

/// LSP 3.17 §DocumentSymbol: `selectionRange` "must be contained by the
/// `range`", and a child's range by its parent's. A client enforces this —
/// VS Code logs "Invalid outline" and DISCARDS the offending symbol — so a
/// violation is silently missing outline, breadcrumbs and sticky scroll,
/// never an error the server sees. Checked over one document carrying every
/// shape the builder can emit (block, nested block, property, list item,
/// array, arm, const, template, oneof + its arms), because the rule is
/// per-shape and only the shape that is wrong goes missing.
///
/// The `oneof` arm was wrong: its range was the arm's MODEL alone, which
/// does not merely fail to contain the value literal — it starts after it.
#[tokio::test]
async fn every_document_symbol_contains_its_own_selection_range() {
    fn before(a: &Value, b: &Value) -> bool {
        let (al, ac) = (a["line"].as_u64(), a["character"].as_u64());
        let (bl, bc) = (b["line"].as_u64(), b["character"].as_u64());
        al < bl || (al == bl && ac <= bc)
    }
    fn contains(outer: &Value, inner: &Value) -> bool {
        before(&outer["start"], &inner["start"]) && before(&inner["end"], &outer["end"])
    }
    fn check(symbols: &[Value], parent: Option<&Value>, seen: &mut usize) {
        for symbol in symbols {
            *seen += 1;
            assert!(
                contains(&symbol["range"], &symbol["selectionRange"]),
                "`{}`: selectionRange {} is not inside range {}",
                symbol["name"],
                symbol["selectionRange"],
                symbol["range"]
            );
            if let Some(parent) = parent {
                assert!(
                    contains(&parent["range"], &symbol["range"]),
                    "`{}`: range {} is not inside its parent's {}",
                    symbol["name"],
                    symbol["range"],
                    parent["range"]
                );
            }
            if let Some(children) = symbol["children"].as_array() {
                check(children, Some(symbol), seen);
            }
        }
    }

    let base = temp_dir("symbol-ranges");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("ws");
    let doc = ws.join("outline.nml");
    let text = "const limit = 3\n\
                \n\
                template greeting = \"hello\"\n\
                \n\
                model core:\n\
                \x20   kind string\n\
                \x20   msg string?\n\
                \n\
                model quiet:\n\
                \x20   kind string\n\
                \n\
                oneof record by kind:\n\
                \x20   \"log\" -> core\n\
                \x20   \"mute\" -> quiet\n\
                \n\
                service app:\n\
                \x20   name = \"app\"\n\
                \x20   limits:\n\
                \x20       retries = 2\n\
                \x20   denial:\n\
                \x20       @role/admin -> core\n\
                \x20       else -> quiet\n\
                \n\
                []service fleet:\n\
                \x20   - west:\n\
                \x20       name = \"w\"\n";
    fs::write(&doc, text).expect("write doc");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    harness.open(&doc, text).await;
    let symbols = harness
        .request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": file_uri(&doc) } }),
        )
        .await;
    let top = symbols.as_array().cloned().unwrap_or_default();
    let mut seen = 0;
    check(&top, None, &mut seen);
    assert!(
        seen >= 15,
        "the fixture must actually reach every builder arm: {seen} symbols in {symbols}"
    );
}

/// `textDocument/rename` — the one surface that edits several files at
/// once — hands out the SAME shape every other action does: `documentChanges`
/// naming each buffer's client version for a client that declared
/// `workspace.workspaceEdit.documentChanges`, so a rename computed against a
/// buffer that has since moved on is refused WHOLESALE rather than half
/// applied; plain `changes` for a client that declared nothing. It used to
/// hand out plain `changes` in both cases, and had no test at all.
///
/// `textDocument/prepareRename` answers the identifier's own range, so the
/// editor's rename box opens pre-filled on the word and not on the line.
#[tokio::test]
async fn a_rename_across_files_is_one_versioned_edit() {
    let base = temp_dir("rename-shape");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("ws");
    fs::write(
        ws.join("core.model.nml"),
        "model core:\n    name string\n    peer string?\n",
    )
    .expect("model");
    let one = ws.join("one.nml");
    let two = ws.join("two.nml");
    let one_text = "core alpha:\n    name = \"a\"\n";
    // A BARE identifier is a reference to the block; a quoted string is
    // not, so this is the cross-file occurrence a rename must carry.
    let two_text = "const link = alpha\n";
    fs::write(&one, one_text).expect("one");
    fs::write(&two, two_text).expect("two");
    // Two more referrers: the document store is a hash map, so with two
    // files an UNSORTED `documentChanges` came out sorted half the time;
    // with four, one run in twenty-four.
    let three = ws.join("three.nml");
    let four = ws.join("four.nml");
    fs::write(&three, "const again = alpha\n").expect("three");
    fs::write(&four, "const more = alpha\n").expect("four");

    // A client that declared `documentChanges`, with `one.nml` OPEN at
    // version 1 and `two.nml` only indexed (the disk is its master).
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_as_vscode(&ws).await;
    harness.open(&one, one_text).await;
    let prepared = harness
        .request(
            "textDocument/prepareRename",
            json!({
                "textDocument": { "uri": file_uri(&one) },
                "position": { "line": 0, "character": 7 },
            }),
        )
        .await;
    assert_eq!(
        prepared,
        json!({"start": {"line": 0, "character": 5}, "end": {"line": 0, "character": 10}}),
        "the identifier's own range: {prepared}"
    );
    let edit = harness
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": file_uri(&one) },
                "position": { "line": 0, "character": 7 },
                "newName": "gamma",
            }),
        )
        .await;
    assert!(
        edit.get("changes").is_none(),
        "a declaring client gets documentChanges only: {edit}"
    );
    let changes = edit["documentChanges"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let named: Vec<(String, Value)> = changes
        .iter()
        .map(|c| {
            (
                c["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                c["textDocument"]["version"].clone(),
            )
        })
        .collect();
    // Sorted by URI, so the same rename is handed out the same way twice.
    let mut sorted = named.clone();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(named, sorted, "documentChanges is an ordered array: {edit}");
    assert_eq!(
        named,
        vec![
            (file_uri(&four), Value::Null),
            (file_uri(&one), json!(1)),
            (file_uri(&three), Value::Null),
            (file_uri(&two), Value::Null),
        ],
        "the open buffer's version, and null for the files the client did not open: {edit}"
    );
    assert!(
        changes
            .iter()
            .all(|c| c["edits"].as_array().is_some_and(|e| !e.is_empty())),
        "both files carry edits: {edit}"
    );

    // The same rename to a client that declared nothing: plain `changes`,
    // the only shape it can apply.
    let mut plain = Harness::new(Store::at(&store_base));
    plain.initialize(&ws).await;
    plain.open(&one, one_text).await;
    let edit = plain
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": file_uri(&one) },
                "position": { "line": 0, "character": 7 },
                "newName": "gamma",
            }),
        )
        .await;
    assert!(
        edit.get("documentChanges").is_none(),
        "a plain client gets `changes` only: {edit}"
    );
    let files: Vec<&String> = edit["changes"]
        .as_object()
        .map(|m| m.keys().collect())
        .unwrap_or_default();
    assert_eq!(files.len(), 4, "every referrer: {edit}");
}

/// A pull that rediscovers the universe asks a client that declared
/// `workspace.diagnostics.refreshSupport` (as LSP 3.17 and VS Code spell
/// it) to re-pull every open document —
/// `workspace/diagnostic/refresh` (LSP 3.17), once per change: a manifest
/// buffer opened beside a denied tenant makes the tenant's cached report
/// stale (and with it the grant action the manifest offers from it); the
/// refresh brings both back without a refocus. A pull that hits the cache
/// asks nothing; a client that declared no support is never asked.
#[tokio::test]
async fn a_universe_change_asks_a_declaring_client_to_refresh_pulled_diagnostics() {
    let base = temp_dir("diag-refresh");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, manifest, manifest_text, tenant, text) = grant_workspace(&base, "");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_as_vscode_with_refresh(&ws).await;
    let report = harness.open(&tenant, &text).await;
    assert_eq!(codes_of(&report), ["NML2064"], "{report}");
    // The tenant's own first pull discovered the universe; no other
    // document holds a report, so nothing is asked.
    assert_eq!(harness.refreshes(), 0, "{:?}", harness.requests);
    // The manifest buffer is a universe input: its pull rediscovers, the
    // tenant's report is stale — one refresh.
    harness.open(&manifest, &manifest_text).await;
    assert_eq!(harness.refreshes(), 1, "{:?}", harness.requests);
    // The re-pull the client performs hits the fresh cache: nothing more.
    harness.diagnostics(&file_uri(&tenant)).await;
    harness.diagnostics(&file_uri(&manifest)).await;
    assert_eq!(harness.refreshes(), 1, "{:?}", harness.requests);
    // …and the manifest offers the grant from the tenant's fresh report.
    let at_binding = code_actions_in(&mut harness, &manifest, range(9, 6, 17), vec![]).await;
    assert!(grant_fix(&json!(at_binding)).is_some(), "{at_binding:?}");
    // An edit to the manifest (version 2) rediscovers again: one more.
    let v2 = format!("{manifest_text}// end of the manifest\n");
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&manifest), "version": 2 },
                "contentChanges": [{ "text": v2 }],
            }),
        )
        .await;
    harness.diagnostics(&file_uri(&manifest)).await;
    assert_eq!(harness.refreshes(), 2, "{:?}", harness.requests);
    // A client that declared no refresh support is never asked.
    let mut plain = Harness::new(Store::at(&store_base));
    plain.initialize_as_vscode(&ws).await;
    plain.open(&tenant, &text).await;
    plain.open(&manifest, &manifest_text).await;
    assert_eq!(plain.refreshes(), 0, "{:?}", plain.requests);
}

/// The OTHER refused `initialize`: one tower-lsp refuses BEFORE the
/// handshake, for params the typed handler cannot deserialize
/// (`invalid_params`; the server stays uninitialized and the next
/// `initialize` is the real one). Its raw look still ran and parked what
/// the frame declared — so the real handshake that follows, declaring
/// nothing, must not take a declaration it never made. The parked slot is
/// therefore written on EVERY `initialize` frame, the empty answer
/// included; writing it only when a frame declares (the natural
/// short-cut, and a mutant that passed every pin) hands this client a
/// refresh it has no handler for.
#[tokio::test]
async fn a_refused_first_initialize_leaves_no_declaration_parked_for_the_real_one() {
    let base = temp_dir("refused-first-initialize");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, manifest, manifest_text, tenant, text) = grant_workspace(&base, "");
    let mut harness = Harness::new(Store::at(&store_base));
    // Declares refresh support, but `processId` is no number: refused
    // before the handler, the declaration parked.
    harness.next_id += 1;
    let malformed = Request::build("initialize")
        .params(json!({
            "processId": "not-a-number",
            "capabilities": { "workspace": { "diagnostics": { "refreshSupport": true } } },
            "rootUri": file_uri(&ws),
        }))
        .id(harness.next_id)
        .finish();
    let (_, result) = harness
        .call_raw(malformed)
        .await
        .expect("a request always yields a response")
        .into_parts();
    assert!(
        result.is_err(),
        "a malformed `initialize` must be refused: {result:?}"
    );
    // The real handshake: no refresh support. The same two pulls that DO
    // ask a declaring client must ask this one nothing.
    harness.initialize_as_vscode(&ws).await;
    harness.open(&tenant, &text).await;
    harness.open(&manifest, &manifest_text).await;
    assert_eq!(
        harness.refreshes(),
        0,
        "a refused frame's declaration reached the next handshake: {:?}",
        harness.requests
    );
}

/// A second `initialize` declares NOTHING. tower-lsp refuses a duplicate
/// with `invalid_request` and never runs the typed handler — but the raw
/// look at `initialize` params (the one that reads LSP 3.17's
/// `workspace.diagnostics` spelling, which lsp-types drops) sits upstream
/// of that lifecycle and fires on every frame. Applied there, a duplicate
/// turned `refreshSupport` on for a client that never declared it, and the
/// next universe-changing pull then AWAITS an answer such a client has no
/// handler for: the pull never returns, and with the transport's in-flight
/// budget spent the server stops reading altogether. The declaration is
/// parked for the accepted handshake's handler to take instead.
#[tokio::test]
async fn a_refused_duplicate_initialize_declares_no_capability() {
    let base = temp_dir("duplicate-initialize");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, manifest, manifest_text, tenant, text) = grant_workspace(&base, "");
    let mut harness = Harness::new(Store::at(&store_base));
    // The real handshake: no refresh support.
    harness.initialize_as_vscode(&ws).await;
    // A buggy (or hostile) client's second `initialize`, declaring it.
    harness.next_id += 1;
    let duplicate = Request::build("initialize")
        .params(json!({
            "capabilities": { "workspace": { "diagnostics": { "refreshSupport": true } } },
            "rootUri": file_uri(&ws),
        }))
        .id(harness.next_id)
        .finish();
    let (_, result) = harness
        .call_raw(duplicate)
        .await
        .expect("a request always yields a response")
        .into_parts();
    assert!(
        result.is_err(),
        "a duplicate `initialize` must be refused: {result:?}"
    );
    // The same two pulls that DO ask a declaring client (see the test
    // above) must ask this one nothing.
    harness.open(&tenant, &text).await;
    harness.open(&manifest, &manifest_text).await;
    assert_eq!(
        harness.refreshes(),
        0,
        "a refused `initialize` declared a capability: {:?}",
        harness.requests
    );
}

/// The capability under lsp-types 0.94.1's own key (`workspace.diagnostic`,
/// the spelling that crate deserializes) counts too: a client generated from
/// it is refreshed like a conforming one.
#[tokio::test]
async fn a_client_spelling_the_capability_as_lsp_types_does_is_refreshed_too() {
    let base = temp_dir("diag-refresh-lsp-types");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, manifest, manifest_text, tenant, text) = grant_workspace(&base, "");
    let mut harness = Harness::new(Store::at(&store_base));
    harness
        .initialize_as_lsp_types_client_with_refresh(&ws)
        .await;
    harness.open(&tenant, &text).await;
    assert_eq!(harness.refreshes(), 0, "{:?}", harness.requests);
    harness.open(&manifest, &manifest_text).await;
    assert_eq!(harness.refreshes(), 1, "{:?}", harness.requests);
}

/// The edit is VERSIONED against an OPEN manifest (the client's version,
/// as `didChange` last said), offered ON THE MANIFEST at the binding the
/// denial points at (and nowhere else on it), and REFUSED once the
/// manifest moved on: the old diagnostic's data no longer belongs to the
/// current report (membership) and the old anchor names no binding — the
/// next pull's diagnostic and the binding's new place offer it again.
#[tokio::test]
async fn the_grant_quick_fix_is_versioned_offered_on_the_manifest_and_refused_when_stale() {
    let base = temp_dir("grant-fix-versioned");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, manifest, manifest_text, tenant, text) = grant_workspace(&base, "");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_as_vscode(&ws).await;
    harness.open(&tenant, &text).await;
    // The manifest opened (version 1) and edited (version 2 — a trailing
    // comment; the binding stays where it was).
    harness.open(&manifest, &manifest_text).await;
    let v2 = format!("{manifest_text}// end of the manifest\n");
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&manifest), "version": 2 },
                "contentChanges": [{ "text": v2 }],
            }),
        )
        .await;
    let report = harness.diagnostics(&file_uri(&tenant)).await;
    assert_eq!(codes_of(&report), ["NML2064"], "{report}");
    let diag = report["diagnostics"][0].clone();
    let fix = grant_fix(&one_code_action(&mut harness, &tenant, diag.clone()).await)
        .unwrap_or_else(|| panic!("the grant fix: {report}"));
    let change = &fix["edit"]["documentChanges"][0];
    assert_eq!(
        change["textDocument"]["version"],
        json!(2),
        "the buffer's version: {fix}"
    );
    assert_eq!(
        change["edits"][0]["range"],
        range(15, 0, 0),
        "after the binding's last line, above the trailing comment: {fix}"
    );
    // On the manifest, at the binding's name: the same action. Elsewhere
    // on it: nothing.
    let at_binding = code_actions_in(&mut harness, &manifest, range(9, 6, 17), vec![]).await;
    assert_eq!(
        grant_fix(&json!(at_binding)),
        Some(fix.clone()),
        "{at_binding:?}"
    );
    let elsewhere = code_actions_in(&mut harness, &manifest, range(0, 0, 0), vec![]).await;
    assert!(grant_fix(&json!(elsewhere)).is_none(), "{elsewhere:?}");
    // An unrelated open document holding the SAME text under another name
    // (so the anchor's range exists in it) is not the file the insertion
    // edits: nothing is offered there.
    let twin = ws.join("notes.nml");
    harness.open(&twin, &manifest_text).await;
    // A new buffer is a new input to the walk: the tenant's report is
    // re-pulled so its cached denial is fresh when the twin asks.
    harness.diagnostics(&file_uri(&tenant)).await;
    let unrelated = code_actions_in(&mut harness, &twin, range(9, 6, 17), vec![]).await;
    assert!(grant_fix(&json!(unrelated)).is_none(), "{unrelated:?}");
    // The binding moves (version 3): stale data offers nothing, the old
    // place offers nothing; the fresh diagnostic and the new place do.
    let v3 = format!(
        "{}// end of the manifest\n",
        manifest_text.replace(
            "    - tenantFlows:",
            "    // the tenant binding\n    - tenantFlows:"
        )
    );
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&manifest), "version": 3 },
                "contentChanges": [{ "text": v3 }],
            }),
        )
        .await;
    let stale = one_code_action(&mut harness, &tenant, diag).await;
    assert!(
        grant_fix(&stale).is_none(),
        "a stale denial offers no edit: {stale}"
    );
    let at_old = code_actions_in(&mut harness, &manifest, range(9, 6, 17), vec![]).await;
    assert!(grant_fix(&json!(at_old)).is_none(), "{at_old:?}");
    let report = harness.diagnostics(&file_uri(&tenant)).await;
    let fresh = report["diagnostics"][0].clone();
    let fix = grant_fix(&one_code_action(&mut harness, &tenant, fresh).await)
        .unwrap_or_else(|| panic!("the fresh fix: {report}"));
    let change = &fix["edit"]["documentChanges"][0];
    assert_eq!(change["textDocument"]["version"], json!(3), "{fix}");
    assert_eq!(change["edits"][0]["range"], range(16, 0, 0), "{fix}");
    let at_new = code_actions_in(&mut harness, &manifest, range(10, 6, 17), vec![]).await;
    assert_eq!(grant_fix(&json!(at_new)), Some(fix), "{at_new:?}");
    // The denial leaves the CONTENT buffer (its `uses` clause dropped,
    // no pull yet): the manifest offers nothing for a diagnostic the
    // buffer no longer has — the cached report is read only while its
    // text is the buffer's.
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&tenant), "version": 2 },
                "contentChanges": [{ "text": "thing base:\n    v = \"b\"\n" }],
            }),
        )
        .await;
    let gone = code_actions_in(&mut harness, &manifest, range(10, 6, 17), vec![]).await;
    assert!(grant_fix(&json!(gone)).is_none(), "{gone:?}");
}

/// A binding that already carries a grant denies nothing, so nothing is
/// offered (the kernel emits no insertion; the resolver would refuse a
/// duplicate `layers:` regardless). And a client that declared no
/// `documentChanges` gets the fix as plain `changes` on the manifest —
/// the only shape it can apply — never a versioned edit.
#[tokio::test]
async fn a_granted_binding_offers_no_fix_and_a_plain_client_gets_plain_changes() {
    let base = temp_dir("grant-fix-shapes");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let granted = "        layers:\n            allowRefs:\n                - \"tenants/**\"\n";
    let (ws, _, _, tenant, text) = grant_workspace(&base, granted);
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_as_vscode(&ws).await;
    let report = harness.open(&tenant, &text).await;
    assert!(codes_of(&report).is_empty(), "{report}");
    let none = code_actions_at_top(&mut harness, &tenant).await;
    assert!(grant_fix(&json!(none)).is_none(), "{none:?}");

    let base = temp_dir("grant-fix-plain");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, manifest, _, tenant, text) = grant_workspace(&base, "");
    let mut plain = Harness::new(Store::at(&store_base));
    plain.initialize(&ws).await;
    let report = plain.open(&tenant, &text).await;
    let diag = report["diagnostics"][0].clone();
    let fix = grant_fix(&one_code_action(&mut plain, &tenant, diag).await)
        .unwrap_or_else(|| panic!("the grant fix: {report}"));
    assert!(fix["edit"]["documentChanges"].is_null(), "{fix}");
    let edits = &fix["edit"]["changes"][file_uri(&manifest)];
    assert_eq!(edits[0]["range"], range(15, 0, 0), "{fix}");
}

/// A `key:` block dedented to a list body's item column is an ERROR in the
/// parse band (NML0002, at the block; the lowering refuses to drop it),
/// the hover explains the code, the formatter returns NO edits and says
/// why as a log line (never a lossy rewrite, never a toast on every
/// save), and the outline and the semantic tokens still answer around it.
/// The parse band carries a fallback chain's missing arm ONCE, at the
/// pipe, naming the line break (RFC 0026 decision 2); the next line is
/// its own entry, so no row lands there.
#[tokio::test]
async fn a_fallback_pipe_ending_a_line_is_nml0002_at_the_pipe_in_the_parse_band() {
    let base = temp_dir("fallback-line");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let file = ws.join("app.nml");
    let text = "service App:\n    host = $ENV.HOST |\n    port = 3000\n";
    fs::write(&file, text).expect("write");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&file, text).await;
    assert_eq!(codes_of(&report), ["NML0002"], "{report}");
    let row = &report["diagnostics"][0];
    assert_eq!(
        row["range"],
        json!({ "start": { "line": 1, "character": 21 }, "end": { "line": 1, "character": 22 } }),
        "{row}"
    );
    assert_eq!(
        row["message"],
        json!("expected a value after `|`, found a line break"),
        "{row}"
    );
}

#[tokio::test]
async fn a_dedented_block_in_a_list_body_is_nml0002_in_the_parse_band() {
    let base = temp_dir("stray-block");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let file = ws.join("stray.nml");
    let text = "[]validator validators:\n    - a:\n        files:\n            - \"x/**\"\n    stray:\n        \
                allowRefs:\n            - \"y\"\n    - b:\n        files:\n            - \"z/**\"\n";
    fs::write(&file, text).expect("write");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&file, text).await;
    assert_eq!(codes_of(&report), ["NML0002"], "{report}");
    let row = &report["diagnostics"][0];
    assert_eq!(
        row["range"]["start"],
        json!({ "line": 4, "character": 4 }),
        "{row}"
    );
    let message = row["message"].as_str().expect("message");
    assert!(
        message.contains("expected a list item") && message.contains("in an array body"),
        "{message}"
    );
    let hover = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(&file) },
                "position": { "line": 4, "character": 5 },
            }),
        )
        .await;
    assert!(
        hover["contents"]["value"]
            .as_str()
            .is_some_and(|v| v.contains("**NML0002**")),
        "the explanation tier: {hover}"
    );
    let edits = harness
        .request(
            "textDocument/formatting",
            json!({
                "textDocument": { "uri": file_uri(&file) },
                "options": { "tabSize": 4, "insertSpaces": true },
            }),
        )
        .await;
    assert!(
        edits.is_null(),
        "no edits for a document that does not parse: {edits}"
    );
    // Other log lines precede it (the server's own); scan for the one
    // the formatter says — the per-wait timeout bounds the scan.
    loop {
        let said = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        let line = said["message"].as_str().expect("message");
        if !line.starts_with("NML: formatting skipped") {
            continue;
        }
        assert_eq!(said["type"], json!(2), "a warning: {said}");
        assert!(
            line.starts_with("NML: formatting skipped for stray.nml: 5:5: [NML0002]")
                && line.ends_with("fix the parse error first"),
            "{line}"
        );
        break;
    }
    let symbols = harness
        .request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": file_uri(&file) } }),
        )
        .await;
    let names: Vec<&str> = symbols[0]["children"]
        .as_array()
        .map(|c| c.iter().filter_map(|s| s["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(names.contains(&"a") && names.contains(&"b"), "{symbols}");
    let tokens = harness
        .request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": file_uri(&file) } }),
        )
        .await;
    assert!(tokens["data"].is_array(), "{tokens}");
}

/// An unloadable manifest's row lands AT the kernel's location on the
/// manifest document — for every shape: a grant rule (NML2081 at the
/// glob), a `maxStackDepth` over the cap (NML2081 at the value), a
/// non-whole one (the meta-schema's `multipleOf` facet: NML2088 at the
/// value), an unknown property and a parse error (NML2088 at the
/// finding); through the overlay too (an unsaved manifest buffer). On a
/// content file the row sits at the top with the manifest's location as
/// related information. On the manifest document an NML2088 row whose
/// finding the document reports itself is folded into that finding's
/// row (RFC 0026 decision 6) — one row at the place, carrying the load
/// note; the grant's NML2081 is the row itself. A manifest that cannot
/// be READ (past the manifest cap) has no location: the row sits at the
/// top, alone.
#[tokio::test]
async fn an_unloadable_manifest_lands_at_its_first_finding() {
    let grant = |rest: &str| {
        format!("        layers:\n            allowRefs:\n                - \"tenants/**\"\n{rest}")
    };
    let shapes: [(&str, String, &str, u64); 5] = [
        (
            "rule",
            "        layers:\n            allowRefs:\n                - \"vendor/**x\"\n"
                .to_string(),
            "NML2081",
            17,
        ),
        (
            "whole",
            grant("            maxStackDepth = 1.5\n"),
            "NML2088",
            18,
        ),
        (
            "cap",
            grant("            maxStackDepth = 17\n"),
            "NML2081",
            18,
        ),
        (
            "unknown",
            "        laters:\n            allowRefs:\n                - \"tenants/**\"\n"
                .to_string(),
            "NML2088",
            15,
        ),
        ("parse", String::new(), "NML2088", 2),
    ];
    for (tag, layers, code, line) in shapes {
        let base = temp_dir(&format!("unloadable-{tag}"));
        let store_base = base.join("store");
        fs::create_dir_all(&store_base).expect("create store dir");
        let (ws, manifest, mut manifest_text, tenant, text) = grant_workspace(&base, &layers);
        if tag == "parse" {
            manifest_text = manifest_text.replace("formatVersion = 1", "formatVersion = = 1");
            fs::write(&manifest, &manifest_text).expect("manifest");
        }
        let mut harness = Harness::new(Store::at(&store_base));
        harness.initialize(&ws).await;
        // The content file: refused, one row at the top, the manifest's
        // location a jump away.
        let report = harness.open(&tenant, &text).await;
        assert_eq!(codes_of(&report), [code], "{tag}: {report}");
        let row = &report["diagnostics"][0];
        assert_eq!(
            row["range"]["start"],
            json!({ "line": 0, "character": 0 }),
            "{tag}: {row}"
        );
        let related = &row["relatedInformation"][0];
        assert_eq!(
            related["location"]["uri"],
            json!(file_uri(&manifest)),
            "{tag}: {row}"
        );
        assert_eq!(
            related["location"]["range"]["start"]["line"],
            json!(line),
            "{tag}: {row}"
        );
        // The manifest document: ONE row at the finding. The grant rule's
        // own row (NML2081) restates nothing; a wrapped finding is the
        // document's own row, the NML2088 twin folded into it.
        let report = harness.open(&manifest, &manifest_text).await;
        let rows = report["diagnostics"].as_array().expect("diagnostics");
        let folded: Vec<&Value> = rows.iter().filter(|d| carries_load_note(d)).collect();
        if code == "NML2088" {
            // The wrapper is gone from the document: its context rides
            // the document's OWN row for the same finding, ONCE. What
            // else the document says on that line is the document's
            // business (the `parse` shape reports two NML0002 rows and
            // a type mismatch there) — the fold is by RANGE and code.
            assert_eq!(folded.len(), 1, "{tag}: one folded row: {report}");
            let own = folded[0];
            assert_ne!(own["code"], json!("NML2088"), "{tag}: folded: {report}");
            assert_eq!(own["range"]["start"]["line"], json!(line), "{tag}: {own}");
            assert!(
                !codes_of(&report).iter().any(|c| c == "NML2088"),
                "{tag}: {report}"
            );
        } else {
            // A rule reported under its OWN code (NML2081) wrapped
            // nothing: there is no twin, and nothing folds.
            assert!(folded.is_empty(), "{tag}: nothing to fold: {report}");
            let own = rows
                .iter()
                .find(|d| d["range"]["start"]["line"] == json!(line))
                .unwrap_or_else(|| panic!("{tag}: a row at the finding: {report}"));
            assert_eq!(own["code"], json!(code), "{tag}: {own}");
            assert!(own["relatedInformation"].is_null(), "{tag}: {own}");
        }
    }
    // Through the overlay: the disk holds a clean manifest, the buffer the
    // broken one — the rows follow the buffer.
    let base = temp_dir("unloadable-overlay");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, manifest, manifest_text, tenant, text) = grant_workspace(&base, "");
    let broken = manifest_text.replace(
        "        strict = true\n",
        "        strict = true\n        laters:\n            allowRefs:\n                - \"tenants/**\"\n",
    );
    assert_ne!(broken, manifest_text);
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&manifest, &broken).await;
    let own = report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .find(|d| carries_load_note(d))
        .unwrap_or_else(|| panic!("the folded row on the buffer: {report}"));
    assert_eq!(own["range"]["start"]["line"], json!(15), "{own}");
    assert!(
        !codes_of(&report).iter().any(|c| c == "NML2088"),
        "{report}"
    );
    let report = harness.open(&tenant, &text).await;
    assert_eq!(codes_of(&report), ["NML2088"], "{report}");
    assert_eq!(
        report["diagnostics"][0]["relatedInformation"][0]["location"]["range"]["start"]["line"],
        json!(15),
        "{report}"
    );
    // Past the manifest cap: unreadable, no location — the top, alone.
    let base = temp_dir("unloadable-cap");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let (ws, manifest, manifest_text, tenant, text) = grant_workspace(&base, "");
    let padding = "// ".to_string() + &"x".repeat(nml_validate::fs::MAX_MANIFEST_BYTES) + "\n";
    fs::write(&manifest, format!("{manifest_text}{padding}")).expect("oversized manifest");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&tenant, &text).await;
    assert_eq!(codes_of(&report), ["NML2088"], "{report}");
    let row = &report["diagnostics"][0];
    assert_eq!(
        row["range"]["start"],
        json!({ "line": 0, "character": 0 }),
        "{row}"
    );
    assert!(
        row["relatedInformation"].is_null(),
        "no location to relate: {row}"
    );
}

/// A document outside every folder whose universe the kernel derives with
/// NO `.git` fence (`derivedTargetDir`) is said as a WARNING log line
/// carrying the folder advice — the kernel's `needs_disclosure` verdict,
/// as the CLI's root note discloses it.
#[tokio::test]
async fn a_no_fence_derivation_is_said_with_the_folder_advice() {
    let base = temp_dir("no-fence-said");
    if base.ancestors().any(|d| d.join(".git").exists()) {
        return; // a checkout above the temp dir: the fence would be its own
    }
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let loose = base.join("loose");
    fs::create_dir_all(&loose).expect("create loose");
    let x = loose.join("x.nml");
    let text = "thing t:\n    v = 1\n";
    fs::write(&x, text).expect("write x");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_folderless().await;
    harness.open(&x, text).await;
    loop {
        let said = harness
            .next_from_client("window/logMessage", FRAME_TIMEOUT)
            .await;
        let line = said["message"].as_str().expect("message");
        if !line.starts_with("derived a workspace root at `") {
            continue;
        }
        assert_eq!(said["type"], json!(2), "a warning: {said}");
        assert!(
            line.contains("(derivedTargetDir")
                && line.ends_with("— open a workspace folder to fix the universe"),
            "{line}"
        );
        break;
    }
}

/// NML2093 in the editor: the row at the later entry, with
/// `relatedInformation` at the first, in the same document — once (the
/// parse band's row: the one emission, beside every parse).
#[tokio::test]
async fn duplicate_entry_carries_related_information_in_the_document() {
    let base = temp_dir("dup-entry");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let app = ws.join("t.nml");
    let text = "model thing:\n    v string\n\nthing t:\n    v = \"x\"\n    v = \"y\"\n";
    fs::write(&app, text).expect("write");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&app, text).await;
    let diags = report["diagnostics"].as_array().expect("diagnostics");
    let rows: Vec<&Value> = diags
        .iter()
        .filter(|d| d["code"] == json!("NML2093"))
        .collect();
    assert_eq!(rows.len(), 1, "one row, never a twin: {report}");
    let d = rows[0];
    assert_eq!(
        d["range"]["start"],
        json!({"line": 5, "character": 4}),
        "{d}"
    );
    assert_eq!(d["range"]["end"], json!({"line": 5, "character": 5}), "{d}");
    assert_eq!(
        d["message"],
        json!("duplicate entry 'v' — a body declares each name once"),
        "{d}"
    );
    let related = &d["relatedInformation"][0];
    assert!(
        related["location"]["uri"]
            .as_str()
            .is_some_and(|u| u.ends_with("/t.nml")),
        "{d}"
    );
    assert_eq!(
        related["location"]["range"]["start"],
        json!({"line": 4, "character": 4}),
        "{d}"
    );
    assert_eq!(related["message"], json!("'v' first declared here"), "{d}");
}

/// RFC 0026 decision 3: a failed manifest's did-you-mean rides the
/// NML2088 row on a GOVERNED file, naming the manifest as its file
/// (`data.suggestions[0].source`), and the code action edits the
/// manifest from there — titled with the file it changes; on the
/// manifest document itself the row's remedy and the document's own
/// NML2001 finding offer ONE quick fix (twins collapse), applied to the
/// buffer's text.
#[tokio::test]
async fn a_failed_manifests_did_you_mean_is_offered_on_the_manifest_from_every_document() {
    let base = temp_dir("did-you-mean-manifest");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu")).expect("create workspace");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/manifest-rules/did-you-mean");
    for f in [
        "demo.package.nml",
        "core.model.nml",
        "tenants/cu/plain.flow.nml",
    ] {
        fs::copy(fixture.join(f), ws.join(f)).expect(f);
    }
    let manifest = ws.join("demo.package.nml");
    let manifest_text = fs::read_to_string(&manifest).expect("manifest");
    let tenant = ws.join("tenants/cu/plain.flow.nml");
    let tenant_text = fs::read_to_string(&tenant).expect("tenant");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_as_vscode(&ws).await;
    let report = harness.open(&tenant, &tenant_text).await;
    assert_eq!(codes_of(&report), ["NML2088"], "{report}");
    let diag = report["diagnostics"][0].clone();
    assert!(
        diag["message"]
            .as_str()
            .is_some_and(|m| m.ends_with("(did you mean \"version\"?)")),
        "the hint rides the sentence: {diag}"
    );
    let suggestion = &diag["data"]["suggestions"][0];
    assert_eq!(suggestion["kind"], json!("didYouMean"), "{diag}");
    assert_eq!(suggestion["source"], json!("demo.package.nml"), "{diag}");
    assert_eq!(suggestion["replacement"], json!("version"), "{diag}");
    let actions = one_code_action(&mut harness, &tenant, diag.clone()).await;
    let actions = actions.as_array().expect("actions");
    let fixes: Vec<&Value> = actions
        .iter()
        .filter(|a| a["title"] == json!("Replace with \"version\" in demo.package.nml"))
        .collect();
    assert_eq!(fixes.len(), 1, "{actions:?}");
    let fix = fixes[0];
    assert_eq!(fix["kind"], json!("quickfix"), "{fix}");
    let change = &fix["edit"]["documentChanges"][0];
    assert_eq!(
        change["textDocument"]["uri"],
        json!(file_uri(&manifest)),
        "the edit lands in the manifest: {fix}"
    );
    assert_eq!(change["edits"][0]["range"], range(1, 4, 10), "{fix}");
    assert_eq!(change["edits"][0]["newText"], json!("version"), "{fix}");
    // The manifest document: the document's own finding carries the
    // edit, the universe's row folded into it (decision 6) — one
    // action, on the buffer.
    let report = harness.open(&manifest, &manifest_text).await;
    let diags = report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .clone();
    let own = diags
        .iter()
        .find(|d| d["code"] == json!("NML2001"))
        .unwrap_or_else(|| panic!("{report}"));
    assert!(
        carries_load_note(own),
        "the folded wrapper's context: {report}"
    );
    assert!(
        !diags.iter().any(|d| d["code"] == json!("NML2088")),
        "the twin is folded, not shown: {report}"
    );
    let actions = harness
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": file_uri(&manifest) },
                "range": range(1, 4, 10),
                "context": { "diagnostics": diags },
            }),
        )
        .await;
    let actions = actions.as_array().expect("actions");
    let fixes: Vec<&Value> = actions
        .iter()
        .filter(|a| a["title"] == json!("Replace with \"version\""))
        .collect();
    assert_eq!(fixes.len(), 1, "twins collapse to one: {actions:?}");
    let change = &fixes[0]["edit"]["documentChanges"][0];
    assert_eq!(
        change["textDocument"]["uri"],
        json!(file_uri(&manifest)),
        "{fixes:?}"
    );
    assert_eq!(change["edits"][0]["newText"], json!("version"), "{fixes:?}");
}

/// RFC 0026 decision 2: a loader rule of the manifest's own shape (a
/// second `[]validator` array, NML2094) sits on the MANIFEST document at
/// the later keyword with the first as related information, as the CLI
/// prints it — the editor shows what it showed (the code is the wire's
/// `cause`, which the editor does not carry); a file the manifest would
/// govern gets NML2088 alone, naming the rule's sentence.
#[tokio::test]
async fn a_repeated_declaration_is_located_on_the_manifest_document() {
    let base = temp_dir("repeated-declaration-manifest");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu")).expect("create workspace");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/manifest-rules/repeated");
    for f in [
        "demo.package.nml",
        "core.model.nml",
        "tenants/cu/plain.flow.nml",
    ] {
        fs::copy(fixture.join(f), ws.join(f)).expect(f);
    }
    let manifest = ws.join("demo.package.nml");
    let manifest_text = fs::read_to_string(&manifest).expect("manifest");
    let tenant = ws.join("tenants/cu/plain.flow.nml");
    let tenant_text = fs::read_to_string(&tenant).expect("tenant");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&manifest, &manifest_text).await;
    let diags = report["diagnostics"].as_array().expect("diagnostics");
    let row = diags
        .iter()
        .find(|d| d["code"] == json!("NML2088"))
        .unwrap_or_else(|| panic!("{report}"));
    assert_eq!(
        row["range"]["start"],
        json!({"line": 15, "character": 2}),
        "at the later `[]validator` keyword: {row}"
    );
    assert!(
        row["message"]
            .as_str()
            .is_some_and(|m| m.contains("`[]validator` is declared twice")),
        "{row}"
    );
    let related = &row["relatedInformation"][0];
    assert!(
        related["location"]["uri"]
            .as_str()
            .is_some_and(|u| u.ends_with("/demo.package.nml")),
        "{row}"
    );
    assert_eq!(
        related["location"]["range"]["start"],
        json!({"line": 8, "character": 2}),
        "the first declaration: {row}"
    );
    let report = harness.open(&tenant, &tenant_text).await;
    let codes: Vec<&str> = report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .map(|d| d["code"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(codes, ["NML2088"], "{report}");
    assert!(
        report["diagnostics"][0]["message"].as_str().is_some_and(
            |m| m.contains("manifest failed to load: `[]validator` is declared twice")
        ),
        "{report}"
    );
}

/// The manifest document, through the overlay: the located row with its
/// note under the builtin meta package, the universe's NML2088 folded
/// into it as the load note (RFC 0026 decision 6); a file the manifest
/// would govern gets NML2088 alone, naming the entry.
#[tokio::test]
async fn a_manifest_naming_an_entry_twice_is_located_on_the_manifest_document() {
    let base = temp_dir("dup-entry-manifest");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu")).expect("create workspace");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workspace-dup");
    for f in [
        "demo.package.nml",
        "core.model.nml",
        "tenants/cu/plain.flow.nml",
    ] {
        fs::copy(fixture.join(f), ws.join(f)).expect(f);
    }
    let manifest = ws.join("demo.package.nml");
    let manifest_text = fs::read_to_string(&manifest).expect("manifest");
    let tenant = ws.join("tenants/cu/plain.flow.nml");
    let tenant_text = fs::read_to_string(&tenant).expect("tenant");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&manifest, &manifest_text).await;
    let diags = report["diagnostics"].as_array().expect("diagnostics");
    let rows: Vec<&Value> = diags
        .iter()
        .filter(|d| d["code"] == json!("NML2093"))
        .collect();
    assert_eq!(rows.len(), 1, "{report}");
    let row = rows[0];
    assert_eq!(
        row["range"]["start"],
        json!({"line": 15, "character": 8}),
        "{row}"
    );
    let related = &row["relatedInformation"][0];
    assert!(
        related["location"]["uri"]
            .as_str()
            .is_some_and(|u| u.ends_with("/demo.package.nml")),
        "{row}"
    );
    assert_eq!(
        related["location"]["range"]["start"],
        json!({"line": 10, "character": 8}),
        "{row}"
    );
    assert!(
        !diags.iter().any(|d| d["code"] == json!("NML2088")),
        "the twin is folded into the finding: {report}"
    );
    assert!(carries_load_note(row), "the load named on the row: {row}");
    let report = harness.open(&tenant, &tenant_text).await;
    let codes: Vec<&str> = report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .map(|d| d["code"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(codes, ["NML2088"], "{report}");
    let row = &report["diagnostics"][0];
    let message = row["message"].as_str().expect("message");
    assert!(
        message.contains("manifest failed to load: duplicate entry 'files'")
            && !message.contains("16:9"),
        "the sentence names no line — the location is the row's own: {report}"
    );
    // The first `files` rides the row as related information, located
    // in the manifest — the editor jumps there from the tenant file.
    let related = &row["relatedInformation"][0];
    assert!(
        related["location"]["uri"]
            .as_str()
            .is_some_and(|u| u.ends_with("/demo.package.nml")),
        "{row}"
    );
    assert_eq!(
        related["location"]["range"]["start"],
        json!({"line": 10, "character": 8}),
        "{row}"
    );
    assert_eq!(
        related["message"],
        json!("'files' first declared here"),
        "{row}"
    );
    // The row's own location — the later `files`, in the manifest — rides
    // the tenant document too, as a second related location (a universe
    // row spanned in ANOTHER file keeps its place there, a jump away).
    let located = &row["relatedInformation"][1];
    assert!(
        located["location"]["uri"]
            .as_str()
            .is_some_and(|u| u.ends_with("/demo.package.nml")),
        "{row}"
    );
    assert_eq!(
        located["location"]["range"]["start"],
        json!({"line": 15, "character": 8}),
        "{row}"
    );
    assert_eq!(located["message"], json!("the manifest's finding"), "{row}");
}

/// One row whatever the document's schema state: a schema-less file (no
/// model, no package) carries NML2093 in the parse band alone — the rule
/// is the parser's, so no validator pass and no deduplicating sink is
/// needed for it to appear once — the row follows the buffer through the
/// overlay, and the hover explains the code (RFC 0010 tier 1).
#[tokio::test]
async fn a_duplicate_entry_is_one_row_in_a_schema_less_document_and_follows_the_buffer() {
    let base = temp_dir("dup-entry-bare");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let app = ws.join("bare.nml");
    let text = "thing t:\n    v = \"x\"\n    v = \"y\"\n";
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let rows = |report: &Value| -> Vec<Value> {
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .filter(|d| d["code"] == json!("NML2093"))
            .cloned()
            .collect()
    };
    let report = harness.open(&app, text).await;
    let first = rows(&report);
    assert_eq!(first.len(), 1, "one row, never a twin: {report}");
    assert_eq!(
        first[0]["range"]["start"],
        json!({"line": 2, "character": 4}),
        "{report}"
    );
    assert_eq!(
        first[0]["relatedInformation"][0]["location"]["range"]["start"],
        json!({"line": 1, "character": 4}),
        "{report}"
    );
    assert!(
        first[0]["data"]["suggestions"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "no suggestion: which entry is meant is unknowable: {report}"
    );
    let hover = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(&app) },
                "position": { "line": 2, "character": 4 },
            }),
        )
        .await;
    assert!(
        hover["contents"]["value"]
            .as_str()
            .is_some_and(|v| v.contains("**NML2093**")),
        "the hover explains the code: {hover}"
    );
    // The buffer moves on (a line above the pair): the row follows.
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&app), "version": 2 },
                "contentChanges": [{ "text": "thing t:\n    w = 1\n    v = \"x\"\n    v = \"y\"\n" }],
            }),
        )
        .await;
    let report = harness.diagnostics(&file_uri(&app)).await;
    let moved = rows(&report);
    assert_eq!(moved.len(), 1, "{report}");
    assert_eq!(
        moved[0]["range"]["start"],
        json!({"line": 3, "character": 4}),
        "{report}"
    );
    assert_eq!(
        moved[0]["relatedInformation"][0]["location"]["range"]["start"],
        json!({"line": 2, "character": 4}),
        "{report}"
    );
}

/// A `.model.nml` buffer defining a field twice: one row at the later
/// definition — the buffer's schema passes re-derive the parse band's
/// findings (fed the same extraction) and the exact twin is suppressed,
/// as for every parse finding of a model file.
#[tokio::test]
async fn a_model_source_defining_a_field_twice_is_one_row() {
    let base = temp_dir("dup-entry-model");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let model = ws.join("thing.model.nml");
    let text = "model thing:\n    v string\n    v number\n";
    fs::write(&model, text).expect("write model");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&model, text).await;
    let rows: Vec<&Value> = report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter(|d| d["code"] == json!("NML2093"))
        .collect();
    assert_eq!(rows.len(), 1, "one row on a model source: {report}");
    assert_eq!(
        rows[0]["range"]["start"],
        json!({"line": 2, "character": 4}),
        "{report}"
    );
    assert_eq!(
        rows[0]["relatedInformation"][0]["location"]["range"]["start"],
        json!({"line": 1, "character": 4}),
        "{report}"
    );
}

/// RFC 0026 decision 6: a `*.schema.nml` buffer is a schema source as the
/// kernel admits it (`is_schema_source_name`, both spellings) — the
/// editor's schema-source pass opens for it, so the kernel's NML5000
/// verdict and the builtin's hover answer there exactly as on a
/// `*.model.nml` (the demo package's declared source, spelled
/// `core.schema.nml`); the editor used to open the pass for `.model.nml`
/// URIs only, so `nml check` judged the file and the editor did not.
#[tokio::test]
async fn a_schema_nml_buffer_is_a_schema_source_as_the_kernel_admits_it() {
    let base = temp_dir("schema-nml-admission");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let text = "model core:\n    action string #sealed\n    rateLimit number #lvie\n";
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    // The demo package with its declared source spelled `.schema.nml`.
    fs::write(
        ws.join("demo.package.nml"),
        DEMO_MANIFEST_WITH_DIRECTIVES.replace("core.model.nml", "core.schema.nml"),
    )
    .expect("write manifest");
    let schema = ws.join("core.schema.nml");
    fs::write(&schema, text).expect("write schema source");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&schema, text).await;
    let rows: Vec<&Value> = report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter(|d| d["code"] == json!("NML5000"))
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "the kernel's verdict on a .schema.nml: {report}"
    );
    assert_eq!(
        rows[0]["message"],
        json!("unknown directive '#lvie' (package 'demo') (did you mean \"#live\"?)"),
        "{report}"
    );
    assert_eq!(
        rows[0]["range"]["start"],
        json!({"line": 2, "character": 21}),
        "{report}"
    );
    assert!(
        !report.to_string().contains("#sealed'"),
        "a language directive is never unknown: {report}"
    );
    let hover = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(&schema) },
                "position": { "line": 1, "character": 20 },
            }),
        )
        .await;
    assert!(
        hover["contents"]["value"]
            .as_str()
            .is_some_and(|v| v.starts_with("**#sealed** (no argument)")),
        "the builtin's entry on a .schema.nml: {hover}"
    );
}

/// RFC 0019 §Editor surface, built: the language's merge-policy directives
/// are merged into every vocabulary outcome at the three consumption sites.
/// Under the demo package's declared vocabulary, `#sealed` is never unknown,
/// a near-miss of a declared name is the kernel's NML5000 — the CLI's exact
/// sentence (`a_schema_source_is_judged_under_the_kernels_directive_vocabulary_on_every_verb`
/// pins the same literal) — hovering `#sealed` renders the builtin's entry,
/// and completion after `#` offers the builtins before the declared names.
#[tokio::test]
async fn the_languages_directives_are_known_under_a_declared_vocabulary_at_every_site() {
    let base = temp_dir("builtin-directives");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let text = "model core:\n    action string #sealed\n    rateLimit number #lvie\n";
    let (ws, model) = directive_workspace(&base, text);
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&model, text).await;
    let rows: Vec<&Value> = report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter(|d| d["code"] == json!("NML5000"))
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "one unknown directive, never `#sealed`: {report}"
    );
    assert_eq!(
        rows[0]["message"],
        json!("unknown directive '#lvie' (package 'demo') (did you mean \"#live\"?)"),
        "{report}"
    );
    assert_eq!(
        rows[0]["range"]["start"],
        json!({"line": 2, "character": 21}),
        "{report}"
    );
    assert!(
        !report.to_string().contains("#sealed'"),
        "a language directive is never unknown: {report}"
    );
    let hover = harness
        .request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(&model) },
                "position": { "line": 1, "character": 20 },
            }),
        )
        .await;
    assert!(
        hover["contents"]["value"].as_str().is_some_and(
            |v| v.starts_with("**#sealed** (no argument) — Write-once from the bottom")
        ),
        "the builtin's entry: {hover}"
    );
    let typing =
        "model core:\n    action string #sealed\n    rateLimit number #lvie\n    x string #\n";
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&model), "version": 2 },
                "contentChanges": [{ "text": typing }],
            }),
        )
        .await;
    let result = harness
        .request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": file_uri(&model) },
                "position": { "line": 3, "character": 14 },
            }),
        )
        .await;
    let labels: Vec<&str> = result
        .as_array()
        .expect("completion item array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert_eq!(
        labels,
        [
            "sealed", "identity", "append", "overlay", "live", "restart", "key"
        ],
        "{result}"
    );
}

/// A repeated top-level name is the parse band's ONE NML1000 row at the
/// later name, with the first declaration as `relatedInformation` — in a
/// schema-less document, through the overlay (a line added above moves the
/// row and its note together), and in a manifest buffer with two `package`
/// blocks.
#[tokio::test]
async fn duplicate_names_ride_the_parse_band_with_their_notes() {
    let base = temp_dir("dup-decl-parse-band");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let rows = |report: &Value| -> Vec<Value> {
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .filter(|d| d["code"] == json!("NML1000"))
            .cloned()
            .collect()
    };
    let expect_one = |report: &Value, at: (u64, u64), first: (u64, u64), name: &str| {
        let rows = rows(report);
        assert_eq!(rows.len(), 1, "one row, never a twin: {report}");
        assert!(
            rows[0]["message"]
                .as_str()
                .is_some_and(|m| m.starts_with(&format!("duplicate declaration '{name}'"))),
            "{report}"
        );
        assert_eq!(
            rows[0]["range"]["start"],
            json!({"line": at.0, "character": at.1}),
            "at the later NAME: {report}"
        );
        let related = &rows[0]["relatedInformation"][0];
        assert_eq!(
            related["location"]["range"]["start"],
            json!({"line": first.0, "character": first.1}),
            "the first declaration is the note: {report}"
        );
        assert_eq!(
            related["message"],
            json!(format!("'{name}' first declared here")),
            "{report}"
        );
    };
    let app = ws.join("bare.nml");
    let text = "model api:\n    v string\n\nmodel api:\n    w string\n";
    let report = harness.open(&app, text).await;
    expect_one(&report, (3, 6), (0, 6), "api");
    // The buffer moves on (a line above the pair): the row and its note follow.
    harness
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(&app), "version": 2 },
                "contentChanges": [{ "text": "const N = 1\n\nmodel api:\n    v string\n\nmodel api:\n    w string\n" }],
            }),
        )
        .await;
    let report = harness.diagnostics(&file_uri(&app)).await;
    expect_one(&report, (5, 6), (2, 6), "api");
    // A manifest buffer with two `package` blocks: the same parse-band row.
    let manifest = ws.join("demo.package.nml");
    let text = "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    - core:\n        file = \"core.model.nml\"\n\npackage demo:\n    version = \"0.2.0\"\n";
    let report = harness.open(&manifest, text).await;
    expect_one(&report, (8, 8), (0, 8), "demo");
}

/// RFC 0026 decision 3 (the composition's NML2104): a `[]string` element
/// spelled as a template (`"tenants/cu/{{x}}"`) is a LOADER rule — the
/// manifest's own pass reports nothing there, the text being legal — so
/// the universe's NML2088 row has no twin to fold into (decision 6) and
/// is published as itself, AT the element, on the manifest's own
/// document; a file the manifest would govern gets the same row at its
/// top, the element a jump away.
/// RFC 0026 decision 1: the loader reads the one schema-source admission
/// too, so the file the editor could not see is refused where it is
/// DECLARED. Before the rule the manifest loaded, `nml check` judged
/// `core.nml`'s directives and this buffer got zero rows and no hover;
/// now the manifest document carries the row at the `file` value and the
/// mis-spelled source carries the universe's row like any governed file.
#[tokio::test]
async fn a_declared_schema_source_not_spelled_as_one_is_refused_on_both_documents() {
    let base = temp_dir("schema-source-name");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu")).expect("create workspace");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/manifest-rules/schema-source-name");
    for f in ["demo.package.nml", "core.nml", "tenants/cu/plain.flow.nml"] {
        fs::copy(fixture.join(f), ws.join(f)).expect(f);
    }
    let manifest = ws.join("demo.package.nml");
    let manifest_text = fs::read_to_string(&manifest).expect("manifest");
    let line = manifest_text
        .lines()
        .position(|l| l.contains("core.nml"))
        .expect("the `file` value") as u64;
    let character = manifest_text
        .lines()
        .nth(line as usize)
        .and_then(|l| l.find('"'))
        .expect("the value's opening quote") as u64;

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;

    let report = harness.open(&manifest, &manifest_text).await;
    assert_eq!(codes_of(&report), ["NML2088"], "{report}");
    let row = &report["diagnostics"][0];
    assert_eq!(
        row["range"]["start"],
        json!({ "line": line, "character": character }),
        "at the `file` value: {row}"
    );
    assert!(
        row["message"]
            .as_str()
            .is_some_and(|m| m.contains("is not spelled as a schema source")),
        "{row}"
    );

    // The mis-spelled source: the universe's row, where it used to be silent.
    let source = ws.join("core.nml");
    let source_text = fs::read_to_string(&source).expect("source");
    let report = harness.open(&source, &source_text).await;
    assert_eq!(codes_of(&report), ["NML2088"], "{report}");
}

#[tokio::test]
async fn a_manifest_list_element_spelled_as_a_template_is_one_row_at_the_element() {
    let base = temp_dir("template-in-list-manifest");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu")).expect("create workspace");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/manifest-rules/template-in-list");
    for f in [
        "demo.package.nml",
        "core.model.nml",
        "tenants/cu/plain.flow.nml",
    ] {
        fs::copy(fixture.join(f), ws.join(f)).expect(f);
    }
    let manifest = ws.join("demo.package.nml");
    let manifest_text = fs::read_to_string(&manifest).expect("manifest");
    let tenant = ws.join("tenants/cu/plain.flow.nml");
    let tenant_text = fs::read_to_string(&tenant).expect("tenant");
    // The element the loader refuses, in the manifest's own text.
    let line = manifest_text
        .lines()
        .position(|l| l.contains("{{x}}"))
        .expect("the template element") as u64;
    let character = manifest_text
        .lines()
        .nth(line as usize)
        .and_then(|l| l.find('"'))
        .expect("the element's opening quote") as u64;

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;

    // The manifest document: ONE row, at the element, under NML2088 — the
    // rule is not one whose code IS the verdict (decision 1: NML2081 and
    // NML2082 alone ride their own), and nothing of the document's own is
    // there to fold it into.
    let report = harness.open(&manifest, &manifest_text).await;
    assert_eq!(codes_of(&report), ["NML2088"], "{report}");
    let row = &report["diagnostics"][0];
    assert_eq!(
        row["range"]["start"],
        json!({ "line": line, "character": character }),
        "at the element: {row}"
    );
    assert!(
        row["message"]
            .as_str()
            .is_some_and(|m| m.contains("`denyRefs` holds a template string (`{{…}}`)")),
        "{row}"
    );
    assert!(
        !carries_load_note(row),
        "nothing to fold into, so no load note: {row}"
    );
    assert!(row["relatedInformation"].is_null(), "{row}");

    // A file it would govern: the same row at the top, the element a jump
    // away — never a second copy of the rule.
    let report = harness.open(&tenant, &tenant_text).await;
    assert_eq!(codes_of(&report), ["NML2088"], "{report}");
    let row = &report["diagnostics"][0];
    assert_eq!(
        row["range"]["start"],
        json!({ "line": 0, "character": 0 }),
        "{row}"
    );
    assert!(
        row["message"]
            .as_str()
            .is_some_and(|m| m.contains("`denyRefs` holds a template string (`{{…}}`)")),
        "{row}"
    );
    let related = &row["relatedInformation"][0];
    assert!(
        related["location"]["uri"]
            .as_str()
            .is_some_and(|u| u.ends_with("/demo.package.nml")),
        "{row}"
    );
    assert_eq!(
        related["location"]["range"]["start"],
        json!({ "line": line, "character": character }),
        "{row}"
    );
}

/// Two root-level packages that could each cover an undeclared schema
/// source (neither declares it) are an ambiguity the editor SAYS, as
/// `nml check` says it: one INFORMATION row at (0,0) in the kernel's one
/// sentence naming both packages, the directives judged under no
/// vocabulary (no NML5000) — never a silent pass. With one package the
/// same buffer is NML5000 beside the NML5003 sibling note.
#[tokio::test]
async fn an_undeclared_schema_source_two_packages_could_cover_carries_the_ambiguity_note() {
    let base = temp_dir("ambiguous-coverage");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("tenants/cu")).expect("create workspace");
    fs::create_dir_all(ws.join("shared")).expect("create workspace");
    fs::write(ws.join("core.model.nml"), "model thing:\n    v string\n").expect("core");
    fs::write(
        ws.join("tenants/cu/plain.flow.nml"),
        "thing a:\n    v = \"x\"\n",
    )
    .expect("tenant");
    fs::write(ws.join("shared/s.flow.nml"), "thing a:\n    v = \"x\"\n").expect("shared");
    let manifest = |name: &str, glob: &str| {
        format!(
            "package {name}:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    \
             - core:\n        file = \"core.model.nml\"\n\n[]validator validators:\n    - flows:\n        \
             files:\n            - \"{glob}\"\n        schemas:\n            - core\n"
        )
    };
    fs::write(
        ws.join("demo.package.nml"),
        manifest("demo", "tenants/**/*.flow.nml"),
    )
    .expect("demo");
    fs::write(
        ws.join("other.package.nml"),
        manifest("other", "shared/**/*.flow.nml"),
    )
    .expect("other");
    let stray = ws.join("stray.model.nml");
    let text = "model stray:\n    v string #bogus\n";
    fs::write(&stray, text).expect("stray");
    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize(&ws).await;
    let report = harness.open(&stray, text).await;
    let rows = report["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(rows.len(), 1, "one note, no verdict: {report}");
    assert_eq!(
        rows[0]["severity"],
        json!(3),
        "an information row: {report}"
    );
    assert_eq!(
        rows[0]["range"]["start"],
        json!({"line": 0, "character": 0}),
        "{report}"
    );
    assert_eq!(
        rows[0]["message"],
        json!(
            "package coverage ambiguous: 2 packages could cover this schema source (demo, other) \
             and none declares it — judged under no vocabulary (every directive accepted); \
             declare it in one package's []schema"
        ),
        "{report}"
    );
    assert!(rows[0]["code"].is_null(), "{report}");
    // One package: covered and judged.
    fs::remove_file(ws.join("other.package.nml")).expect("remove");
    harness
        .notify(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": [{ "uri": file_uri(&ws.join("other.package.nml")), "type": 3 }] }),
        )
        .await;
    let report = harness.diagnostics(&file_uri(&stray)).await;
    assert_eq!(codes_of(&report), ["NML5000", "NML5003"], "{report}");
}

/// The VS Code E2E's first assertion, at the native tier: the extension's
/// `fixtures/ws` layout — `app.nml` beside `core.model.nml`, no manifest —
/// with ONLY the instance opened, the model on disk. The open-mode
/// workspace registry indexes the sibling model and the instance's
/// string-for-number diagnoses; `nml/schemaInfo` answers `bound: false`
/// (no package governs it) with no note.
#[tokio::test]
async fn the_e2e_fixture_diagnoses_with_only_the_instance_open() {
    let base = temp_dir("e2e-fixture-native");
    let store_base = base.join("store");
    fs::create_dir_all(&store_base).expect("create store dir");
    let ws = base.join("ws");
    fs::create_dir_all(&ws).expect("create workspace");
    fs::write(
        ws.join("core.model.nml"),
        "model server:\n    port number\n",
    )
    .expect("write model");
    let app = ws.join("app.nml");
    let app_text = "server main:\n    port = \"x\"\n";
    fs::write(&app, app_text).expect("write app");

    let mut harness = Harness::new(Store::at(&store_base));
    harness.initialize_as_vscode(&ws).await;
    let published = harness.open(&app, app_text).await;
    let diags = published["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .clone();
    assert!(
        diags.iter().any(|d| d["code"] == json!("NML2008")),
        "string-for-number must diagnose with the model unopened: {published}"
    );
    let info = harness
        .request("nml/schemaInfo", json!({ "uri": file_uri(&app) }))
        .await;
    assert_eq!(info["bound"], json!(false), "{info}");
    assert_eq!(info["notes"], json!([]), "{info}");
}
