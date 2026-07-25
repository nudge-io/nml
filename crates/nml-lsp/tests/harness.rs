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
use serde_json::{json, Value};
use tower::{Service, ServiceExt};
use tower_lsp::jsonrpc::{Request, Response};
use tower_lsp::lsp_types::Url;
use tower_lsp::{ClientSocket, LspService};

use nml_lsp::server::NmlLanguageServer;
use nml_validate::store::Store;
use nml_validate::test_support::{demo_package, publish_demo, DEMO_MANIFEST_WITH_DIRECTIVES};

/// Generous slack for a server→client notification. Store-health
/// `window/logMessage`s are emitted during the diagnostic-pull handler
/// (`drain_store_events`), so after a pull they are already queued; this
/// bound only guards against a hang, never a busy-wait.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// The in-process server plus both directions of its wire.
struct Harness {
    service: LspService<NmlLanguageServer>,
    socket: ClientSocket,
    /// Server→client notifications drained off the socket but not yet
    /// consumed by an assertion, in arrival order. Server→client *requests*
    /// never land here — they are auto-acknowledged in [`Self::route`].
    inbox: VecDeque<Request>,
    next_id: i64,
}

impl Harness {
    /// Build the service through the same `nml_lsp::build_service` owner the
    /// binary uses — so `nml/schemaInfo` (and every future custom method) is
    /// exercised through the real JSON-RPC route — but with the resolver's
    /// store injected.
    fn new(store: Store) -> Self {
        let (service, socket) =
            nml_lsp::build_service(|client| NmlLanguageServer::with_store(client, Some(store)));
        Self {
            service,
            socket,
            inbox: VecDeque::new(),
            next_id: 0,
        }
    }

    /// Build a *provider* service (RFC 0035 in-binary channel) — the `nudge
    /// lsp` wiring: the tool's package injected in-process, plus a store (here
    /// a tempdir, so coverage must come from the injected package, not the
    /// cache). Exercises `NmlLanguageServer::with_provider` through the same
    /// service builder the tool binary uses.
    fn new_provider(package: nml_validate::package::SchemaPackage, store: Store) -> Self {
        let (service, socket) = nml_lsp::build_service(move |client| {
            NmlLanguageServer::with_provider(client, package, Some(store))
        });
        Self {
            service,
            socket,
            inbox: VecDeque::new(),
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
            Some(id) => self
                .socket
                .send(Response::from_ok(id, Value::Null))
                .await
                .expect("client socket closed"),
            None => self.inbox.push_back(frame),
        }
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

/// Fresh scratch dir per test, canonicalized because the server
/// canonicalizes workspace roots and document paths (macOS `/var` →
/// `/private/var`); the URIs the test sends must agree byte-for-byte with
/// the URIs the server publishes back.
fn temp_dir(tag: &str) -> PathBuf {
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
    dunce::canonicalize(&dir).expect("canonicalize scratch dir")
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
                message.contains("falling back to basic validation"),
                "fallback wording missing from: {message}"
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
    let model_text =
        "model modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel host:\n    slot (modelA | modelB)?\n";
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
    let model_text =
        "model modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel host:\n    slots [](modelA | modelB)?\n";
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
        ["live", "restart", "key"],
        "vocabulary only, declaration order: {result}"
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

/// TEST D — walk-cap honesty end-to-end: a model file whose root-coverage
/// walk hits the entry cap (2048; the filler wall guarantees it fires before
/// the only glob-bound file is reachable) gets ONE info diagnostic naming
/// the candidate package and the remedy — instead of silently losing its
/// directive vocabulary.
#[tokio::test]
async fn walk_cap_surfaces_undetermined_coverage_diagnostic() {
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
    let stray_text = "model stray:\n    name string\n";
    fs::write(&stray, stray_text).expect("write stray model");
    // The only glob-bound file sits BELOW the filler wall: the walk exhausts
    // its cap on the root's >2048 entries before it can descend.
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
    let undetermined: Vec<&Value> = diags
        .iter()
        .filter(|d| {
            d["message"]
                .as_str()
                .is_some_and(|m| m.contains("package coverage undetermined"))
        })
        .collect();
    assert_eq!(
        undetermined.len(),
        1,
        "exactly ONE undetermined-coverage info diagnostic: {published}"
    );
    let diag = undetermined[0];
    assert_eq!(
        diag["message"],
        json!(
            "package coverage undetermined ('demo'? root exceeds the scan bound); \
             declare this file in the package's []schema to get directive vocabulary"
        ),
        "{diag}"
    );
    // Info severity: honesty, not an error.
    assert_eq!(diag["severity"], json!(3), "{diag}");
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
